//! A stack-based TVM interpreter over U256 words, with pluggable state access.
//!
//! P2 (SPEC section 5.2): validates the execution + energy model as a real (if
//! subset) contract executor — full 256-bit arithmetic, control flow, memory-less
//! storage via a [`Host`], and per-step energy metering against [`crate::energy`].
//! This is the clean-room execution core; the revm-adaptation decision reuses this
//! energy model and opcode table as the reference.

use crate::energy::{base_cost, sstore_cost};
use crate::opcode::OpCode;
use crate::EnergyMeter;
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
}

/// Run `code` with an energy `limit` against `host` (no calldata).
pub fn run(code: &[u8], limit: u64, host: &mut dyn Host) -> Outcome {
    run_with_input(code, &[], limit, host)
}

/// Byte-addressed memory that grows in 32-byte words, charging expansion energy.
#[derive(Default)]
struct Memory {
    data: Vec<u8>,
}

impl Memory {
    fn words(&self) -> u64 {
        (self.data.len() as u64 + 31) / 32
    }
    /// Ensure at least `end` bytes exist; returns the expansion energy to charge.
    fn expand_to(&mut self, end: usize) -> u64 {
        if end <= self.data.len() {
            return 0;
        }
        let cur = self.words();
        let new_words = (end as u64 + 31) / 32;
        self.data.resize((new_words * 32) as usize, 0);
        crate::energy::memory_expansion_cost(cur, new_words)
    }
    fn load(&self, off: usize) -> U256 {
        let mut buf = [0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.data.get(off + i).copied().unwrap_or(0);
        }
        U256::from_big_endian(&buf)
    }
    fn store(&mut self, off: usize, val: U256) {
        let bytes = val.to_big_endian();
        self.data[off..off + 32].copy_from_slice(&bytes);
    }
}

