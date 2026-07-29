//! Pending-transaction pool (java-tron `PendingManager` / opentron mempool).
//!
//! Holds not-yet-included transactions keyed by their id ([`tron_chain::tx_id`]),
//! deduplicated, insertion-ordered, and capacity-bounded. Block production drains
//! it in order; tx relay and block application evict included txs.

use prost::Message;
use std::collections::HashSet;
use thiserror::Error;
use tron_chain::{block_id_of, recover_tx_signer, tx_id, tx_owner_address};
use tron_proto::protocol;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::H256;

/// Default mempool capacity (bounded to resist flooding).
pub const DEFAULT_CAPACITY: usize = 10_000;

/// Max serialized transaction size (java-tron `Constant.TRANSACTION_MAX_BYTE_SIZE`).
pub const MAX_TX_SIZE: usize = 512 * 1024;

/// Reject transactions whose expiration is more than this far in the future
/// (java-tron `maxFutureTransactionTimeInterval`, 1 day by default).
pub const MAX_FUTURE_EXPIRATION_MS: i64 = 24 * 60 * 60 * 1000;

/// Why a transaction was refused admission (java-tron `pushTransaction` rejects).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdmitError {
    #[error("transaction too large: {0} bytes")]
    TooLarge(usize),
    #[error("transaction has no raw_data")]
    NoRawData,
    #[error("transaction has no contract")]
    NoContract,
    #[error("bad or missing signature")]
    BadSignature,
    #[error("transaction expired (expiration {expiration} <= now {now})")]
    Expired { expiration: i64, now: i64 },
    #[error("expiration too far in the future (expiration {expiration}, now {now})")]
    TooFarInFuture { expiration: i64, now: i64 },
    #[error("ref-block (TaPoS) mismatch")]
    RefBlockMismatch,
    #[error("duplicate transaction (already pooled)")]
    Duplicate,
    #[error("transaction already included in a block")]
    AlreadyIncluded,
    #[error("contract validation failed: {0}")]
    Invalid(String),
    #[error("mempool is full")]
    PoolFull,
}

/// An insertion-ordered, deduplicated transaction pool.
pub struct Mempool {
    order: Vec<(H256, protocol::Transaction)>,
    seen: HashSet<H256>,
    capacity: usize,
}

impl Mempool {
    pub fn new(capacity: usize) -> Self {
        Self { order: Vec::new(), seen: HashSet::new(), capacity }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, id: &H256) -> bool {
        self.seen.contains(id)
    }

    /// Add a transaction. Returns `false` if it is a duplicate or the pool is full.
    pub fn add(&mut self, tx: protocol::Transaction) -> bool {
        if self.order.len() >= self.capacity {
            return false;
        }
        let id = tx_id(&tx);
        if !self.seen.insert(id) {
            return false; // duplicate
        }
        self.order.push((id, tx));
        true
    }

    /// Remove a transaction by id (e.g. once included in a block). Returns whether
    /// it was present.
    pub fn remove(&mut self, id: &H256) -> bool {
        if !self.seen.remove(id) {
            return false;
        }
        if let Some(pos) = self.order.iter().position(|(i, _)| i == id) {
            self.order.remove(pos);
        }
        true
    }

    /// Peek the first `n` transactions in insertion order (block assembly).
    pub fn peek(&self, n: usize) -> Vec<protocol::Transaction> {
        self.order.iter().take(n).map(|(_, tx)| tx.clone()).collect()
    }

    /// Evict every transaction included in `block`.
    pub fn evict_included(&mut self, block: &protocol::Block) {
        for tx in &block.transactions {
            self.remove(&tx_id(tx));
        }
    }

    /// Drop transactions whose `expiration` has passed (`<= now_ms`). Returns the
    /// number evicted. A zero expiration means "no expiry" and is never evicted.
    pub fn evict_expired(&mut self, now_ms: i64) -> usize {
        let before = self.order.len();
        let seen = &mut self.seen;
        self.order.retain(|(id, tx)| {
            let exp = tx.raw_data.as_ref().map(|r| r.expiration).unwrap_or(0);
            let keep = exp == 0 || exp > now_ms;
            if !keep {
                seen.remove(id);
            }
            keep
        });
        before - self.order.len()
    }
}

