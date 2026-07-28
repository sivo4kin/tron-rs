//! DPoS consensus + PBFT finality (P3), and block production (P5).
//!
//! Reference parameters from java-tron (`common/.../config/Parameter.java`,
//! `DynamicPropertiesStore`). Validation and fork-choice land in P3 — the area
//! opentron never finished; block production (witness scheduling, mempool) is P5.

/// Number of active block-producing Super Representatives.
pub const MAX_ACTIVE_WITNESSES: usize = 27;
/// Standby witness list length.
pub const WITNESS_STANDBY_LENGTH: usize = 127;
/// Block production interval, milliseconds.
pub const BLOCK_INTERVAL_MS: u64 = 3_000;
/// Maintenance (round) period, milliseconds (6 hours) — active set is rebuilt from votes.
pub const MAINTENANCE_PERIOD_MS: u64 = 21_600_000;
/// Default block production reward, in sun.
pub const WITNESS_PAY_PER_BLOCK_SUN: i64 = 32_000_000;

/// Slot for a timestamp relative to genesis, given the block interval.
pub fn slot_of(block_time_ms: u64, genesis_time_ms: u64) -> u64 {
    if block_time_ms <= genesis_time_ms {
        return 0;
    }
    (block_time_ms - genesis_time_ms) / BLOCK_INTERVAL_MS
}

/// Compute the next maintenance timestamp, java-tron
/// `DynamicPropertiesStore.updateNextMaintenanceTime`:
/// snap forward past `block_time` in whole `MAINTENANCE_PERIOD_MS` rounds from the
/// current maintenance time. Determines when the DPoS election cycle runs.
pub fn next_maintenance_time(current_maintenance_time: i64, block_time: i64) -> i64 {
    let interval = MAINTENANCE_PERIOD_MS as i64;
    let round = (block_time - current_maintenance_time) / interval;
    current_maintenance_time + (round + 1) * interval
}

/// Whether a block at `block_time` crosses into a new maintenance round, i.e.
/// triggers the election cycle (java-tron `needMaintenance`).
pub fn need_maintenance(next_maintenance_time: i64, block_time: i64) -> bool {
    block_time >= next_maintenance_time
}

/// A witness produces this many consecutive slots before rotating (java-tron
/// `ChainConstant.SINGLE_REPEAT`).
pub const SINGLE_REPEAT: u64 = 1;

/// The scheduled producer for an offset `slot` from the head, java-tron
/// `DposSlot.getScheduledWitness`: index into the ordered active-witness list by
/// `(head_abs_slot + slot) % (n*SINGLE_REPEAT) / SINGLE_REPEAT`.
///
/// `head_abs_slot` is the absolute slot of the latest block ([`slot_of`] with the
/// genesis reference). Returns the producer's address, or `None` if there are no
/// active witnesses.
pub fn scheduled_witness<'a>(
    active: &'a [Vec<u8>],
    head_abs_slot: u64,
    slot: u64,
) -> Option<&'a Vec<u8>> {
    if active.is_empty() {
        return None;
    }
    let current = head_abs_slot + slot;
    let n = active.len() as u64;
    let idx = (current % (n * SINGLE_REPEAT)) / SINGLE_REPEAT;
    active.get(idx as usize)
}

pub mod fork;
pub mod mempool;
pub mod producer;
pub mod reward;

pub mod validation {
    //! Structural block validation (java-tron `BlockCapsule` checks):
    //! header present, `txTrieRoot` matches the transactions, witness signature
    //! recovers to `witness_address`, and parent linkage carries `number - 1`.
    //! Contextual validation (parent hash chain, slot/schedule) lands with the
    //! fork-choice work in P3.

    use thiserror::Error;
    use tron_proto::protocol;

    #[derive(Debug, Error, PartialEq)]
    pub enum BlockValidationError {
        #[error("block has no header or raw data")]
        MissingHeader,
        #[error("txTrieRoot mismatch: header {header}, computed {computed}")]
        TxTrieRootMismatch { header: String, computed: String },
        #[error("witness signature missing or malformed (len {0})")]
        BadWitnessSignature(usize),
        #[error("witness signature recovers to {recovered}, header says {header}")]
        WitnessMismatch { recovered: String, header: String },
        #[error("parent hash height prefix {got} != number-1 {expected}")]
        BadParentLink { got: i64, expected: i64 },
    }

