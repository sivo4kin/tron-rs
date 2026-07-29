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
use std::sync::{Arc, Mutex};
use tron_consensus::mempool::{admit_transaction, Mempool};
use tron_p2p::service::ChannelHandler;
use tron_state::{blocks::LATEST_BLOCK_NUMBER, WorldState};
use tron_storage::KvStore;

/// A [`ChannelHandler`] over the node's shared world state + mempool.
pub struct NodeChannelHandler<S: KvStore> {
    world: Arc<WorldState<S>>,
    mempool: Arc<Mutex<Mempool>>,
    /// Whether inbound blocks must carry a valid witness signature.
    require_sig: bool,
}

impl<S: KvStore> NodeChannelHandler<S> {
    pub fn new(world: Arc<WorldState<S>>, mempool: Arc<Mutex<Mempool>>, require_sig: bool) -> Self {
        Self { world, mempool, require_sig }
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
            Ok(applied) if applied >= 1 => Some(self.head()),
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
}