/// Verify a transaction's TaPoS ref-block against stored blocks: the block whose
/// number matches `ref_block_bytes` (low 16 bits) near the head must have a block
/// id whose bytes `[8..16]` equal `ref_block_hash`. Lenient when there is nothing
/// to check (empty ref hash, or the chain has no such block yet).
fn verify_ref_block<S: KvStore>(
    state: &WorldState<S>,
    raw: &protocol::transaction::Raw,
) -> Result<(), AdmitError> {
    if raw.ref_block_hash.is_empty() {
        return Ok(()); // no TaPoS reference (genesis-era / test tx)
    }
    let head = state
        .get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER)
        .map_err(|_| AdmitError::RefBlockMismatch)?;
    if head <= 0 || raw.ref_block_bytes.len() != 2 {
        return Ok(()); // nothing to verify against yet
    }
    // The largest block number <= head whose low 16 bits match ref_block_bytes.
    let ref16 = u16::from_be_bytes([raw.ref_block_bytes[0], raw.ref_block_bytes[1]]) as i64;
    let diff = (head - ref16).rem_euclid(0x1_0000);
    let candidate = head - diff;
    let block = match state.get_block_by_num(candidate).map_err(|_| AdmitError::RefBlockMismatch)? {
        Some(b) => b,
        None => return Err(AdmitError::RefBlockMismatch),
    };
    let id = block_id_of(&block).ok_or(AdmitError::RefBlockMismatch)?;
    if id.0.get(8..16) == Some(raw.ref_block_hash.as_slice()) {
        Ok(())
    } else {
        Err(AdmitError::RefBlockMismatch)
    }
}

