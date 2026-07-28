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
}
