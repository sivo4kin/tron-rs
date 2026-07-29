//! Single-contract execution entry over a persistent [`Host`] — the actuator storage
//! path (`StateHost`).
//!
//! Since T01 this is a thin adapter over the ONE unified engine ([`crate::frame`]): it
//! binds the `Host` as the storage backend of a fresh `World` and runs the engine loop.
//! So this path shares the exact memory / opcode / energy model with the multi-account
//! engine — and now genuinely has EVM memory + calldata. Inter-contract CALL here
//! reaches precompiles only (a single `Host` exposes no other contract's code), and
//! SELFDESTRUCT is typed-unsupported on this path (halts `BadOpcode(0xff)`).

use crate::engine::run_frame;
use crate::frame::{Halt as EngineHalt, World};
use primitive_types::U256;

/// Contract state access (persistent storage). SLOAD/SSTORE go through this.
pub trait Host {
    fn sload(&self, key: U256) -> U256;
    fn sstore(&mut self, key: U256, value: U256);
}

/// An in-memory [`Host`] for tests / isolated execution.
#[derive(Default)]
pub struct MemoryHost {
    storage: std::collections::HashMap<U256, U256>,
}

impl Host for MemoryHost {
    fn sload(&self, key: U256) -> U256 {
        self.storage.get(&key).copied().unwrap_or_default()
    }
    fn sstore(&mut self, key: U256, value: U256) {
        if value.is_zero() {
            self.storage.remove(&key);
        } else {
            self.storage.insert(key, value);
        }
    }
}

/// Why single-contract execution stopped (the public, narrower surface actuators use).
#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    Stop,
    Return,
    Revert,
    OutOfEnergy,
    StackUnderflow,
    StackOverflow,
    BadOpcode(u8),
    BadJump,
}

/// EVM stack depth limit.
pub const STACK_LIMIT: usize = 1024;

#[derive(Debug)]
pub struct Outcome {
    pub halt: Halt,
    pub stack_top: Option<U256>,
    pub energy_used: u64,
    /// Bytes produced by a `RETURN`/`REVERT` (memory[off..off+len]), else empty.
    pub return_data: Vec<u8>,
}

/// Run `code` with an energy `limit` against `host` (no calldata).
pub fn run(code: &[u8], limit: u64, host: &mut dyn Host) -> Outcome {
    run_with_input(code, &[], limit, host)
}

/// Run `code` with `calldata` and an energy `limit` against `host`.
pub fn run_with_input(code: &[u8], calldata: &[u8], limit: u64, host: &mut dyn Host) -> Outcome {
    // The single executing contract's storage routes to the host regardless of address,
    // so this sentinel address is immaterial.
    const SELF_ADDR: [u8; 20] = [0u8; 20];
    let mut world = World::with_host(host);
    let e = run_frame(&mut world, &SELF_ADDR, code, calldata, limit, 0);
    Outcome {
        halt: map_halt(e.halt),
        stack_top: e.stack_top,
        energy_used: e.energy_used,
        return_data: e.return_data,
    }
}