/// Admit a transaction into `pool` after the full java-tron `pushTransaction`
/// pipeline: size bound, signature recovery + owner match, expiration/TaPoS, dedup
/// against the pool and already-included txs, then the caller-supplied actuator
/// `validate` (cheap rejection, **no execution**). Returns the tx id on success.
///
/// The actuator check is injected as `validate` because `tron-consensus` cannot
/// depend on `tron-actuators` (the dependency runs the other way); callers pass a
/// closure over `tron_actuators` validation. On success the returned id is the
/// **gossip seam** — the caller advertises it to peers (T04 wires this).
///
/// Deviation: bandwidth/energy pre-charge is deferred to block execution (java-tron
/// charges it in `pushTransaction`); documented in the task.
#[allow(clippy::too_many_arguments)]
pub fn admit_transaction<S, F>(
    pool: &mut Mempool,
    state: &WorldState<S>,
    tx: &protocol::Transaction,
    now_ms: i64,
    validate: F,
) -> Result<H256, AdmitError>
where
    S: KvStore,
    F: FnOnce(&WorldState<S>, &protocol::Transaction) -> Result<(), String>,
{
    // 1. Size bound.
    let size = tx.encoded_len();
    if size > MAX_TX_SIZE {
        return Err(AdmitError::TooLarge(size));
    }

    // 2. Structure.
    let raw = tx.raw_data.as_ref().ok_or(AdmitError::NoRawData)?;
    if raw.contract.is_empty() {
        return Err(AdmitError::NoContract);
    }

    // 3. Signature: recover the signer and require it to match the contract owner.
    let signer = recover_tx_signer(tx).ok_or(AdmitError::BadSignature)?;
    if let Some(owner) = tx_owner_address(tx) {
        if signer.as_bytes() != owner.as_slice() {
            return Err(AdmitError::BadSignature);
        }
    }

    // 4. Expiration window.
    if raw.expiration != 0 {
        if raw.expiration <= now_ms {
            return Err(AdmitError::Expired { expiration: raw.expiration, now: now_ms });
        }
        if raw.expiration > now_ms + MAX_FUTURE_EXPIRATION_MS {
            return Err(AdmitError::TooFarInFuture { expiration: raw.expiration, now: now_ms });
        }
    }

    // 5. TaPoS ref-block.
    verify_ref_block(state, raw)?;

    // 6. Dedup — against the pool and against already-included transactions.
    let id = tx_id(tx);
    if pool.contains(&id) {
        return Err(AdmitError::Duplicate);
    }
    if state.get_transaction(id.0.as_slice()).ok().flatten().is_some() {
        return Err(AdmitError::AlreadyIncluded);
    }

    // 7. Actuator validation (no execution).
    validate(state, tx).map_err(AdmitError::Invalid)?;

    // 8. Pool it (capacity-bounded).
    if !pool.add(tx.clone()) {
        return Err(AdmitError::PoolFull);
    }
    Ok(id)
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(n: u64) -> protocol::Transaction {
        protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw {
                ref_block_num: n as i64,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn add_dedups_and_orders() {
        let mut pool = Mempool::default();
        assert!(pool.add(tx(1)));
        assert!(pool.add(tx(2)));
        assert!(!pool.add(tx(1))); // duplicate id
        assert_eq!(pool.len(), 2);
        let peeked = pool.peek(10);
        assert_eq!(peeked.len(), 2);
        // insertion order preserved
        assert_eq!(peeked[0].raw_data.as_ref().unwrap().ref_block_num, 1);
    }

    #[test]
    fn capacity_bound_rejects_when_full() {
        let mut pool = Mempool::new(2);
        assert!(pool.add(tx(1)));
        assert!(pool.add(tx(2)));
        assert!(!pool.add(tx(3))); // full
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn remove_and_contains() {
        let mut pool = Mempool::default();
        pool.add(tx(1));
        let id = tx_id(&tx(1));
        assert!(pool.contains(&id));
        assert!(pool.remove(&id));
        assert!(!pool.contains(&id));
        assert!(!pool.remove(&id)); // already gone
        assert!(pool.is_empty());
    }

    #[test]
    fn evict_included_clears_block_txs() {
        let mut pool = Mempool::default();
        pool.add(tx(1));
        pool.add(tx(2));
        pool.add(tx(3));
        let block = protocol::Block {
            transactions: vec![tx(1), tx(3)],
            ..Default::default()
        };
        pool.evict_included(&block);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&tx_id(&tx(2))));
    }

    #[test]
    fn peek_takes_prefix_for_block_assembly() {
        let mut pool = Mempool::default();
        for i in 0..5 {
            pool.add(tx(i));
        }
        assert_eq!(pool.peek(3).len(), 3);
        assert_eq!(pool.peek(100).len(), 5);
    }

    // -- admission pipeline ----------------------------------------------

    mod admit {
        use super::super::*;
        use tron_crypto::{address_from_public_key, public_key, sign_digest, SecretKey};
        use tron_state::WorldState;
        use tron_storage::MemoryStore;
        use tron_types::Address;

        const NOW: i64 = 1_700_000_000_000;

        fn sk() -> SecretKey {
            SecretKey::from_slice(&[0x11u8; 32]).unwrap()
        }

        /// Build a transfer tx and sign it with `key` (whose address is the owner
        /// unless `wrong_owner`). `expiration` 0 means no expiry.
        fn signed_transfer(key: &SecretKey, expiration: i64, wrong_owner: bool) -> protocol::Transaction {
            let owner = if wrong_owner {
                Address::from_body([0x09; 20]) // not the signer
            } else {
                address_from_public_key(&public_key(key))
            };
            let c = protocol::TransferContract {
                owner_address: owner.as_bytes().to_vec(),
                to_address: Address::from_body([0x02; 20]).as_bytes().to_vec(),
                amount: 1_000,
            };
            let contract = protocol::transaction::Contract {
                r#type: protocol::transaction::contract::ContractType::TransferContract as i32,
                parameter: Some(prost_types::Any {
                    type_url: "type.googleapis.com/protocol.TransferContract".into(),
                    value: c.encode_to_vec(),
                }),
                ..Default::default()
            };
            let raw = protocol::transaction::Raw {
                contract: vec![contract],
                expiration,
                ..Default::default()
            };
            let mut tx = protocol::Transaction { raw_data: Some(raw), ..Default::default() };
            let digest = tx_id(&tx).0;
            let sig = sign_digest(key, &digest).unwrap();
            let mut sig_bytes = sig.rs.to_vec();
            sig_bytes.push(sig.recovery_id);
            tx.signature = vec![sig_bytes];
            tx
        }

        fn ok(_: &WorldState<MemoryStore>, _: &protocol::Transaction) -> Result<(), String> {
            Ok(())
        }

        #[test]
        fn admits_well_formed_signed_tx() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            let tx = signed_transfer(&sk(), NOW + 60_000, false);
            let id = admit_transaction(&mut pool, &state, &tx, NOW, ok).unwrap();
            assert_eq!(id, tx_id(&tx));
            assert_eq!(pool.len(), 1);
            assert!(pool.contains(&id));
        }

        #[test]
        fn rejects_bad_signature() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            // Signed by the key, but the contract owner is someone else.
            let tx = signed_transfer(&sk(), NOW + 60_000, true);
            assert_eq!(
                admit_transaction(&mut pool, &state, &tx, NOW, ok),
                Err(AdmitError::BadSignature)
            );
            // Missing signature entirely.
            let mut unsigned = signed_transfer(&sk(), NOW + 60_000, false);
            unsigned.signature.clear();
            assert_eq!(
                admit_transaction(&mut pool, &state, &unsigned, NOW, ok),
                Err(AdmitError::BadSignature)
            );
            assert!(pool.is_empty());
        }

        #[test]
        fn rejects_expired_and_far_future() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            let expired = signed_transfer(&sk(), NOW - 1, false);
            assert!(matches!(
                admit_transaction(&mut pool, &state, &expired, NOW, ok),
                Err(AdmitError::Expired { .. })
            ));
            let far = signed_transfer(&sk(), NOW + MAX_FUTURE_EXPIRATION_MS + 1, false);
            assert!(matches!(
                admit_transaction(&mut pool, &state, &far, NOW, ok),
                Err(AdmitError::TooFarInFuture { .. })
            ));
        }

        #[test]
        fn rejects_duplicate_and_already_included() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            let tx = signed_transfer(&sk(), NOW + 60_000, false);
            admit_transaction(&mut pool, &state, &tx, NOW, ok).unwrap();
            // Second admission of the same tx -> duplicate (already pooled).
            assert_eq!(
                admit_transaction(&mut pool, &state, &tx, NOW, ok),
                Err(AdmitError::Duplicate)
            );
            // A tx already recorded in state (included in a block) -> AlreadyIncluded.
            let tx2 = signed_transfer(&sk(), NOW + 61_000, false);
            let mut pool2 = Mempool::default();
            state.put_transaction(tx_id(&tx2).0.as_slice(), &tx2).unwrap();
            assert_eq!(
                admit_transaction(&mut pool2, &state, &tx2, NOW, ok),
                Err(AdmitError::AlreadyIncluded)
            );
        }

        #[test]
        fn rejects_actuator_invalid() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            let tx = signed_transfer(&sk(), NOW + 60_000, false);
            let err = admit_transaction(&mut pool, &state, &tx, NOW, |_, _| {
                Err("balance is not sufficient".into())
            });
            assert_eq!(err, Err(AdmitError::Invalid("balance is not sufficient".into())));
            assert!(pool.is_empty()); // not pooled on validation failure
        }

        #[test]
        fn rejects_too_large() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::default();
            // Oversize via a big raw_data.data blob (checked before signature).
            let raw = protocol::transaction::Raw {
                data: vec![0u8; MAX_TX_SIZE + 1],
                ..Default::default()
            };
            let tx = protocol::Transaction { raw_data: Some(raw), ..Default::default() };
            assert!(matches!(
                admit_transaction(&mut pool, &state, &tx, NOW, ok),
                Err(AdmitError::TooLarge(_))
            ));
        }

        #[test]
        fn respects_capacity_cap() {
            let state = WorldState::new(MemoryStore::new());
            let mut pool = Mempool::new(1);
            let a = signed_transfer(&sk(), NOW + 60_000, false);
            admit_transaction(&mut pool, &state, &a, NOW, ok).unwrap();
            // Pool is full: a different valid tx is rejected with PoolFull.
            let b = signed_transfer(&sk(), NOW + 61_000, false);
            assert_eq!(
                admit_transaction(&mut pool, &state, &b, NOW, ok),
                Err(AdmitError::PoolFull)
            );
        }

        #[test]
        fn pool_evicts_expired() {
            let mut pool = Mempool::default();
            pool.add(signed_transfer(&sk(), NOW + 60_000, false)); // lives
            pool.add(signed_transfer(&sk(), NOW - 1, false)); // expired
            pool.add(signed_transfer(&sk(), 0, false)); // no-expiry (kept)
            assert_eq!(pool.len(), 3);
            let dropped = pool.evict_expired(NOW);
            assert_eq!(dropped, 1);
            assert_eq!(pool.len(), 2);
        }
    }
}
