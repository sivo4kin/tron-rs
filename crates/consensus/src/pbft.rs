//! PBFT block finality (java-tron `PbftManager` / solidified-block advance).
//!
//! On top of DPoS, a block becomes **irreversible** once more than 2/3 of the
//! active SRs have confirmed it (java-tron requires `> 2/3 * activeWitnessNum`
//! agreeing prepare/commit messages). This module computes the finality
//! threshold and advances the solidified block number from per-block confirmations.

use crate::MAX_ACTIVE_WITNESSES;
use std::collections::HashMap;

/// The minimum confirmations for finality: strictly more than 2/3 of `total` SRs,
/// i.e. `floor(2*total/3) + 1` (java-tron `SolidNode` / PBFT quorum).
pub fn finality_threshold(total: usize) -> usize {
    (2 * total) / 3 + 1
}

/// Threshold for the full active set of 27 SRs (= 19).
pub fn default_threshold() -> usize {
    finality_threshold(MAX_ACTIVE_WITNESSES)
}

/// Whether a block with `confirmations` distinct SR confirmations is finalized.
pub fn is_finalized(confirmations: usize, total_srs: usize) -> bool {
    confirmations >= finality_threshold(total_srs)
}

/// Given per-block confirmation counts (block number -> distinct SRs) and the
/// active SR count, return the highest **contiguous** finalized block number at or
/// below `head` (finality cannot skip a gap). `None` if nothing is finalized.
pub fn solidified_block(
    confirmations: &HashMap<i64, usize>,
    head: i64,
    total_srs: usize,
) -> Option<i64> {
    let mut solid = None;
    let mut n = 1;
    while n <= head {
        match confirmations.get(&n) {
            Some(&c) if is_finalized(c, total_srs) => solid = Some(n),
            _ => break, // gap: finality stops here
        }
        n += 1;
    }
    solid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_two_thirds_plus_one() {
        assert_eq!(finality_threshold(27), 19); // 2*27/3 + 1 = 18+1
        assert_eq!(finality_threshold(3), 3); // 2 + 1
        assert_eq!(default_threshold(), 19);
    }

    #[test]
    fn finalization_at_threshold() {
        assert!(!is_finalized(18, 27)); // just under
        assert!(is_finalized(19, 27)); // exactly quorum
        assert!(is_finalized(27, 27)); // unanimous
    }

    #[test]
    fn solidified_advances_contiguously() {
        let mut conf = HashMap::new();
        conf.insert(1, 20);
        conf.insert(2, 19);
        conf.insert(3, 25);
        assert_eq!(solidified_block(&conf, 3, 27), Some(3));
    }

    #[test]
    fn solidified_stops_at_a_gap() {
        let mut conf = HashMap::new();
        conf.insert(1, 20);
        conf.insert(2, 10); // below quorum -> not final
        conf.insert(3, 25); // finalized but unreachable past the gap
        assert_eq!(solidified_block(&conf, 3, 27), Some(1));
    }

    #[test]
    fn nothing_finalized_yet() {
        let mut conf = HashMap::new();
        conf.insert(1, 5);
        assert_eq!(solidified_block(&conf, 5, 27), None);
    }
}
