//! Node ↔ channel-service glue: a [`ChannelHandler`] backed by the node's world
//! state and mempool.
//!
//! The p2p [`ChannelService`](tron_p2p::service::ChannelService) is deliberately
//! free of consensus/state deps; this adapter supplies the real behavior:
//! - `on_block` runs the H05 intake gate ([`crate::sync::apply_synced_blocks_gated`]
//!   with the live active-witness set) before the block is stored,
//! - `on_transaction` admits to the mempool via [`tron_consensus::mempool::admit_transaction`],
//! - `block_bytes` serves stored blocks for `FetchInvData`,
//! - `head` reports the current chain head.

use prost::Message;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use tron_consensus::mempool::{admit_transaction, Mempool};
use tron_consensus::pbft::{accept_pbft_message, is_finalized, solidified_block, PbftCommit};
use tron_crypto::{address_from_public_key, public_key, sign_digest, SecretKey};
use tron_p2p::service::{ChannelHandle, ChannelHandler};
use tron_state::{blocks::LATEST_BLOCK_NUMBER, props, WorldState};
use tron_storage::KvStore;

/// This node's SR identity for PBFT commit generation: its witness secret key and
/// 21-byte address (T09).
struct WitnessCommitter {
    key: SecretKey,
    address: Vec<u8>,
}

/// A [`ChannelHandler`] over the node's shared world state + mempool.
pub struct NodeChannelHandler<S: KvStore> {
    world: Arc<WorldState<S>>,
    mempool: Arc<Mutex<Mempool>>,
    /// Whether inbound blocks must carry a valid witness signature.
    require_sig: bool,
    /// PBFT confirmations: block number → the distinct SR addresses that have
    /// committed it. Kept bounded (H01) — entries below the solidified block are
    /// pruned as finality advances.
    confirmations: Mutex<HashMap<i64, HashSet<Vec<u8>>>>,
    /// This node's SR identity, present only on witness-configured nodes (T09). When
    /// set, the node signs + broadcasts a PBFT commit for each block it accepts/produces.
    witness: Option<WitnessCommitter>,
    /// Handle to broadcast our own commits. Injected after the `ChannelService` is
    /// built (the handle is created alongside the service, so it can't be passed in at
    /// construction).
    commit_out: OnceLock<ChannelHandle>,
}