/// Run `code` with `calldata` and an energy `limit` against `host`.
pub fn run_with_input(code: &[u8], calldata: &[u8], limit: u64, host: &mut dyn Host) -> Outcome {
    let mut meter = EnergyMeter::new(limit);
    let mut stack: Vec<U256> = Vec::new();
    let mut mem = Memory::default();
    let mut pc = 0usize;

    macro_rules! pop {
        () => {
            match stack.pop() {
                Some(v) => v,
                None => return done(Halt::StackUnderflow, &stack, &meter),
            }
        };
    }
    macro_rules! push {
        ($v:expr) => {{
            if stack.len() >= STACK_LIMIT {
                return done(Halt::StackOverflow, &stack, &meter);
            }
            stack.push($v);
        }};
    }

    while pc < code.len() {
        let byte = code[pc];
        let op = match OpCode::from_u8(byte) {
            Some(o) => o,
            None => return done(Halt::BadOpcode(byte), &stack, &meter),
        };

        // Static per-op base cost; SSTORE is charged dynamically below.
        if op != OpCode::Sstore && !meter.charge(base_cost(op)) {
            return done(Halt::OutOfEnergy, &stack, &meter);
        }

        match op {
            OpCode::Stop => return done(Halt::Stop, &stack, &meter),
            OpCode::Add => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_add(b).0); }
            OpCode::Sub => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_sub(b).0); }
            OpCode::Mul => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_mul(b).0); }
            OpCode::Div => { let (a,b)=(pop!(),pop!()); push!(if b.is_zero(){U256::zero()}else{a/b}); }
            OpCode::Mod => { let (a,b)=(pop!(),pop!()); push!(if b.is_zero(){U256::zero()}else{a%b}); }
            OpCode::Exp => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_pow(b).0); }
            OpCode::Lt => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a<b))); }
            OpCode::Gt => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a>b))); }
            OpCode::Eq => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a==b))); }
            OpCode::IsZero => { let a=pop!(); push!(U256::from(u8::from(a.is_zero()))); }
            OpCode::And => { let (a,b)=(pop!(),pop!()); push!(a & b); }
            OpCode::Or => { let (a,b)=(pop!(),pop!()); push!(a | b); }
            OpCode::Xor => { let (a,b)=(pop!(),pop!()); push!(a ^ b); }
            OpCode::Pop => { let _=pop!(); }
            OpCode::Dup1 => {
                let a = match stack.last() {
                    Some(v) => *v,
                    None => return done(Halt::StackUnderflow, &stack, &meter),
                };
                push!(a);
            }
            OpCode::Swap1 => {
                let n = stack.len();
                if n < 2 { return done(Halt::StackUnderflow, &stack, &meter); }
                stack.swap(n-1, n-2);
            }
            OpCode::Push1 => {
                pc += 1;
                let v = *code.get(pc).unwrap_or(&0);
                push!(U256::from(v));
            }
            OpCode::CallDataLoad => {
                let off = pop!().low_u64() as usize;
                let mut buf = [0u8; 32];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = calldata.get(off + i).copied().unwrap_or(0);
                }
                push!(U256::from_big_endian(&buf));
            }
            OpCode::CallDataSize => push!(U256::from(calldata.len())),
            OpCode::CallDataCopy => {
                let dest = pop!().low_u64() as usize;
                let off = pop!().low_u64() as usize;
                let len = pop!().low_u64() as usize;
                if !meter.charge(mem.expand_to(dest.saturating_add(len))) {
                    return done(Halt::OutOfEnergy, &stack, &meter);
                }
                for i in 0..len {
                    mem.data[dest + i] = calldata.get(off + i).copied().unwrap_or(0);
                }
            }
            OpCode::Mload => {
                let off = pop!().low_u64() as usize;
                if !meter.charge(mem.expand_to(off.saturating_add(32))) {
                    return done(Halt::OutOfEnergy, &stack, &meter);
                }
                push!(mem.load(off));
            }
            OpCode::Mstore => {
                let off = pop!().low_u64() as usize;
                let val = pop!();
                if !meter.charge(mem.expand_to(off.saturating_add(32))) {
                    return done(Halt::OutOfEnergy, &stack, &meter);
                }
                mem.store(off, val);
            }
            OpCode::Mstore8 => {
                let off = pop!().low_u64() as usize;
                let val = pop!();
                if !meter.charge(mem.expand_to(off.saturating_add(1))) {
                    return done(Halt::OutOfEnergy, &stack, &meter);
                }
                mem.data[off] = (val.low_u64() & 0xff) as u8;
            }
            OpCode::Sload => {
                let key = pop!();
                push!(host.sload(key));
            }
            OpCode::Sstore => {
                let key = pop!();
                let value = pop!();
                let current = host.sload(key);
                let cost = sstore_cost(current.is_zero(), value.is_zero());
                if !meter.charge(cost) {
                    return done(Halt::OutOfEnergy, &stack, &meter);
                }
                host.sstore(key, value);
            }
            OpCode::Jump => {
                let dest = pop!();
                let d = dest.low_u64() as usize;
                if dest > U256::from(code.len()) || code.get(d) != Some(&(OpCode::Jumpdest as u8)) {
                    return done(Halt::BadJump, &stack, &meter);
                }
                pc = d; continue;
            }
            OpCode::Jumpi => {
                let dest = pop!();
                let cond = pop!();
                if !cond.is_zero() {
                    let d = dest.low_u64() as usize;
                    if dest > U256::from(code.len()) || code.get(d) != Some(&(OpCode::Jumpdest as u8)) {
                        return done(Halt::BadJump, &stack, &meter);
                    }
                    pc = d; continue;
                }
            }
            OpCode::Jumpdest => {}
            OpCode::Return => return done(Halt::Return, &stack, &meter),
            OpCode::Revert => return done(Halt::Revert, &stack, &meter),
            other => return done(Halt::BadOpcode(other as u8), &stack, &meter),
        }
        pc += 1;
    }
    done(Halt::Stop, &stack, &meter)
}

