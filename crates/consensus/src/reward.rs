//! Block-reward distribution (java-tron `MortgageService.payReward`).
//!
//! A witness's block reward is split by its **brokerage** rate (percent, default
//! 20): the brokerage share goes to the witness; the remainder is distributed to
//! its voters in proportion to vote weight. Amounts accumulate as withdrawable
//! allowance (mortgage) — this module computes the split; the caller credits it.

/// Default witness brokerage percentage (java-tron `DEFAULT_BROKERAGE = 20`).
pub const DEFAULT_BROKERAGE: i64 = 20;

/// Reward per produced block, in sun.
pub use crate::WITNESS_PAY_PER_BLOCK_SUN;

/// Split a reward `value` by `brokerage` percent into (witness cut, voter pool).
/// Mirrors java-tron: `brokerageAmount = (long)((brokerage/100.0) * value)` (trunc),
/// voter pool = `value - brokerageAmount`.
pub fn split_reward(value: i64, brokerage: i64) -> (i64, i64) {
    let brokerage = brokerage.clamp(0, 100);
    // (brokerage/100.0 * value) truncated toward zero == value*brokerage/100 for
    // non-negative value (integer math matches the double-then-cast here).
    let witness_cut = value.saturating_mul(brokerage) / 100;
    (witness_cut, value - witness_cut)
}

/// Distribute a `pool` to voters in proportion to their vote weight (integer
/// floor division; the truncation remainder stays unallocated, as in java-tron's
/// per-voter `reward = pool * voterVote / totalVote`).
pub fn distribute_to_voters(pool: i64, votes: &[(Vec<u8>, i64)]) -> Vec<(Vec<u8>, i64)> {
    let total: i128 = votes.iter().map(|(_, w)| *w as i128).sum();
    if total <= 0 || pool <= 0 {
        return votes.iter().map(|(a, _)| (a.clone(), 0)).collect();
    }
    votes
        .iter()
        .map(|(addr, weight)| {
            let share = (pool as i128 * *weight as i128 / total) as i64;
            (addr.clone(), share)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_default_brokerage() {
        // 1000 with 20% brokerage -> witness 200, voters 800
        assert_eq!(split_reward(1000, DEFAULT_BROKERAGE), (200, 800));
        // 100% -> all to witness
        assert_eq!(split_reward(1000, 100), (1000, 0));
        // 0% -> all to voters
        assert_eq!(split_reward(1000, 0), (0, 1000));
        // clamp out-of-range brokerage
        assert_eq!(split_reward(1000, 150), (1000, 0));
    }

    #[test]
    fn split_conserves_value() {
        for v in [0, 1, 999, 32_000_000, i64::MAX / 2] {
            let (w, p) = split_reward(v, 37);
            assert_eq!(w + p, v);
        }
    }

    #[test]
    fn voter_distribution_is_proportional() {
        let votes = vec![(vec![1], 30), (vec![2], 10)]; // 3:1
        let shares = distribute_to_voters(800, &votes);
        assert_eq!(shares[0].1, 600);
        assert_eq!(shares[1].1, 200);
        assert_eq!(shares[0].1 + shares[1].1, 800);
    }

    #[test]
    fn distribution_floor_leaves_remainder_unallocated() {
        // pool 100 across 3 equal voters -> 33 each, 1 sun remainder unallocated
        let votes = vec![(vec![1], 1), (vec![2], 1), (vec![3], 1)];
        let shares = distribute_to_voters(100, &votes);
        let total: i64 = shares.iter().map(|(_, s)| *s).sum();
        assert_eq!(shares.iter().map(|(_, s)| *s).collect::<Vec<_>>(), vec![33, 33, 33]);
        assert_eq!(total, 99); // 1 sun truncation remainder
    }

    #[test]
    fn zero_total_votes_pays_nothing() {
        let votes = vec![(vec![1], 0), (vec![2], 0)];
        assert!(distribute_to_voters(800, &votes).iter().all(|(_, s)| *s == 0));
    }

    #[test]
    fn full_block_reward_split() {
        let (witness, voters) = split_reward(WITNESS_PAY_PER_BLOCK_SUN, DEFAULT_BROKERAGE);
        assert_eq!(witness, 6_400_000); // 20% of 32M
        assert_eq!(voters, 25_600_000);
    }
}
