//! Deterministic SR / standby witness ordering (audit **CS-JTRON-024**).
//!
//! The DPoS election ranks witnesses by vote count. When two witnesses have the
//! **same** vote count the tie-break must be fixed in code — it must never fall back
//! to hash-map or DB iteration order, or two nodes could disagree on the active set
//! and fork.
//!
//! [`cmp_witness`] is the single canonical comparator (used by both this module's
//! [`rank_witnesses`] and `tron_actuators::maintenance`), defining the total order:
//! **1) vote count descending, then 2) address bytes descending.**
//! Addresses in an election are unique, so `(votes, address)` is a *total* order with
//! no ties left to chance: any permutation of the input yields the same ranking.
//!
//! ## Relation to java-tron
//! This matches java-tron `WitnessStore.sortWitnesses` under
//! `allowWitnessSortOptimization` (the mainnet-activated path), which sorts by
//! `voteCount` descending then by `ByteArray.toHexString(address)` **descending**.
//! For fixed-length 21-byte addresses, descending hex-string order is identical to
//! descending raw-byte order, so we compare the raw address bytes directly. The
//! pre-optimization java path broke ties with `ByteString.hashCode()` — an
//! implementation detail with only 4 bytes of entropy that CS-JTRON-024 flagged as
//! not consensus-safe; we never use it.

use std::cmp::Ordering;
use tron_types::Address;

/// Canonical java-tron witness ordering comparator: **vote count descending, then
/// address bytes descending** (java `WitnessStore.sortWitnesses`, sort-opt on).
///
/// Each argument is `(address_bytes, vote_count)`. Deterministic — no `hashCode` or
/// store-iteration dependence. For equal-length addresses, byte-descending equals
/// java's hex-string-descending tie-break.
pub fn cmp_witness(a: (&[u8], i64), b: (&[u8], i64)) -> Ordering {
    // 1) vote count descending
    b.1.cmp(&a.1)
        // 2) address bytes descending (== java's toHexString(addr) reversed)
        .then_with(|| b.0.cmp(a.0))
}

/// Rank witnesses for the SR / standby election by [`cmp_witness`]
/// **(vote count descending, then address bytes descending)**.
///
/// The returned `Vec<Address>` is fully determined by the set of `(address, votes)`
/// pairs and is independent of their order in `votes` — a shuffled input yields the
/// same ranking. Duplicate addresses (should not occur in a real election) collapse to
/// identical sort keys and stay adjacent.
pub fn rank_witnesses(votes: &[(Address, i64)]) -> Vec<Address> {
    let mut ranked: Vec<(Address, i64)> = votes.to_vec();
    ranked.sort_by(|(a_addr, a_votes), (b_addr, b_votes)| {
        cmp_witness((a_addr.as_bytes(), *a_votes), (b_addr.as_bytes(), *b_votes))
    });
    ranked.into_iter().map(|(addr, _)| addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    #[test]
    fn sorts_by_votes_descending() {
        let ranked = rank_witnesses(&[(addr(1), 10), (addr(2), 30), (addr(3), 20)]);
        assert_eq!(ranked, vec![addr(2), addr(3), addr(1)]);
    }

    #[test]
    fn equal_votes_break_by_address_descending() {
        // All equal votes -> order is purely address bytes descending (java sort-opt).
        let ranked = rank_witnesses(&[(addr(3), 5), (addr(1), 5), (addr(2), 5)]);
        assert_eq!(ranked, vec![addr(3), addr(2), addr(1)]);
    }

    #[test]
    fn ranking_is_independent_of_input_order() {
        // A fixed expected ranking; every permutation of the input must reproduce it.
        let a = (addr(10), 100);
        let b = (addr(20), 100); // ties with `a` on votes -> address DESC breaks it (20 > 10)
        let c = (addr(5), 200); // highest votes -> first
        let d = (addr(30), 50); // lowest votes -> last
        let expected = vec![addr(5), addr(20), addr(10), addr(30)];

        let inputs = [
            vec![a, b, c, d],
            vec![d, c, b, a],
            vec![b, d, a, c],
            vec![c, a, d, b],
            vec![b, a, d, c],
        ];
        for input in inputs {
            assert_eq!(rank_witnesses(&input), expected, "input {input:?} misranked");
        }
    }

    #[test]
    fn shuffled_equal_vote_block_is_stable() {
        // A large block of equal-vote witnesses always comes out address-descending,
        // regardless of the (here ascending) input order.
        let descending: Vec<Address> = (1u8..=20).rev().map(addr).collect();
        let ascending_input: Vec<(Address, i64)> =
            (1u8..=20).map(|b| (addr(b), 7)).collect();
        assert_eq!(rank_witnesses(&ascending_input), descending);
    }

    #[test]
    fn empty_input_yields_empty_ranking() {
        assert!(rank_witnesses(&[]).is_empty());
    }

    #[test]
    fn votes_dominate_over_address() {
        // A low address with more votes still outranks a high address with fewer.
        let ranked = rank_witnesses(&[(addr(1), 100), (addr(255), 50)]);
        assert_eq!(ranked, vec![addr(1), addr(255)]);
    }

    #[test]
    fn matches_java_sort_opt_tie_break() {
        // java WitnessStore.sortWitnesses (opt on): voteCount desc, hex-address desc.
        // 0xff before 0x01 at equal votes (higher hex/byte first).
        let lo = Address::from_body([0x01; 20]);
        let hi = Address::from_body([0xff; 20]);
        let ranked = rank_witnesses(&[(lo, 100), (hi, 100)]);
        assert_eq!(ranked, vec![hi, lo]);
    }
}
