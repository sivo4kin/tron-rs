//! Fork choice and reorganization (java-tron `KhaosDatabase` essence).
//!
//! Tron DPoS follows the **longest valid chain**; among equal-length heads the
//! one with the greater cumulative weight (SR confirmations / PBFT solidity)
//! wins. This module exposes the canonical-head choice and the reorg computation
//! (common ancestor + the blocks to roll back / apply) that a chain manager runs
//! when a competing branch arrives.

/// A branch head summary for fork choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// Block number (height).
    pub number: i64,
    /// Cumulative confirmation weight (PBFT/solidified count); tie-breaker.
    pub weight: u64,
}

/// Which branch is canonical: the longer chain, tie-broken by higher weight.
/// Returns `true` if `candidate` should replace `current` as canonical.
pub fn should_switch(current: Head, candidate: Head) -> bool {
    match candidate.number.cmp(&current.number) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => candidate.weight > current.weight,
    }
}

/// The result of a reorg computation: roll back `revert` (our blocks above the
/// fork, newest first) and apply `apply` (their blocks above the fork, oldest first).
#[derive(Debug, PartialEq, Eq)]
pub struct Reorg {
    pub common_ancestor: Vec<u8>,
    pub revert: Vec<Vec<u8>>,
    pub apply: Vec<Vec<u8>>,
}

/// Compute the reorg between our chain and a competing chain, each given as an
/// ascending list of block ids (genesis-first). Finds the last shared id, then
/// the divergent suffixes.
pub fn compute_reorg(ours: &[Vec<u8>], theirs: &[Vec<u8>]) -> Reorg {
    let mut fork = 0usize;
    while fork < ours.len() && fork < theirs.len() && ours[fork] == theirs[fork] {
        fork += 1;
    }
    let common_ancestor = if fork == 0 { Vec::new() } else { ours[fork - 1].clone() };
    // revert our blocks above the fork, newest first
    let mut revert: Vec<Vec<u8>> = ours[fork..].to_vec();
    revert.reverse();
    let apply: Vec<Vec<u8>> = theirs[fork..].to_vec();
    Reorg { common_ancestor, revert, apply }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_chain_wins() {
        let cur = Head { number: 10, weight: 5 };
        assert!(should_switch(cur, Head { number: 11, weight: 0 }));
        assert!(!should_switch(cur, Head { number: 9, weight: 100 }));
    }

    #[test]
    fn equal_length_breaks_on_weight() {
        let cur = Head { number: 10, weight: 5 };
        assert!(should_switch(cur, Head { number: 10, weight: 6 }));
        assert!(!should_switch(cur, Head { number: 10, weight: 5 })); // not strictly greater
        assert!(!should_switch(cur, Head { number: 10, weight: 4 }));
    }

    fn ids(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes.iter().map(|b| vec![*b]).collect()
    }

    #[test]
    fn reorg_finds_common_ancestor_and_suffixes() {
        // shared: 0,1,2 ; ours diverges to 3,4 ; theirs to 7,8,9
        let ours = ids(&[0, 1, 2, 3, 4]);
        let theirs = ids(&[0, 1, 2, 7, 8, 9]);
        let r = compute_reorg(&ours, &theirs);
        assert_eq!(r.common_ancestor, vec![2]);
        assert_eq!(r.revert, vec![vec![4], vec![3]]); // newest-first rollback
        assert_eq!(r.apply, vec![vec![7], vec![8], vec![9]]); // oldest-first apply
    }

    #[test]
    fn reorg_pure_extension_reverts_nothing() {
        let ours = ids(&[0, 1, 2]);
        let theirs = ids(&[0, 1, 2, 3, 4]);
        let r = compute_reorg(&ours, &theirs);
        assert_eq!(r.common_ancestor, vec![2]);
        assert!(r.revert.is_empty());
        assert_eq!(r.apply, vec![vec![3], vec![4]]);
    }

    #[test]
    fn reorg_no_common_history() {
        let r = compute_reorg(&ids(&[1, 2]), &ids(&[3, 4]));
        assert!(r.common_ancestor.is_empty());
        assert_eq!(r.revert, vec![vec![2], vec![1]]);
        assert_eq!(r.apply, vec![vec![3], vec![4]]);
    }
}
