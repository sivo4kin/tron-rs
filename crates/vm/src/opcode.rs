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
    // Tron-specific (0xd0..=0xdf) — authoritative java-tron `Op.java:242-257`.
    CallToken = 0xd0,
    TokenBalance = 0xd1,
    CallTokenValue = 0xd2,
    CallTokenId = 0xd3,
    IsContract = 0xd4,
    Freeze = 0xd5,
    Unfreeze = 0xd6,
    FreezeExpireTime = 0xd7,
    VoteWitness = 0xd8,
    WithdrawReward = 0xd9,
    FreezeBalanceV2 = 0xda,
    UnfreezeBalanceV2 = 0xdb,
    CancelAllUnfreezeV2 = 0xdc,
    WithdrawExpireUnfreeze = 0xdd,
    DelegateResource = 0xde,
    UnDelegateResource = 0xdf,
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
            0xd5 => Freeze,
            0xd6 => Unfreeze,
            0xd7 => FreezeExpireTime,
            0xd8 => VoteWitness,
            0xd9 => WithdrawReward,
            0xda => FreezeBalanceV2,
            0xdb => UnfreezeBalanceV2,
            0xdc => CancelAllUnfreezeV2,
            0xdd => WithdrawExpireUnfreeze,
            0xde => DelegateResource,
            0xdf => UnDelegateResource,
            0xf1 => Call,
            0xf3 => Return,
            0xfd => Revert,
            0xfe => Invalid,
            0xff => SelfDestruct,
            _ => return None,
        })
    }

    /// Whether this is a Tron-specific opcode (`0xd0..=0xdf`, java-tron `Op.java`).
    pub fn is_tron_specific(self) -> bool {
        (0xd0..=0xdf).contains(&(self as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tron_opcode_table_matches_java_op_java() {
        // Authoritative java-tron Op.java:242-257 (0xd0..=0xdf). Every entry must
        // have the right discriminant, decode from its byte, and be tron-specific.
        let table: [(u8, OpCode); 16] = [
            (0xd0, OpCode::CallToken),
            (0xd1, OpCode::TokenBalance),
            (0xd2, OpCode::CallTokenValue),
            (0xd3, OpCode::CallTokenId),
            (0xd4, OpCode::IsContract),
            (0xd5, OpCode::Freeze),
            (0xd6, OpCode::Unfreeze),
            (0xd7, OpCode::FreezeExpireTime),
            (0xd8, OpCode::VoteWitness),
            (0xd9, OpCode::WithdrawReward),
            (0xda, OpCode::FreezeBalanceV2),
            (0xdb, OpCode::UnfreezeBalanceV2),
            (0xdc, OpCode::CancelAllUnfreezeV2),
            (0xdd, OpCode::WithdrawExpireUnfreeze),
            (0xde, OpCode::DelegateResource),
            (0xdf, OpCode::UnDelegateResource),
        ];
        for (byte, op) in table {
            assert_eq!(op as u8, byte, "{op:?} must have discriminant {byte:#x}");
            assert_eq!(OpCode::from_u8(byte), Some(op), "0x{byte:x} must decode to {op:?}");
            assert!(op.is_tron_specific(), "{op:?} must be tron-specific");
        }
        // The phantom fix: 0xda is FREEZEBALANCEV2, not the old (nonexistent) opcode.
        assert_eq!(OpCode::from_u8(0xda), Some(OpCode::FreezeBalanceV2));
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