impl<S: KvStore + 'static> NodeChannelHandler<S> {
    pub fn new(world: Arc<WorldState<S>>, mempool: Arc<Mutex<Mempool>>, require_sig: bool) -> Self {
        Self {
            world,
            mempool,
            require_sig,
            confirmations: Mutex::new(HashMap::new()),
            witness: None,
            commit_out: OnceLock::new(),
        }
    }

    /// Configure this node as a witness (SR) that generates PBFT commits with `key`.
    /// `None` leaves it a non-witness node (never emits commits).
    pub fn with_witness(mut self, key: Option<SecretKey>) -> Self {
        self.witness = key.map(|k| {
            let address = address_from_public_key(&public_key(&k)).as_bytes().to_vec();
            WitnessCommitter { key: k, address }
        });
        self
    }

    /// Inject the broadcast handle (call once, right after `ChannelService::new`).
    pub fn set_commit_handle(&self, handle: ChannelHandle) {
        let _ = self.commit_out.set(handle);
    }

    /// Generate, self-record, and broadcast this node's PBFT commit for a block it
    /// accepted or produced (T09). No-op unless this node is a witness whose address is
    /// in the active SR set **and** `allow_pbft` is on — so only SRs emit, and nothing
    /// is signed while PBFT is committee-disabled.
    ///
    /// Deviation vs java-tron: the two-phase prepare→commit is collapsed to a single
    /// commit round here; view-change / epoch handling is out of scope.
    pub fn emit_commit(&self, block_num: i64, block_id: [u8; 32]) {
        let Some(w) = self.witness.as_ref() else { return };
        if self.world.get_prop_i64(props::ALLOW_PBFT).unwrap_or(0) != 1 {
            return;
        }
        let active = match self.world.get_active_witnesses() {
            Ok(a) => a,
            Err(_) => return,
        };
        if !active.iter().any(|a| a == &w.address) {
            return; // only active SRs emit commits
        }
        let digest = PbftCommit::digest(block_num, &block_id);
        let sig = match sign_digest(&w.key, &digest) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut signature = [0u8; 65];
        signature[..64].copy_from_slice(&sig.rs);
        signature[64] = 27 + sig.recovery_id;
        let bytes = PbftCommit { block_num, block_id, signature }.encode();
        // Count our own commit through the same accounting (may advance solidified).
        self.on_pbft_commit(&bytes);
        // Broadcast to peers so the network reaches quorum.
        if let Some(handle) = self.commit_out.get() {
            handle.advertise_pbft_commit(bytes);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl<S: KvStore + 'static> ChannelHandler for NodeChannelHandler<S> {
    fn head(&self) -> i64 {
        self.world.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap_or(0)
    }

    fn on_block(&self, block_bytes: &[u8]) -> Option<i64> {
        // Gate against the live active-witness set (CS-JTRON-006/-007): a peer
        // block must be signed by a scheduled producer before it is stored.
        let witnesses = self.world.get_active_witnesses().ok()?;
        let gate = if witnesses.is_empty() { None } else { Some(witnesses.as_slice()) };
        match crate::sync::apply_synced_blocks_gated(
            &self.world,
            &[block_bytes.to_vec()],
            self.require_sig,
            gate,
        ) {
            Ok(applied) if applied >= 1 => {
                // The intake gate already proved this block's producer is an active
                // witness, so if we are an SR we sign + broadcast our commit for it (T09).
                if let Ok(block) = tron_proto::protocol::Block::decode(block_bytes) {
                    if let (Some(raw), Some(id)) = (
                        block.block_header.as_ref().and_then(|h| h.raw_data.as_ref()),
                        tron_chain::block_id_of(&block),
                    ) {
                        self.emit_commit(raw.number, id.0);
                    }
                }
                Some(self.head())
            }
            _ => None,
        }
    }

    fn on_transaction(&self, tx_bytes: &[u8]) {
        if let Ok(tx) = tron_proto::protocol::Transaction::decode(tx_bytes) {
            if let Ok(mut pool) = self.mempool.lock() {
                let _ = admit_transaction(&mut pool, &self.world, &tx, now_ms(), |_, _| Ok(()));
            }
        }
    }

    fn block_bytes(&self, number: i64) -> Option<Vec<u8>> {
        self.world
            .get_block_by_num(number)
            .ok()
            .flatten()
            .map(|b| b.encode_to_vec())
    }

    /// Verify + account for an inbound PBFT commit (T08). Gated on `allow_pbft`
    /// (default off). Decodes the commit, requires the recovered signer to be an
    /// active SR, dedups per (block, SR), records the confirmation, and when a block
    /// reaches quorum advances + persists the solidified block **contiguously**
    /// (never skipping a gap), pruning the confirmation map below the new tip (H01).
    /// Returns `true` only for a newly-accepted commit (so the service re-broadcasts).
    fn on_pbft_commit(&self, commit_bytes: &[u8]) -> bool {
        // Feature gate: drop entirely unless PBFT is committee-enabled (do not decode
        // or cache), so an unprivileged peer cannot grow memory while PBFT is off.
        let allow_pbft = self.world.get_prop_i64(props::ALLOW_PBFT).unwrap_or(0) == 1;
        if !allow_pbft {
            return false;
        }
        let commit = match PbftCommit::decode(commit_bytes) {
            Some(c) => c,
            None => return false,
        };
        // Recover the signer and require active-SR membership.
        let signer = match commit.recover_signer() {
            Some(a) => a.as_bytes().to_vec(),
            None => return false,
        };
        let active = match self.world.get_active_witnesses() {
            Ok(a) => a,
            Err(_) => return false,
        };
        if !active.iter().any(|w| w == &signer) {
            return false;
        }

        let head = self.head();
        let solidified = self.world.get_solidified_block().unwrap_or(0);
        // Window gate (CS-JTRON-004): above solidified, not absurdly future.
        if !accept_pbft_message(allow_pbft, commit.block_num, head, solidified) {
            return false;
        }

        let mut conf = match self.confirmations.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        // Dedup per (block, SR): a repeat commit counts once and is not re-broadcast.
        if !conf.entry(commit.block_num).or_default().insert(signer) {
            return false;
        }

        // Advance finality: highest contiguous block at/below head with quorum.
        if is_finalized(conf[&commit.block_num].len(), active.len()) {
            let counts: HashMap<i64, usize> = conf.iter().map(|(k, v)| (*k, v.len())).collect();
            if let Some(solid) = solidified_block(&counts, head, active.len()) {
                if solid > solidified {
                    let _ = self.world.put_solidified_block(solid);
                    conf.retain(|&block, _| block >= solid); // prune_below_solidified
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tron_consensus::producer::produce_block;
    use tron_crypto::{address_from_public_key, public_key, SecretKey};
    use tron_p2p::service::{ChannelConfig, ChannelService};
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    fn genesis() -> protocol::Block {
        protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw { number: 0, ..Default::default() }),
                witness_signature: vec![],
            }),
            transactions: vec![],
        }
    }

    fn handler(
        world: Arc<WorldState<MemoryStore>>,
    ) -> Arc<NodeChannelHandler<MemoryStore>> {
        Arc::new(NodeChannelHandler::new(world, Arc::new(Mutex::new(Mempool::default())), true))
    }

    async fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..(timeout_ms / 20) {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    /// Two in-process nodes over real TCP: A advertises a produced block; B fetches
    /// it, runs it through the intake gate, applies it, and the heads converge —
    /// entirely over the advertise→inventory→fetch path (no periodic poll).
    #[tokio::test]
    async fn two_nodes_converge_via_block_advertise() {
        let sk = SecretKey::from_slice(&[0x88u8; 32]).unwrap();
        let producer = address_from_public_key(&public_key(&sk));

        // A: genesis + block 1 applied (so A can serve it and is at head 1).
        let a_world = Arc::new(WorldState::new(MemoryStore::new()));
        a_world.put_block(&genesis()).unwrap();
        a_world.put_active_witnesses(&[producer]).unwrap();
        let block1 = produce_block(&genesis(), &sk, 3000, vec![], 30);
        let a_active = a_world.get_active_witnesses().unwrap();
        let applied = crate::sync::apply_synced_blocks_gated(
            &a_world,
            &[block1.encode_to_vec()],
            true,
            Some(&a_active),
        )
        .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(a_world.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 1);

        // B: genesis only, at head 0, knows the active witness set.
        let b_world = Arc::new(WorldState::new(MemoryStore::new()));
        b_world.put_block(&genesis()).unwrap();
        b_world.put_active_witnesses(&[producer]).unwrap();
        assert_eq!(b_world.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 0);

        let cfg = || ChannelConfig { keepalive: Duration::from_millis(50), hello: b"node".to_vec() };
        let (a_svc, a_handle) = ChannelService::new(handler(a_world.clone()), cfg());
        let (b_svc, _b_handle) = ChannelService::new(handler(b_world.clone()), cfg());

        let a_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let b_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        tokio::spawn(a_svc.run(a_listener, vec![], token.clone()));
        tokio::spawn(b_svc.run(b_listener, vec![a_addr], token.clone()));

        assert!(wait_until(2000, || a_handle.live_peers() >= 1).await, "A never got a peer");
        a_handle.advertise_block(1);

        let converged =
            wait_until(3000, || b_world.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap() == 1).await;
        assert!(converged, "B did not converge to A's head");
        // B stored the very block A produced.
        assert!(b_world.get_block_by_num(1).unwrap().is_some());

        token.cancel();
    }

    /// A block signed by a non-active producer is fetched but rejected by the gate,
    /// so B does not advance.
    #[tokio::test]
    async fn block_from_foreign_producer_is_not_applied() {
        let sk = SecretKey::from_slice(&[0x88u8; 32]).unwrap();
        let block1 = produce_block(&genesis(), &sk, 3000, vec![], 30);

        let a_world = Arc::new(WorldState::new(MemoryStore::new()));
        a_world.put_block(&genesis()).unwrap();
        // A serves block 1 from its store (store it directly; serving needs no gate).
        a_world.put_block(&block1).unwrap();

        // B's active set does NOT include the producer.
        let b_world = Arc::new(WorldState::new(MemoryStore::new()));
        b_world.put_block(&genesis()).unwrap();
        b_world.put_active_witnesses(&[Address::from_body([0x09; 20])]).unwrap();

        let cfg = || ChannelConfig { keepalive: Duration::from_millis(50), hello: b"node".to_vec() };
        let (a_svc, a_handle) = ChannelService::new(handler(a_world.clone()), cfg());
        let (b_svc, _b) = ChannelService::new(handler(b_world.clone()), cfg());

        let a_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = a_listener.local_addr().unwrap();
        let b_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        tokio::spawn(a_svc.run(a_listener, vec![], token.clone()));
        tokio::spawn(b_svc.run(b_listener, vec![a_addr], token.clone()));

        assert!(wait_until(2000, || a_handle.live_peers() >= 1).await);
        a_handle.advertise_block(1);

        // Give the fetch+gate time to run and reject.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(b_world.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 0);
        assert!(b_world.get_block_by_num(1).unwrap().is_none());

        token.cancel();
    }

    // -- PBFT inbound commit accounting (T08) ----------------------------

    use tron_crypto::sign_digest;
    use tron_state::props;

    fn sr_key(b: u8) -> SecretKey {
        SecretKey::from_slice(&[b; 32]).unwrap()
    }

    fn sr_addr(k: &SecretKey) -> Address {
        address_from_public_key(&public_key(k))
    }

    /// A commit for `block_num`/`block_id` signed by SR `k` (v = 27 + recovery id).
    fn commit(block_num: i64, block_id: [u8; 32], k: &SecretKey) -> Vec<u8> {
        let digest = PbftCommit::digest(block_num, &block_id);
        let sig = sign_digest(k, &digest).unwrap();
        let mut signature = [0u8; 65];
        signature[..64].copy_from_slice(&sig.rs);
        signature[64] = 27 + sig.recovery_id;
        PbftCommit { block_num, block_id, signature }.encode()
    }

    /// A handler over a state with `active` SRs, head at `head`, and `allow_pbft`.
    fn pbft_handler(
        active: &[Address],
        head: i64,
        allow_pbft: bool,
    ) -> (Arc<NodeChannelHandler<MemoryStore>>, Arc<WorldState<MemoryStore>>) {
        let world = Arc::new(WorldState::new(MemoryStore::new()));
        world.put_block(&genesis()).unwrap();
        world.put_prop_i64(LATEST_BLOCK_NUMBER, head).unwrap();
        world.put_active_witnesses(active).unwrap();
        world.put_prop_i64(props::ALLOW_PBFT, if allow_pbft { 1 } else { 0 }).unwrap();
        let h = Arc::new(NodeChannelHandler::new(
            world.clone(),
            Arc::new(Mutex::new(Mempool::default())),
            true,
        ));
        (h, world)
    }

    const BID: [u8; 32] = [0xAB; 32];

    #[test]
    fn quorum_of_distinct_srs_advances_persisted_solidified() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)]; // threshold = 3
        let (h, world) = pbft_handler(&active, 1, true);

        assert!(h.on_pbft_commit(&commit(1, BID, &k1)));
        assert_eq!(world.get_solidified_block().unwrap(), 0); // 1/3
        assert!(h.on_pbft_commit(&commit(1, BID, &k2)));
        assert_eq!(world.get_solidified_block().unwrap(), 0); // 2/3
        assert!(h.on_pbft_commit(&commit(1, BID, &k3)));
        assert_eq!(world.get_solidified_block().unwrap(), 1); // quorum -> finalized + persisted
    }

    #[test]
    fn non_sr_commit_is_rejected() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)];
        let (h, world) = pbft_handler(&active, 1, true);
        // Signed by a key that is not an active SR.
        assert!(!h.on_pbft_commit(&commit(1, BID, &sr_key(9))));
        assert_eq!(world.get_solidified_block().unwrap(), 0);
    }

    #[test]
    fn duplicate_block_sr_counts_once() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)];
        let (h, world) = pbft_handler(&active, 1, true);

        assert!(h.on_pbft_commit(&commit(1, BID, &k1)));
        assert!(!h.on_pbft_commit(&commit(1, BID, &k1))); // duplicate (block, sr) -> not accepted
        assert_eq!(world.get_solidified_block().unwrap(), 0); // still only 1 distinct SR
        // Two more distinct SRs reach quorum.
        assert!(h.on_pbft_commit(&commit(1, BID, &k2)));
        assert!(h.on_pbft_commit(&commit(1, BID, &k3)));
        assert_eq!(world.get_solidified_block().unwrap(), 1);
    }

    #[test]
    fn commits_dropped_when_allow_pbft_off() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)];
        let (h, world) = pbft_handler(&active, 1, false); // allow_pbft OFF
        for k in [&k1, &k2, &k3] {
            assert!(!h.on_pbft_commit(&commit(1, BID, k)));
        }
        assert_eq!(world.get_solidified_block().unwrap(), 0); // nothing finalizes
    }

    #[test]
    fn solidified_never_skips_a_gap() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)];
        let (h, world) = pbft_handler(&active, 2, true); // head = 2

        // Quorum for block 2 only — block 1 is unconfirmed, so finality can't reach 2.
        for k in [&k1, &k2, &k3] {
            assert!(h.on_pbft_commit(&commit(2, BID, k)));
        }
        assert_eq!(world.get_solidified_block().unwrap(), 0); // gap at block 1

        // Now confirm block 1 to quorum -> solidified advances contiguously to 2.
        for k in [&k1, &k2, &k3] {
            assert!(h.on_pbft_commit(&commit(1, BID, k)));
        }
        assert_eq!(world.get_solidified_block().unwrap(), 2);
    }

    // -- PBFT commit GENERATION + broadcast (T09) -------------------------

    /// A witness node at head 1 (block 1 applied through the gate), allow_pbft on,
    /// configured to sign with `key`.
    fn witness_node(
        active: &[Address],
        block1: &protocol::Block,
        key: SecretKey,
    ) -> (Arc<NodeChannelHandler<MemoryStore>>, Arc<WorldState<MemoryStore>>) {
        let world = Arc::new(WorldState::new(MemoryStore::new()));
        world.put_block(&genesis()).unwrap();
        world.put_active_witnesses(active).unwrap();
        world.put_prop_i64(props::ALLOW_PBFT, 1).unwrap();
        let a = world.get_active_witnesses().unwrap();
        crate::sync::apply_synced_blocks_gated(&world, &[block1.encode_to_vec()], true, Some(&a))
            .unwrap();
        let h = Arc::new(
            NodeChannelHandler::new(
                world.clone(),
                Arc::new(Mutex::new(Mempool::default())),
                true,
            )
            .with_witness(Some(key)),
        );
        (h, world)
    }

    #[test]
    fn on_block_emits_self_commit_and_sole_sr_solidifies() {
        // Sole SR (threshold 1): accepting block 1 via on_block signs our commit and
        // self-records it, immediately solidifying the block — no peers needed.
        let k = sr_key(1);
        let world = Arc::new(WorldState::new(MemoryStore::new()));
        world.put_block(&genesis()).unwrap();
        world.put_active_witnesses(&[sr_addr(&k)]).unwrap();
        world.put_prop_i64(props::ALLOW_PBFT, 1).unwrap();
        let h = Arc::new(
            NodeChannelHandler::new(world.clone(), Arc::new(Mutex::new(Mempool::default())), true)
                .with_witness(Some(k.clone())),
        );
        let block1 = produce_block(&genesis(), &k, 3000, vec![], 30);
        assert_eq!(h.on_block(&block1.encode_to_vec()), Some(1));
        assert_eq!(world.get_solidified_block().unwrap(), 1);
    }

    #[test]
    fn non_witness_node_does_not_emit_commit() {
        // Same sole-SR state, but this handler has no witness key -> emit is a no-op,
        // so nothing is self-recorded and finality never advances.
        let k = sr_key(1);
        let world = Arc::new(WorldState::new(MemoryStore::new()));
        world.put_block(&genesis()).unwrap();
        world.put_active_witnesses(&[sr_addr(&k)]).unwrap();
        world.put_prop_i64(props::ALLOW_PBFT, 1).unwrap();
        let h = Arc::new(NodeChannelHandler::new(
            world.clone(),
            Arc::new(Mutex::new(Mempool::default())),
            true,
        ));
        h.emit_commit(1, BID);
        assert_eq!(world.get_solidified_block().unwrap(), 0);
    }

    /// 3 in-process witness nodes over the real TCP channel (line topology a<-b<-c):
    /// each emits its commit for block 1; commits circulate (advertise -> peer
    /// on_pbft_commit -> re-advertise) until all 3 distinct SR commits reach every
    /// node, and every node's persisted solidified block advances to 1 (threshold 3).
    #[tokio::test]
    async fn three_witness_nodes_reach_quorum_solidified_over_channel() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)];
        let block1 = produce_block(&genesis(), &k1, 3000, vec![], 30);
        let id = tron_chain::block_id_of(&block1).unwrap().0;

        let (ha, wa) = witness_node(&active, &block1, k1.clone());
        let (hb, wb) = witness_node(&active, &block1, k2.clone());
        let (hc, wc) = witness_node(&active, &block1, k3.clone());

        let cfg =
            || ChannelConfig { keepalive: Duration::from_millis(50), hello: b"node".to_vec() };
        let (sa, adva) = ChannelService::new(ha.clone(), cfg());
        let (sb, advb) = ChannelService::new(hb.clone(), cfg());
        let (sc, advc) = ChannelService::new(hc.clone(), cfg());
        ha.set_commit_handle(adva.clone());
        hb.set_commit_handle(advb.clone());
        hc.set_commit_handle(advc.clone());

        let la = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = la.local_addr().unwrap();
        let lb = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_addr = lb.local_addr().unwrap();
        let lc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        tokio::spawn(sa.run(la, vec![], token.clone()));
        tokio::spawn(sb.run(lb, vec![a_addr], token.clone()));
        tokio::spawn(sc.run(lc, vec![b_addr], token.clone()));

        assert!(
            wait_until(3000, || adva.live_peers() >= 1
                && advb.live_peers() >= 2
                && advc.live_peers() >= 1)
            .await,
            "nodes never connected"
        );

        ha.emit_commit(1, id);
        hb.emit_commit(1, id);
        hc.emit_commit(1, id);

        let ok = wait_until(5000, || {
            wa.get_solidified_block().unwrap() == 1
                && wb.get_solidified_block().unwrap() == 1
                && wc.get_solidified_block().unwrap() == 1
        })
        .await;
        assert!(
            ok,
            "not all solidified: a={} b={} c={}",
            wa.get_solidified_block().unwrap(),
            wb.get_solidified_block().unwrap(),
            wc.get_solidified_block().unwrap()
        );
        token.cancel();
    }

    /// Only 2 of the 3 active SRs emit -> quorum (3) is never met, so no node solidifies.
    #[tokio::test]
    async fn two_of_three_sr_commits_do_not_solidify() {
        let (k1, k2, k3) = (sr_key(1), sr_key(2), sr_key(3));
        let active = vec![sr_addr(&k1), sr_addr(&k2), sr_addr(&k3)]; // threshold 3
        let block1 = produce_block(&genesis(), &k1, 3000, vec![], 30);
        let id = tron_chain::block_id_of(&block1).unwrap().0;

        let (ha, wa) = witness_node(&active, &block1, k1.clone());
        let (hb, wb) = witness_node(&active, &block1, k2.clone());

        let cfg =
            || ChannelConfig { keepalive: Duration::from_millis(50), hello: b"node".to_vec() };
        let (sa, adva) = ChannelService::new(ha.clone(), cfg());
        let (sb, advb) = ChannelService::new(hb.clone(), cfg());
        ha.set_commit_handle(adva.clone());
        hb.set_commit_handle(advb.clone());

        let la = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a_addr = la.local_addr().unwrap();
        let lb = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        tokio::spawn(sa.run(la, vec![], token.clone()));
        tokio::spawn(sb.run(lb, vec![a_addr], token.clone()));

        assert!(wait_until(2000, || adva.live_peers() >= 1 && advb.live_peers() >= 1).await);

        ha.emit_commit(1, id);
        hb.emit_commit(1, id);

        // Give commits time to circulate, then confirm neither node solidified.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(wa.get_solidified_block().unwrap(), 0);
        assert_eq!(wb.get_solidified_block().unwrap(), 0);
        token.cancel();
    }
}