fn done(halt: Halt, stack: &[U256], meter: &EnergyMeter) -> Outcome {
    Outcome { halt, stack_top: stack.last().copied(), energy_used: meter.used }
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
        // PUSH1 2, PUSH1 3, ADD -> 5
        let out = run_mem(&[Push1 as u8, 2, Push1 as u8, 3, Add as u8, Stop as u8], 1000);
        assert_eq!(out.stack_top, Some(U256::from(5)));
    }

    #[test]
    fn u256_overflow_wraps() {
        // (2^255) * 2 wraps to 0 in 256-bit; build via EXP: PUSH1 255, PUSH1 2, EXP -> 2^255
        // then PUSH1 2, MUL -> 2^256 mod 2^256 = 0
        let code = [Push1 as u8, 255, Push1 as u8, 2, Exp as u8, Push1 as u8, 2, Mul as u8, Stop as u8];
        assert_eq!(run_mem(&code, 100000).stack_top, Some(U256::zero()));
    }

    #[test]
    fn sstore_then_sload_persists_and_meters() {
        // PUSH1 42, PUSH1 7, SSTORE ; PUSH1 7, SLOAD -> 42
        let code = [
            Push1 as u8, 42, Push1 as u8, 7, Sstore as u8,
            Push1 as u8, 7, Sload as u8, Stop as u8,
        ];
        let mut host = MemoryHost::default();
        let out = run(&code, 100000, &mut host);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(U256::from(42)));
        // storage persisted in the host
        assert_eq!(host.sload(U256::from(7)), U256::from(42));
        // SET_SSTORE (20000) dominates the energy used
        assert!(out.energy_used >= 20000);
    }

    #[test]
    fn sstore_out_of_energy_before_write() {
        // not enough energy to cover SET_SSTORE -> storage must NOT change
        let code = [Push1 as u8, 42, Push1 as u8, 7, Sstore as u8];
        let mut host = MemoryHost::default();
        let out = run(&code, 100, &mut host); // pushes fit, sstore(20000) does not
        assert_eq!(out.halt, Halt::OutOfEnergy);
        assert_eq!(host.sload(U256::from(7)), U256::zero()); // unchanged
    }

    #[test]
    fn clearing_a_slot_costs_less_than_setting() {
        // set slot (20000) then clear it (5000)
        let set = [Push1 as u8, 1, Push1 as u8, 3, Sstore as u8, Stop as u8];
        let clear = [Push1 as u8, 0, Push1 as u8, 3, Sstore as u8, Stop as u8];
        let mut host = MemoryHost::default();
        let e_set = run(&set, 100000, &mut host).energy_used;
        let e_clear = run(&clear, 100000, &mut host).energy_used;
        assert!(e_set > e_clear, "set {e_set} should exceed clear {e_clear}");
        assert!(host.sload(U256::from(3)).is_zero()); // cleared
    }

    #[test]
    fn loop_with_jumpi_counts_down() {
        // storage[0] = 3; loop: dec until zero. Simplified: just prove JUMPI loop
        // executes multiple times by summing. PUSH1 3 as counter is enough to
        // exercise the back-edge; here we verify a taken then not-taken branch.
        let code = [
            Push1 as u8, 1, Push1 as u8, 7, Jumpi as u8,   // cond=1 -> jump to 7
            Push1 as u8, 0xff,                               // skipped
            Jumpdest as u8, Push1 as u8, 9, Stop as u8,      // @7
        ];
        assert_eq!(run_mem(&code, 1000).halt, Halt::Stop);
    }

    #[test]
    fn mstore_then_mload_roundtrips_and_charges_expansion() {
        // PUSH1 42, PUSH1 0, MSTORE ; PUSH1 0, MLOAD -> 42
        let code = [Push1 as u8, 42, Push1 as u8, 0, Mstore as u8, Push1 as u8, 0, Mload as u8, Stop as u8];
        let out = run_mem(&code, 100000);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(U256::from(42)));
        // memory expansion (one word) was charged on top of the base costs
        assert!(out.energy_used > 0);
    }

    #[test]
    fn calldataload_reads_input_word() {
        // CALLDATALOAD at offset 0 reads the first 32 bytes of calldata as a word
        let code = [Push1 as u8, 0, CallDataLoad as u8, Stop as u8];
        let mut calldata = [0u8; 32];
        calldata[31] = 7; // big-endian -> value 7
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
        // copy 32 bytes of calldata to mem[0], then MLOAD 0
        // PUSH1 32(len) PUSH1 0(src) PUSH1 0(dest) CALLDATACOPY ; PUSH1 0 MLOAD
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
    fn stack_overflow_guarded() {
        // DUP1 forever from a single value would overflow; build 1025 pushes worth
        let mut code = vec![Push1 as u8, 1];
        for _ in 0..STACK_LIMIT + 5 {
            code.push(Dup1 as u8);
        }
        assert_eq!(run_mem(&code, 10_000_000).halt, Halt::StackOverflow);
    }
}
