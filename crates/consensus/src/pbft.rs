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

/// Upper bound on how far above the current head a PBFT confirmation may reference
/// before it is rejected as a flood-guard. A block this far ahead cannot plausibly
/// have quorum yet, so accepting it would only let an unprivileged peer grow the
/// confirmation map without bound. This is our bound (java-tron drops out-of-window
/// PBFT messages); it is not a specific java constant.
pub const MAX_PBFT_FUTURE_BLOCKS: i64 = 1024;

/// PBFT intake gate — audit **CS-JTRON-004**. Mirrors java-tron's `PbftMessageHandle`
/// / `PbftDataSyncHandler`, which no-op unless the `allowPBFT` dynamic property is set.
/// Returns `true` only when a prepare/commit message should be processed (and cached):
///
/// - `allow_pbft` must be on — otherwise the handlers drop the message entirely (do not
///   cache, do not forward), so an unprivileged peer cannot grow node memory with PBFT
///   traffic while PBFT is inactive;
/// - the block must be **above** the last `solidified` block (a confirmation for an
///   already-irreversible block is redundant); and
/// - the block must be within [`MAX_PBFT_FUTURE_BLOCKS`] of `head` (flood guard against
///   confirmations for blocks that cannot have quorum yet).
///
/// Follow-up: when the async p2p peer loop lands, the on-the-wire PBFT message types
/// must likewise be dropped unless `allow_pbft` — this function is the shared predicate.
pub fn accept_pbft_message(allow_pbft: bool, block_num: i64, head: i64, solidified: i64) -> bool {
    allow_pbft
        && block_num > solidified
        && block_num <= head.saturating_add(MAX_PBFT_FUTURE_BLOCKS)
}

/// Bound the confirmation cache from below: drop every entry for a block **below** the
/// `solidified` block. Those blocks are irreversible, so their confirmation counts are
/// no longer needed; pruning them keeps the map size bounded as finality advances.
pub fn prune_below_solidified(confirmations: &mut HashMap<i64, usize>, solidified: i64) {
    confirmations.retain(|&block, _| block >= solidified);
}

/// Record one distinct-SR confirmation for `block_num`, applying the
/// [`accept_pbft_message`] gate. Returns `true` if it was accepted (the per-block count
/// was incremented) or `false` if it was dropped (the map is left untouched — dropped
/// messages can never contribute to finality).
pub fn record_confirmation(
    confirmations: &mut HashMap<i64, usize>,
    allow_pbft: bool,
    block_num: i64,
    head: i64,
    solidified: i64,
) -> bool {
    if !accept_pbft_message(allow_pbft, block_num, head, solidified) {
        return false;
    }
    *confirmations.entry(block_num).or_insert(0) += 1;
    true
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

    // -- PBFT intake gating (CS-JTRON-004) --------------------------------

    #[test]
    fn message_dropped_when_pbft_disabled() {
        // allow_pbft = false -> always dropped, regardless of a valid window.
        assert!(!accept_pbft_message(false, 5, 10, 0));
        let mut conf = HashMap::new();
        assert!(!record_confirmation(&mut conf, false, 5, 10, 0));
        assert!(conf.is_empty(), "dropped message must not be cached");
    }

    #[test]
    fn nothing_finalizes_from_dropped_messages() {
        // Flood block 1 with a full quorum's worth of confirmations while PBFT is off.
        let mut conf = HashMap::new();
        for _ in 0..finality_threshold(27) {
            assert!(!record_confirmation(&mut conf, false, 1, 10, 0));
        }
        assert!(conf.is_empty());
        assert_eq!(solidified_block(&conf, 10, 27), None); // never finalizes
    }

    #[test]
    fn accepts_message_in_valid_window() {
        assert!(accept_pbft_message(true, 5, 10, 0));
        let mut conf = HashMap::new();
        assert!(record_confirmation(&mut conf, true, 5, 10, 0));
        assert!(record_confirmation(&mut conf, true, 5, 10, 0));
        assert_eq!(conf.get(&5), Some(&2)); // counts accumulate on accept
    }

    #[test]
    fn rejects_message_at_or_below_solidified() {
        // solidified = 5: confirmations for blocks <= 5 are redundant -> dropped.
        assert!(!accept_pbft_message(true, 5, 100, 5));
        assert!(!accept_pbft_message(true, 3, 100, 5));
        assert!(accept_pbft_message(true, 6, 100, 5)); // just above -> accepted
    }

    #[test]
    fn rejects_absurdly_future_block() {
        let head = 100;
        // exactly at the window edge is fine; one past it is a flood guard drop.
        assert!(accept_pbft_message(true, head + MAX_PBFT_FUTURE_BLOCKS, head, 0));
        assert!(!accept_pbft_message(true, head + MAX_PBFT_FUTURE_BLOCKS + 1, head, 0));
    }

    #[test]
    fn prune_evicts_entries_below_solidified() {
        let mut conf = HashMap::new();
        for n in 1..=5 {
            conf.insert(n, 20);
        }
        prune_below_solidified(&mut conf, 3);
        // blocks below the solidified block (3) are gone; 3 and above retained.
        assert!(!conf.contains_key(&1));
        assert!(!conf.contains_key(&2));
        assert!(conf.contains_key(&3));
        assert!(conf.contains_key(&4));
        assert!(conf.contains_key(&5));
    }

    #[test]
    fn cache_stays_bounded_as_finality_advances() {
        // Finalize 1..=3, prune below the new solid tip, and confirm the cache does
        // not retain the pruned entries (bounded growth).
        let mut conf = HashMap::new();
        for n in 1..=3 {
            for _ in 0..finality_threshold(27) {
                assert!(record_confirmation(&mut conf, true, n, 3, 0));
            }
        }
        let solid = solidified_block(&conf, 3, 27).unwrap();
        assert_eq!(solid, 3);
        prune_below_solidified(&mut conf, solid);
        assert!(!conf.contains_key(&1));
        assert!(!conf.contains_key(&2));
        assert_eq!(conf.len(), 1); // only the solid tip remains
    }
}