    /// Options: served blocks on some gateways (e.g. TronGrid Nile) have the
    /// witness signature stripped; `require_witness_signature: false` skips the
    /// signature check for those sources. Full validation requires it.
    #[derive(Debug, Clone, Copy)]
    pub struct ValidationOptions {
        pub require_witness_signature: bool,
    }

    impl Default for ValidationOptions {
        fn default() -> Self {
            Self { require_witness_signature: true }
        }
    }

    /// Validate a block's self-consistency (structure, tx root, producer signature).
    pub fn validate_block(
        block: &protocol::Block,
        opts: ValidationOptions,
    ) -> Result<(), BlockValidationError> {
        let header = block
            .block_header
            .as_ref()
            .ok_or(BlockValidationError::MissingHeader)?;
        let raw = header
            .raw_data
            .as_ref()
            .ok_or(BlockValidationError::MissingHeader)?;

        // 1. txTrieRoot must match the transactions.
        let computed = tron_chain::tx_trie_root(block);
        if computed.0.as_slice() != raw.tx_trie_root.as_slice() {
            return Err(BlockValidationError::TxTrieRootMismatch {
                header: hex::encode(&raw.tx_trie_root),
                computed: computed.to_hex(),
            });
        }

        // 2. Parent linkage layout: parent id carries number-1 in its first 8 bytes.
        if raw.number > 0 && raw.parent_hash.len() == 32 {
            let got = i64::from_be_bytes(raw.parent_hash[..8].try_into().unwrap());
            if got != raw.number - 1 {
                return Err(BlockValidationError::BadParentLink {
                    got,
                    expected: raw.number - 1,
                });
            }
        }

        // 3. Producer signature must recover to the header's witness address.
        if header.witness_signature.is_empty() && !opts.require_witness_signature {
            return Ok(());
        }
        if header.witness_signature.len() != 65 {
            return Err(BlockValidationError::BadWitnessSignature(
                header.witness_signature.len(),
            ));
        }
        let recovered = tron_chain::recover_witness(block).ok_or(
            BlockValidationError::BadWitnessSignature(header.witness_signature.len()),
        )?;
        if recovered.as_bytes().as_slice() != raw.witness_address.as_slice() {
            return Err(BlockValidationError::WitnessMismatch {
                recovered: hex::encode(recovered.as_bytes()),
                header: hex::encode(&raw.witness_address),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_params_match_java_tron() {
        assert_eq!(MAX_ACTIVE_WITNESSES, 27);
        assert_eq!(BLOCK_INTERVAL_MS, 3_000);
        assert_eq!(MAINTENANCE_PERIOD_MS, 6 * 60 * 60 * 1000);
    }

    #[test]
    fn slot_math() {
        let genesis = 1_000_000;
        assert_eq!(slot_of(genesis, genesis), 0);
        assert_eq!(slot_of(genesis + 3_000, genesis), 1);
        assert_eq!(slot_of(genesis + 9_000, genesis), 3);
    }

    #[test]
    fn witness_scheduling_rotates_round_robin() {
        let active: Vec<Vec<u8>> = (0..3u8).map(|i| vec![i]).collect();
        // head at abs slot 0: slot 1 -> witness[1], slot 2 -> witness[2], slot 3 -> witness[0]
        assert_eq!(scheduled_witness(&active, 0, 1), Some(&vec![1]));
        assert_eq!(scheduled_witness(&active, 0, 2), Some(&vec![2]));
        assert_eq!(scheduled_witness(&active, 0, 3), Some(&vec![0]));
        // head offset shifts the schedule
        assert_eq!(scheduled_witness(&active, 5, 1), Some(&vec![0])); // (5+1)%3=0
        // empty set -> None
        assert_eq!(scheduled_witness(&[], 0, 1), None);
    }

    #[test]
    fn maintenance_scheduling() {
        let interval = MAINTENANCE_PERIOD_MS as i64;
        let cur = 1_000_000_000;
        // block just after current maintenance -> next is one interval on
        assert_eq!(next_maintenance_time(cur, cur + 1), cur + interval);
        // block 2.5 intervals ahead -> snaps to the 3rd interval boundary
        assert_eq!(
            next_maintenance_time(cur, cur + interval * 5 / 2),
            cur + interval * 3
        );
        assert!(need_maintenance(cur + interval, cur + interval));
        assert!(!need_maintenance(cur + interval, cur + interval - 1));
    }
}