fn map_halt(h: EngineHalt) -> Halt {
    match h {
        // SelfDestruct / DepthLimit cannot occur on the single-contract host path
        // (SELFDESTRUCT is unsupported there; depth starts at 0). Mapped defensively.
        EngineHalt::Stop | EngineHalt::SelfDestruct => Halt::Stop,
        EngineHalt::Return => Halt::Return,
        EngineHalt::Revert => Halt::Revert,
        EngineHalt::OutOfEnergy | EngineHalt::DepthLimit => Halt::OutOfEnergy,
        EngineHalt::StackUnderflow => Halt::StackUnderflow,
        EngineHalt::StackOverflow => Halt::StackOverflow,
        EngineHalt::BadOpcode(b) => Halt::BadOpcode(b),
        EngineHalt::BadJump => Halt::BadJump,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcode::OpCode::*;

    fn run_mem(code: &[u8], limit: u64) -> Outcome {
        run(code, limit, &mut MemoryHost::default())
    }

    #[test]
    fn u256_arithmetic() {
        let out = run_mem(&[Push1 as u8, 2, Push1 as u8, 3, Add as u8, Stop as u8], 1000);
        assert_eq!(out.stack_top, Some(U256::from(5)));
    }

    #[test]
    fn u256_overflow_wraps() {
        let code = [Push1 as u8, 255, Push1 as u8, 2, Exp as u8, Push1 as u8, 2, Mul as u8, Stop as u8];
        assert_eq!(run_mem(&code, 100000).stack_top, Some(U256::zero()));
    }

    #[test]
    fn sstore_then_sload_persists_and_meters() {
        let code = [
            Push1 as u8, 42, Push1 as u8, 7, Sstore as u8,
            Push1 as u8, 7, Sload as u8, Stop as u8,
        ];
        let mut host = MemoryHost::default();
        let out = run(&code, 100000, &mut host);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(U256::from(42)));
        assert_eq!(host.sload(U256::from(7)), U256::from(42));
        assert!(out.energy_used >= 20000);
    }

    #[test]
    fn sstore_out_of_energy_before_write() {
        let code = [Push1 as u8, 42, Push1 as u8, 7, Sstore as u8];
        let mut host = MemoryHost::default();
        let out = run(&code, 100, &mut host);
        assert_eq!(out.halt, Halt::OutOfEnergy);
        assert_eq!(host.sload(U256::from(7)), U256::zero());
    }

    #[test]
    fn clearing_a_slot_costs_less_than_setting() {
        let set = [Push1 as u8, 1, Push1 as u8, 3, Sstore as u8, Stop as u8];
        let clear = [Push1 as u8, 0, Push1 as u8, 3, Sstore as u8, Stop as u8];
        let mut host = MemoryHost::default();
        let e_set = run(&set, 100000, &mut host).energy_used;
        let e_clear = run(&clear, 100000, &mut host).energy_used;
        assert!(e_set > e_clear, "set {e_set} should exceed clear {e_clear}");
        assert!(host.sload(U256::from(3)).is_zero());
    }

    #[test]
    fn loop_with_jumpi_counts_down() {
        let code = [
            Push1 as u8, 1, Push1 as u8, 7, Jumpi as u8,
            Push1 as u8, 0xff,
            Jumpdest as u8, Push1 as u8, 9, Stop as u8,
        ];
        assert_eq!(run_mem(&code, 1000).halt, Halt::Stop);
    }

    #[test]
    fn mstore_then_mload_roundtrips_and_charges_expansion() {
        let code = [Push1 as u8, 42, Push1 as u8, 0, Mstore as u8, Push1 as u8, 0, Mload as u8, Stop as u8];
        let out = run_mem(&code, 100000);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(U256::from(42)));
        assert!(out.energy_used > 0);
    }

    #[test]
    fn calldataload_reads_input_word() {
        let code = [Push1 as u8, 0, CallDataLoad as u8, Stop as u8];
        let mut calldata = [0u8; 32];
        calldata[31] = 7;
        let out = run_with_input(&code, &calldata, 100000, &mut MemoryHost::default());
        assert_eq!(out.stack_top, Some(U256::from(7)));
    }

    #[test]
    fn calldatasize_reports_input_length() {
        let code = [CallDataSize as u8, Stop as u8];
        let out = run_with_input(&code, &[0u8; 36], 1000, &mut MemoryHost::default());
        assert_eq!(out.stack_top, Some(U256::from(36)));
    }

    #[test]
    fn calldatacopy_into_memory_then_mload() {
        let code = [
            Push1 as u8, 32, Push1 as u8, 0, Push1 as u8, 0, CallDataCopy as u8,
            Push1 as u8, 0, Mload as u8, Stop as u8,
        ];
        let mut calldata = [0u8; 32];
        calldata[31] = 99;
        let out = run_with_input(&code, &calldata, 100000, &mut MemoryHost::default());
        assert_eq!(out.stack_top, Some(U256::from(99)));
    }

    #[test]
    fn call_to_sha256_precompile_from_bytecode() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let code = [
            p(Push1), 0x61, p(Push1), 0, p(Mstore8),
            p(Push1), 0x62, p(Push1), 1, p(Mstore8),
            p(Push1), 0x63, p(Push1), 2, p(Mstore8),
            p(Push1), 32, p(Push1), 32, p(Push1), 3, p(Push1), 0,
            p(Push1), 0, p(Push1), 2, p(Push1), 0, p(Call),
            p(Push1), 32, p(Mload), p(Stop),
        ];
        let out = run_mem(&code, 1_000_000);
        assert_eq!(out.halt, Halt::Stop);
        let expected = U256::from_big_endian(&tron_crypto::sha256(b"abc"));
        assert_eq!(out.stack_top, Some(expected));
    }

    #[test]
    fn return_captures_memory_output() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let code = [p(Push1), 0x2a, p(Push1), 0, p(Mstore8), p(Push1), 1, p(Push1), 0, p(Return)];
        let out = run_mem(&code, 100000);
        assert_eq!(out.halt, Halt::Return);
        assert_eq!(out.return_data, vec![0x2a]);
    }

    #[test]
    fn returndatasize_and_copy_after_call() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let code = [
            p(Push1), 32, p(Push1), 64, p(Push1), 0, p(Push1), 0,
            p(Push1), 0, p(Push1), 2, p(Push1), 0, p(Call),
            p(Pop),
            p(ReturnDataSize), p(Stop),
        ];
        let out = run_mem(&code, 1_000_000);
        assert_eq!(out.stack_top, Some(U256::from(32)));
    }

    #[test]
    fn call_to_non_precompile_pushes_zero() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let code = [
            p(Push1), 0, p(Push1), 0, p(Push1), 0, p(Push1), 0,
            p(Push1), 0, p(Push1), 0x99, p(Push1), 0, p(Call), p(Stop),
        ];
        assert_eq!(run_mem(&code, 100000).stack_top, Some(U256::zero()));
    }

    #[test]
    fn votewitness_prices_size_word_at_high_offset() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let high = [
            p(Push2), 0x27, 0x10,
            p(Push1), 0,
            p(Push1), 0,
            p(Push1), 0,
            p(VoteWitness), p(Stop),
        ];
        let low = [
            p(Push1), 0,
            p(Push1), 0,
            p(Push1), 0,
            p(Push1), 0,
            p(VoteWitness), p(Stop),
        ];
        let out_high = run_mem(&high, 1_000_000);
        let out_low = run_mem(&low, 1_000_000);
        assert_eq!(out_high.halt, Halt::Stop);
        assert_eq!(out_low.halt, Halt::Stop);
        assert!(out_high.energy_used > crate::energy::VOTE_WITNESS);
        assert!(out_low.energy_used > crate::energy::VOTE_WITNESS);
        assert!(out_high.energy_used > out_low.energy_used);
        assert_eq!(out_high.stack_top, Some(U256::zero()));
    }

    #[test]
    fn votewitness_out_of_energy_on_base() {
        let p = |op: crate::opcode::OpCode| op as u8;
        let code = [
            p(Push1), 0, p(Push1), 0, p(Push1), 0, p(Push1), 0, p(VoteWitness), p(Stop),
        ];
        let out = run_mem(&code, 100);
        assert_eq!(out.halt, Halt::OutOfEnergy);
    }

    #[test]
    fn stack_overflow_guarded() {
        let mut code = vec![Push1 as u8, 1];
        for _ in 0..STACK_LIMIT + 5 {
            code.push(Dup1 as u8);
        }
        assert_eq!(run_mem(&code, 10_000_000).halt, Halt::StackOverflow);
    }
}
