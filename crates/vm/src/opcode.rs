//! TVM opcodes.
//!
//! The standard EVM set plus Tron's custom opcodes `0xd0..=0xdb` (CALLTOKEN,
//! TOKENBALANCE, STAKE, UNSTAKE, WITHDRAWREWARD, ISWITNESS, …). Values match
//! java-tron `org.tron.core.vm.Op`. This is the reference table both the
//! clean-room interpreter and any revm adaptation build against (SPEC section 5.2).

/// A TVM opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    Stop = 0x00,
    Add = 0x01,
    Mul = 0x02,
    Sub = 0x03,
    Div = 0x04,
    Sdiv = 0x05,
    Mod = 0x06,
    Exp = 0x0a,
    Lt = 0x10,
    Gt = 0x11,
    Eq = 0x14,
    IsZero = 0x15,
    And = 0x16,
    Or = 0x17,
    Xor = 0x18,
    CallDataLoad = 0x35,
    CallDataSize = 0x36,
    CallDataCopy = 0x37,
    ReturnDataSize = 0x3d,
    ReturnDataCopy = 0x3e,
    Pop = 0x50,
    Mload = 0x51,
    Mstore = 0x52,
    Mstore8 = 0x53,
    Sload = 0x54,
    Sstore = 0x55,
    Jump = 0x56,
    Jumpi = 0x57,
    Jumpdest = 0x5b,
    Push1 = 0x60,
    Push2 = 0x61,
    Dup1 = 0x80,
    Swap1 = 0x90,
    // Tron-specific (0xd0..=0xdb)
    CallToken = 0xd0,
    TokenBalance = 0xd1,
    CallTokenValue = 0xd2,
    CallTokenId = 0xd3,
    IsContract = 0xd4,
    Stake = 0xd5,
    Unstake = 0xd6,
    WithdrawReward = 0xd9,
    RewardBalance = 0xd8,
    IsWitness = 0xda,
    Call = 0xf1,
    Return = 0xf3,
    Revert = 0xfd,
    Invalid = 0xfe,
    SelfDestruct = 0xff,
}

impl OpCode {
    /// Decode a byte into an opcode (`None` for unassigned bytes).
    pub fn from_u8(b: u8) -> Option<Self> {
        use OpCode::*;
        Some(match b {
            0x00 => Stop,
            0x01 => Add,
            0x02 => Mul,
            0x03 => Sub,
            0x04 => Div,
            0x05 => Sdiv,
            0x06 => Mod,
            0x0a => Exp,
            0x10 => Lt,
            0x11 => Gt,
            0x14 => Eq,
            0x15 => IsZero,
            0x16 => And,
            0x17 => Or,
            0x18 => Xor,
            0x35 => CallDataLoad,
            0x36 => CallDataSize,
            0x37 => CallDataCopy,
            0x3d => ReturnDataSize,
            0x3e => ReturnDataCopy,
            0x50 => Pop,
            0x51 => Mload,
            0x52 => Mstore,
            0x53 => Mstore8,
            0x54 => Sload,
            0x55 => Sstore,
            0x56 => Jump,
            0x57 => Jumpi,
            0x5b => Jumpdest,
            0x60 => Push1,
            0x61 => Push2,
            0x80 => Dup1,
            0x90 => Swap1,
            0xd0 => CallToken,
            0xd1 => TokenBalance,
            0xd2 => CallTokenValue,
            0xd3 => CallTokenId,
            0xd4 => IsContract,
            0xd5 => Stake,
            0xd6 => Unstake,
            0xd8 => RewardBalance,
            0xd9 => WithdrawReward,
            0xda => IsWitness,
            0xf1 => Call,
            0xf3 => Return,
            0xfd => Revert,
            0xfe => Invalid,
            0xff => SelfDestruct,
            _ => return None,
        })
    }

    /// Whether this is a Tron-specific opcode (`0xd0..=0xdb`).
    pub fn is_tron_specific(self) -> bool {
        (0xd0..=0xdb).contains(&(self as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tron_opcodes_match_java_tron_values() {
        assert_eq!(OpCode::CallToken as u8, 0xd0);
        assert_eq!(OpCode::TokenBalance as u8, 0xd1);
        assert_eq!(OpCode::IsContract as u8, 0xd4);
        assert_eq!(OpCode::WithdrawReward as u8, 0xd9);
        assert_eq!(OpCode::IsWitness as u8, 0xda);
        assert!(OpCode::CallToken.is_tron_specific());
        assert!(!OpCode::Add.is_tron_specific());
    }

    #[test]
    fn decode_roundtrips() {
        for b in [0x00u8, 0x01, 0x0a, 0x55, 0x60, 0xd0, 0xda, 0xf3, 0xff] {
            assert_eq!(OpCode::from_u8(b).unwrap() as u8, b);
        }
        assert_eq!(OpCode::from_u8(0xff).unwrap(), OpCode::SelfDestruct);
        assert!(!OpCode::SelfDestruct.is_tron_specific());
        assert_eq!(OpCode::from_u8(0x0c), None); // unassigned
    }
}
