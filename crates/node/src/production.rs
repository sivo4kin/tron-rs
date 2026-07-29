//! DPoS block production (T07): the per-slot decision + local application.
//!
//! Mirrors java-tron `DposTask.produceBlock`: at each slot, if this node is the
//! scheduled witness and its head is current, drain the mempool into a signed
//! block on top of the head. The production service loop ([`crate::run`]) calls
//! [`try_produce`] on each interval and, on a produced block, [`apply_produced`]s
//! it locally and gossips it via the channel service.
//!
//! Timing guards (CS-JTRON audit notes): the block timestamp is a genesis-anchored
//! multiple of the 3s interval — we never assume exact wall-clock spacing — and we
//! only produce for a slot strictly ahead of the head's slot, so a stale head or a
//! double-production for one slot is impossible.
//!
//! Boundary: fork-choice among competing produced blocks is [`tron_consensus::fork`];
//! PBFT finality broadcast is a further step, not wired here.

use std::sync::{Arc, Mutex};
use tron_consensus::mempool::Mempool;
use tron_consensus::producer::produce_from_pool;
use tron_consensus::{scheduled_witness, slot_of, BLOCK_INTERVAL_MS};
use tron_crypto::{address_from_public_key, public_key, SecretKey};
use tron_proto::protocol;
use tron_state::WorldState;
use tron_storage::KvStore;

/// Max transactions drained into one produced block.
pub const MAX_BLOCK_TXS: usize = 1000;
/// Block header version we stamp on produced blocks.
pub const BLOCK_VERSION: i32 = 30;

fn header_ts_num(block: &protocol::Block) -> Option<(i64, i64)> {
    let raw = block.block_header.as_ref()?.raw_data.as_ref()?;
    Some((raw.timestamp, raw.number))
}

/// Current wall-clock time in ms since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Decide whether this node produces at `now_ms` and, if so, build+sign the block
/// on top of the current head. Returns `None` when it isn't our scheduled slot, the
/// slot is already covered by the head (stale), or the chain/active set/state isn't
/// ready. Pure: no state mutation — the caller applies it via [`apply_produced`].
pub fn try_produce<S: KvStore>(
    state: &WorldState<S>,
    mempool: &Arc<Mutex<Mempool>>,
    witness_key: &SecretKey,
    now_ms: i64,
) -> Option<protocol::Block> {
    let head = state.get_now_block().ok()??;
    let genesis = state.get_block_by_num(0).ok()??;
    let (genesis_ts, _) = header_ts_num(&genesis)?;
    let (head_ts, _) = header_ts_num(&head)?;
    if now_ms <= genesis_ts {
        return None;
    }

    let now_slot = slot_of(now_ms as u64, genesis_ts as u64);
    let head_slot = slot_of(head_ts as u64, genesis_ts as u64);
    if now_slot <= head_slot {
        return None; // this slot is already covered by the head (or head is ahead)
    }

    let active = state.get_active_witnesses().ok()?;
    let scheduled = scheduled_witness(&active, head_slot, now_slot - head_slot)?;
    let our = address_from_public_key(&public_key(witness_key)).as_bytes().to_vec();
    if scheduled != &our {
        return None; // not our scheduled slot
    }

    // Slot-boundary timestamp: a genesis-anchored multiple of the interval.
    let block_ts = genesis_ts + now_slot as i64 * BLOCK_INTERVAL_MS as i64;
    let pool = mempool.lock().ok()?;
    Some(produce_from_pool(&head, witness_key, block_ts, &pool, MAX_BLOCK_TXS, BLOCK_VERSION))
}

