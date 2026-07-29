//! Deterministic SR / standby vote ordering (audit **CS-JTRON-024**).
//!
//! The DPoS election ranks witnesses by vote count. When two witnesses have the
//! **same** vote count the tie-break must be fixed in code — it must never fall back
//! to hash-map or DB iteration order, or two nodes could disagree on the active set
//! and fork.
//!
//! [`rank_witnesses`] defines the total order explicitly:
//! **1) vote count descending, then 2) address bytes ascending.**
//! Addresses in an election are unique, so `(votes, address)` is a *total* order with
//! no ties left to chance: any permutation of the input yields the same ranking.
//!
//! ## Relation to java-tron (deviation, per task H04)
//! java-tron's `Manager.updateWitness` sorts by vote count descending and breaks ties
//! with `ByteString.hashCode()`, relying on the store's stable iteration order for the
//! rest. `hashCode()` is an implementation detail, not a consensus-safe key, and a
//! different DB engine could reorder equal-vote witnesses. We replace that tie-break
//! with **address bytes ascending**. RocksDB (and java-tron's own RocksDB stores)
//! iterate keys — 21-byte addresses — lexicographically ascending, so "address bytes
//! ascending" matches the effective input order a RocksDB-backed node presents; our
//! explicit sort simply makes that order guaranteed rather than incidental.

use tron_types::Address;

/// Rank witnesses for the SR / standby election by the total order
/// **(vote count descending, then address bytes ascending)**.
///
/// The returned `Vec<Address>` is fully determined by the set of `(address, votes)`
/// pairs and is independent of their order in `votes` — a shuffled input yields the
/// same ranking. Duplicate addresses (should not occur in a real election) collapse to
/// identical sort keys and stay adjacent.
pub fn rank_witnesses(votes: &[(Address, i64)]) -> Vec<Address> {
    let mut ranked: Vec<(Address, i64)> = votes.to_vec();
    ranked.sort_by(|(a_addr, a_votes), (b_addr, b_votes)| {
        // 1) vote count descending
        b_votes
            .cmp(a_votes)
            // 2) address bytes ascending (lexicographic over the 21-byte address)
            .then_with(|| a_addr.as_bytes().cmp(b_addr.as_bytes()))
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
    fn equal_votes_break_by_address_ascending() {
        // All equal votes -> order is purely address bytes ascending.
        let ranked = rank_witnesses(&[(addr(3), 5), (addr(1), 5), (addr(2), 5)]);
        assert_eq!(ranked, vec![addr(1), addr(2), addr(3)]);
    }

    #[test]
    fn ranking_is_independent_of_input_order() {
        // A fixed expected ranking; every permutation of the input must reproduce it.
        let a = (addr(10), 100);
        let b = (addr(20), 100); // ties with `a` on votes -> address breaks it (10 < 20)
        let c = (addr(5), 200); // highest votes -> first
        let d = (addr(30), 50); // lowest votes -> last
        let expected = vec![addr(5), addr(10), addr(20), addr(30)];

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
        // A large block of equal-vote witnesses always comes out address-ascending,
        // regardless of the (here reversed) input order.
        let ascending: Vec<Address> = (1u8..=20).map(addr).collect();
        let reversed_input: Vec<(Address, i64)> =
            (1u8..=20).rev().map(|b| (addr(b), 7)).collect();
        assert_eq!(rank_witnesses(&reversed_input), ascending);
    }

    #[test]
    fn empty_input_yields_empty_ranking() {
        assert!(rank_witnesses(&[]).is_empty());
    }

    #[test]
    fn votes_dominate_over_address() {
        // A high address with more votes still outranks a low address with fewer.
        let ranked = rank_witnesses(&[(addr(255), 100), (addr(1), 50)]);
        assert_eq!(ranked, vec![addr(255), addr(1)]);
    }
}
