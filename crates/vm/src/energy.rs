//! Energy (gas) cost table — java-tron `org.tron.core.vm.EnergyCost`.
//!
//! Tron meters execution in **energy**; per-op tier costs match EnergyCost's
//! constants exactly. Dynamic costs (memory expansion, SSTORE set/clear, copy
//! words) are computed by helpers, mirroring java-tron's formulas.

use crate::opcode::OpCode;

// Static tiers (EnergyCost.java).
pub const ZERO_TIER: u64 = 0;
pub const BASE_TIER: u64 = 2;
pub const VERY_LOW_TIER: u64 = 3;
pub const LOW_TIER: u64 = 5;
pub const MID_TIER: u64 = 8;
pub const HIGH_TIER: u64 = 10;

// Named dynamic-base costs.
pub const EXP_ENERGY: u64 = 10;
pub const EXP_BYTE_ENERGY: u64 = 10;
pub const SLOAD: u64 = 50;
pub const SET_SSTORE: u64 = 20_000;
pub const CLEAR_SSTORE: u64 = 5_000;
pub const RESET_SSTORE: u64 = 5_000;
pub const MEMORY_WORD: u64 = 3;
pub const COPY_WORD: u64 = 3;

/// Energy charged when a system op implicitly creates a new account
/// (java-tron `EnergyCost.NEW_ACCT_CALL`). Added by CALL and by SELFDESTRUCT
/// (`getSuicideCost2`) when the destination/inheritor account does not yet exist.
pub const NEW_ACCT_CALL: u64 = 25_000;
/// Base energy for `SELFDESTRUCT` (java-tron `EnergyCost` suicide base). Tron's base
/// is 0; the only variable part is the [`NEW_ACCT_CALL`] surcharge below.
pub const SUICIDE: u64 = 0;

/// `SELFDESTRUCT` energy cost (java-tron `EnergyCost.getSuicideCost2`, proposal #91):
/// the suicide base plus [`NEW_ACCT_CALL`] when the beneficiary (inheritor) account
/// does not exist — the same new-account surcharge CALL pays when it creates the
/// callee. Audit CS-JTRON-002.
pub fn suicide_cost(beneficiary_exists: bool) -> u64 {
    SUICIDE + if beneficiary_exists { 0 } else { NEW_ACCT_CALL }
}

/// Base energy for `VOTEWITNESS` (java-tron `EnergyCost.VOTE_WITNESS`).
pub const VOTE_WITNESS: u64 = 30_000;

/// Memory extent (bytes) a `VOTEWITNESS` access requires, per java-tron
/// `EnergyCost.getVoteWitnessCost2`: each of the witness and amount arrays occupies
/// `offset + length*32 + 32`. The trailing **`+32` is the array's size word**, which
/// the opcode reads even when `length == 0`. The pre-fix `getVoteWitnessCost` omitted
/// it (`memNeeded` returns 0 for a zero-length array), so a zero-length array priced 0
/// memory regardless of offset — audit **CS-JTRON-005**. Returns the larger of the two
/// arrays' extents.
pub fn vote_witness_mem_needed(
    witness_offset: u64,
    witness_len: u64,
    amount_offset: u64,
    amount_len: u64,
) -> u64 {
    let extent = |off: u64, len: u64| {
        off.saturating_add(len.saturating_mul(32)).saturating_add(32)
    };
    extent(witness_offset, witness_len).max(extent(amount_offset, amount_len))
}

/// `VOTEWITNESS` energy (java-tron `EnergyCost.getVoteWitnessCost2`): the
/// [`VOTE_WITNESS`] base plus the word-aligned memory-expansion energy to reach
/// [`vote_witness_mem_needed`] from the current memory size. Because the extent always
/// includes the size word (see above), a zero-length array at a high offset is charged
/// for that word rather than being free (CS-JTRON-005).
///
/// TODO(feature-gate): java applies this fixed formula only when
/// `ALLOW_ENERGY_ADJUSTMENT` is on (proposal #91); before the gate it used
/// `getVoteWitnessCost` (no size-word charge). Wire the gate via I03 when it lands;
/// until then the fixed pricing is always applied.
pub fn vote_witness_cost(
    old_mem_bytes: u64,
    witness_offset: u64,
    witness_len: u64,
    amount_offset: u64,
    amount_len: u64,
) -> u64 {
    let needed = vote_witness_mem_needed(witness_offset, witness_len, amount_offset, amount_len);
    let new_words = (needed + 31) / 32; // round up to word boundary
    let old_words = old_mem_bytes / 32;
    VOTE_WITNESS + memory_expansion_cost(old_words, new_words)
}

/// Fixed per-opcode energy tier (java-tron `EnergyCost` static portion). Opcodes
/// whose cost is purely dynamic return their base here and add the dynamic part
/// separately.
pub fn base_cost(op: OpCode) -> u64 {
    use OpCode::*;
    match op {
        Stop | Return | Revert | Invalid => ZERO_TIER,
        Add | Sub | Lt | Gt | Eq | IsZero | And | Or | Xor | Pop => VERY_LOW_TIER,
        Mul | Div | Sdiv | Mod => LOW_TIER,
        Push1 | Push2 | Dup1 | Swap1 => VERY_LOW_TIER,
        Jumpdest => 1,
        Jump => MID_TIER,
        Jumpi => HIGH_TIER,
        Mload | Mstore | Mstore8 => VERY_LOW_TIER,
        CallDataLoad => VERY_LOW_TIER,
        CallDataSize | ReturnDataSize => BASE_TIER,
        CallDataCopy | ReturnDataCopy => VERY_LOW_TIER,
        Exp => EXP_ENERGY,
        Sload => SLOAD,
        Sstore => 0, // computed by sstore_cost
        SelfDestruct => SUICIDE, // dynamic surcharge computed by suicide_cost
        VoteWitness => ZERO_TIER, // full cost computed by vote_witness_cost
        // Tron-specific: costs are context-dependent; base tier per java-tron.
        IsContract | TokenBalance | CallTokenValue | CallTokenId => BASE_TIER,
        Call => 40,       // CALL_ENERGY base (java-tron CALL_ENERGY); dynamic parts added in-op
        CallToken => 40, // CALL_ENERGY tier
        // Stake/freeze/delegate family (java-tron Op.java 0xd5..=0xdf). Naming/decode
        // only for now (H06); execution semantics land in follow-ups. Static base tier.
        Freeze | Unfreeze | FreezeExpireTime | WithdrawReward | FreezeBalanceV2
        | UnfreezeBalanceV2 | CancelAllUnfreezeV2 | WithdrawExpireUnfreeze
        | DelegateResource | UnDelegateResource => BASE_TIER,
    }
}