/// Apply a locally produced block: store it (advancing the head), index its
/// transactions, and evict them from the mempool. Returns the block number.
pub fn apply_produced<S: KvStore>(
    state: &WorldState<S>,
    mempool: &Arc<Mutex<Mempool>>,
    block: &protocol::Block,
) -> Option<i64> {
    state.put_block(block).ok()?;
    state.index_block_transactions(block).ok()?;
    if let Ok(mut pool) = mempool.lock() {
        pool.evict_included(block);
    }
    header_ts_num(block).map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_consensus::validation::validate_block_intake;
    use tron_crypto::{address_from_public_key, public_key, SecretKey};
    use tron_state::blocks::LATEST_BLOCK_NUMBER;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    const GENESIS_TS: i64 = 1_700_000_000_000;

    fn key(b: u8) -> SecretKey {
        SecretKey::from_slice(&[b; 32]).unwrap()
    }

    fn our_address(k: &SecretKey) -> Vec<u8> {
        address_from_public_key(&public_key(k)).as_bytes().to_vec()
    }

    /// Genesis block (number 0) at `GENESIS_TS`.
    fn genesis() -> protocol::Block {
        protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw {
                    number: 0,
                    timestamp: GENESIS_TS,
                    ..Default::default()
                }),
                witness_signature: vec![],
            }),
            transactions: vec![],
        }
    }

    fn state_with(active: &[Vec<u8>]) -> (WorldState<MemoryStore>, Arc<Mutex<Mempool>>) {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_block(&genesis()).unwrap();
        // Store the elected active-witness set (H05 accessor).
        let addrs: Vec<Address> = active
            .iter()
            .map(|a| Address::from_bytes(a.as_slice().try_into().unwrap()).unwrap())
            .collect();
        ws.put_active_witnesses(&addrs).unwrap();
        (ws, Arc::new(Mutex::new(Mempool::default())))
    }

    #[test]
    fn scheduled_witness_produces_valid_block_that_applies() {
        let k = key(0x33);
        let active = vec![our_address(&k)]; // sole witness -> always scheduled
        let (state, mempool) = state_with(&active);

        // now in slot 1 (genesis is slot 0).
        let now = GENESIS_TS + BLOCK_INTERVAL_MS as i64 + 500;
        let block = try_produce(&state, &mempool, &k, now).expect("should produce");

        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        assert_eq!(raw.number, 1);
        assert_eq!(raw.timestamp, GENESIS_TS + BLOCK_INTERVAL_MS as i64); // slot boundary

        // Passes the live intake gate against the active set.
        validate_block_intake(&block, &active).expect("valid produced block");

        // Applying it advances the head.
        assert_eq!(apply_produced(&state, &mempool, &block), Some(1));
        assert_eq!(state.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 1);
    }

    #[test]
    fn non_scheduled_node_does_not_produce() {
        let ours = key(0x33);
        let other = key(0x44);
        // Active set is the OTHER witness; we are not scheduled.
        let active = vec![our_address(&other)];
        let (state, mempool) = state_with(&active);
        let now = GENESIS_TS + BLOCK_INTERVAL_MS as i64 + 500;
        assert!(try_produce(&state, &mempool, &ours, now).is_none());
    }

    #[test]
    fn does_not_produce_twice_in_the_same_slot_or_on_stale_head() {
        let k = key(0x33);
        let active = vec![our_address(&k)];
        let (state, mempool) = state_with(&active);

        // Produce for slot 1 and apply it (head now at slot 1).
        let now1 = GENESIS_TS + BLOCK_INTERVAL_MS as i64 + 10;
        let b = try_produce(&state, &mempool, &k, now1).unwrap();
        apply_produced(&state, &mempool, &b);

        // Same slot again -> nothing (head already covers it).
        assert!(try_produce(&state, &mempool, &k, now1 + 100).is_none());
        // A time still within the head's slot window -> nothing.
        assert!(try_produce(&state, &mempool, &k, GENESIS_TS + BLOCK_INTERVAL_MS as i64 + 2000).is_none());
    }

    #[test]
    fn produced_block_syncs_to_a_second_node() {
        use prost::Message;
        let k = key(0x33);
        let active = vec![our_address(&k)];

        // Node A produces a block for its scheduled slot.
        let (a_state, a_pool) = state_with(&active);
        let now = GENESIS_TS + BLOCK_INTERVAL_MS as i64 + 10;
        let block = try_produce(&a_state, &a_pool, &k, now).unwrap();

        // Node B (same genesis + active set) receives it as gossip bytes and applies
        // it through the intake gate (T04 fetch -> T06/H05 gated apply), converging.
        let (b_state, _b_pool) = state_with(&active);
        let bytes = vec![block.encode_to_vec()];
        let applied =
            crate::sync::apply_synced_blocks_gated(&b_state, &bytes, true, Some(&active)).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(b_state.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 1);
    }

    #[test]
    fn no_production_before_any_chain_or_active_set() {
        // No genesis stored -> no head -> no production.
        let ws = WorldState::new(MemoryStore::new());
        let mempool = Arc::new(Mutex::new(Mempool::default()));
        assert!(try_produce(&ws, &mempool, &key(0x33), GENESIS_TS + 5000).is_none());

        // Genesis present but empty active set -> no scheduled witness.
        let (state, mempool) = state_with(&[]);
        assert!(try_produce(&state, &mempool, &key(0x33), GENESIS_TS + 5000).is_none());
    }
}