/// SSTORE cost (java-tron simplified net-gas metering): setting a zero slot to
/// non-zero costs `SET_SSTORE`; clearing costs `CLEAR_SSTORE`; otherwise `RESET_SSTORE`.
pub fn sstore_cost(current_is_zero: bool, new_is_zero: bool) -> u64 {
    match (current_is_zero, new_is_zero) {
        (true, false) => SET_SSTORE,
        (false, true) => CLEAR_SSTORE,
        _ => RESET_SSTORE,
    }
}

/// Memory-expansion cost to grow to `new_words` 32-byte words from `cur_words`
/// (java-tron uses the same quadratic term as the EVM: 3*w + w^2/512).
pub fn memory_expansion_cost(cur_words: u64, new_words: u64) -> u64 {
    if new_words <= cur_words {
        return 0;
    }
    let cost = |w: u64| MEMORY_WORD * w + w * w / 512;
    cost(new_words) - cost(cur_words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_match_java_tron() {
        assert_eq!(base_cost(OpCode::Add), 3);
        assert_eq!(base_cost(OpCode::Mul), 5);
        assert_eq!(base_cost(OpCode::Sload), 50);
        assert_eq!(base_cost(OpCode::Stop), 0);
        assert_eq!(base_cost(OpCode::Jumpi), 10);
    }

    #[test]
    fn sstore_transitions() {
        assert_eq!(sstore_cost(true, false), 20_000); // set
        assert_eq!(sstore_cost(false, true), 5_000); // clear
        assert_eq!(sstore_cost(false, false), 5_000); // reset
    }

    #[test]
    fn suicide_charges_new_account_only_when_beneficiary_absent() {
        // existing beneficiary -> just the (zero) base
        assert_eq!(suicide_cost(true), SUICIDE);
        // absent beneficiary -> base + NEW_ACCT_CALL surcharge
        assert_eq!(suicide_cost(false), SUICIDE + NEW_ACCT_CALL);
        assert_eq!(NEW_ACCT_CALL, 25_000);
        // the surcharge is exactly the extra cost
        assert_eq!(suicide_cost(false) - suicide_cost(true), NEW_ACCT_CALL);
        assert_eq!(base_cost(OpCode::SelfDestruct), SUICIDE);
    }

    #[test]
    fn vote_witness_prices_size_word_even_for_zero_length_array() {
        // CS-JTRON-005: a zero-length array at a HIGH offset must still pay for the
        // size-word memory expansion — it is NOT free.
        let high = vote_witness_cost(0, 10_000, 0, 0, 0);
        assert!(high > VOTE_WITNESS, "zero-length @ high offset must exceed the base");
        // The buggy pre-fix formula (no +32 size word) would price 0 memory for a
        // zero-length array -> exactly VOTE_WITNESS. Prove we charge strictly more.
        assert_ne!(high, VOTE_WITNESS);

        // Even at offset 0, a zero-length array reads one size word (32 bytes = 1 word).
        // expansion(0 -> 1 word) = 3*1 + 1/512 = 3.
        assert_eq!(vote_witness_cost(0, 0, 0, 0, 0), VOTE_WITNESS + 3);
    }

    #[test]
    fn vote_witness_mem_needed_includes_the_size_word() {
        // witness array dominates: 1000 + 0*32 + 32 = 1032.
        assert_eq!(vote_witness_mem_needed(1000, 0, 0, 0), 1032);
        // amount array dominates: 2000 + 0*32 + 32 = 2032.
        assert_eq!(vote_witness_mem_needed(0, 0, 2000, 0), 2032);
        // length contributes 32 per element plus the one size word.
        assert_eq!(vote_witness_mem_needed(0, 2, 0, 0), 2 * 32 + 32);
    }

    #[test]
    fn vote_witness_normal_case_matches_java_formula() {
        // Both arrays: offset 0, length 2 -> needed = 0 + 64 + 32 = 96 bytes = 3 words.
        // calcMemEnergy(0, 96) = 3*3 + 3*3/512 - 0 = 9. Total = VOTE_WITNESS + 9.
        assert_eq!(vote_witness_cost(0, 0, 2, 0, 2), VOTE_WITNESS + 9);
    }

    #[test]
    fn memory_expansion_is_quadratic_and_monotone() {
        assert_eq!(memory_expansion_cost(0, 0), 0);
        assert_eq!(memory_expansion_cost(3, 1), 0); // shrink -> free
        // 1 word: 3*1 + 1/512 = 3
        assert_eq!(memory_expansion_cost(0, 1), 3);
        // grow 0->10 costs more than 0->1
        assert!(memory_expansion_cost(0, 10) > memory_expansion_cost(0, 1));
    }
}
