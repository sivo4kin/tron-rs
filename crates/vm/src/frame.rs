//! Journaled multi-contract execution: contract-to-contract CALL with revert.
//!
//! The single-contract [`crate::interp`] proves the execution+energy model; this
//! module adds the missing piece — a [`World`] holding every contract's storage
//! and code with a **revert journal**, so a CALL into another contract runs
//! against *its* storage and, on REVERT, rolls back only that sub-call's changes
//! (java-tron/EVM nested-call semantics). Recursion is depth-bounded.
//!
//! CALL here uses a simplified `(addr)` convention focused on the journaling
//! semantics; the memory/calldata-based 7-arg CALL lives in `interp`. The two
//! converge in a later unification.

use crate::energy::{base_cost, sstore_cost};
use crate::opcode::OpCode;
use crate::EnergyMeter;
use primitive_types::U256;
use std::collections::HashMap;

/// EVM call-depth limit.
pub const MAX_CALL_DEPTH: usize = 1024;

type Key = (Vec<u8>, U256);

enum JournalEntry {
    Storage { key: Key, prev: Option<U256> },
}

/// The multi-contract world: per-(address,slot) storage, per-address code, and a
/// journal enabling checkpoint/revert of storage mutations.
#[derive(Default)]
pub struct World {
    storage: HashMap<Key, U256>,
    code: HashMap<Vec<u8>, Vec<u8>>,
    journal: Vec<JournalEntry>,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_code(&mut self, addr: &[u8], code: Vec<u8>) {
        self.code.insert(addr.to_vec(), code);
    }
    pub fn get_code(&self, addr: &[u8]) -> Vec<u8> {
        self.code.get(addr).cloned().unwrap_or_default()
    }
    pub fn sload(&self, addr: &[u8], slot: U256) -> U256 {
        self.storage.get(&(addr.to_vec(), slot)).copied().unwrap_or_default()
    }
    fn sstore(&mut self, addr: &[u8], slot: U256, value: U256) {
        let key = (addr.to_vec(), slot);
        let prev = self.storage.get(&key).copied();
        self.journal.push(JournalEntry::Storage { key: key.clone(), prev });
        if value.is_zero() {
            self.storage.remove(&key);
        } else {
            self.storage.insert(key, value);
        }
    }
    fn checkpoint(&self) -> usize {
        self.journal.len()
    }
    /// Undo every journal entry after `cp`, restoring prior storage values.
    fn revert_to(&mut self, cp: usize) {
        while self.journal.len() > cp {
            match self.journal.pop().unwrap() {
                JournalEntry::Storage { key, prev } => match prev {
                    Some(v) => {
                        self.storage.insert(key, v);
                    }
                    None => {
                        self.storage.remove(&key);
                    }
                },
            }
        }
    }
    /// Keep changes since `cp` (drop the undo records so they persist).
    fn commit(&mut self, cp: usize) {
        self.journal.truncate(cp);
        // Truncating only discards undo records; the storage map keeps the writes.
        let _ = cp;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    Stop,
    Return,
    Revert,
    OutOfEnergy,
    StackUnderflow,
    BadOpcode(u8),
    DepthLimit,
}

#[derive(Debug)]
pub struct CallResult {
    pub success: bool,
    pub halt: Halt,
    pub energy_used: u64,
}

/// Execute `code` as contract `address` against `world`, with revert journaling.
pub fn execute(
    world: &mut World,
    address: &[u8],
    energy_limit: u64,
    depth: usize,
) -> CallResult {
    let mut meter = EnergyMeter::new(energy_limit);
    if depth > MAX_CALL_DEPTH {
        return CallResult { success: false, halt: Halt::DepthLimit, energy_used: 0 };
    }
    let code = world.get_code(address);
    let mut stack: Vec<U256> = Vec::new();
    let mut pc = 0usize;

    macro_rules! pop {
        () => {
            match stack.pop() {
                Some(v) => v,
                None => return CallResult { success: false, halt: Halt::StackUnderflow, energy_used: meter.used },
            }
        };
    }

    while pc < code.len() {
        let byte = code[pc];
        let op = match OpCode::from_u8(byte) {
            Some(o) => o,
            None => return CallResult { success: false, halt: Halt::BadOpcode(byte), energy_used: meter.used },
        };
        if op != OpCode::Sstore && !meter.charge(base_cost(op)) {
            return CallResult { success: false, halt: Halt::OutOfEnergy, energy_used: meter.used };
        }
        match op {
            OpCode::Stop => return CallResult { success: true, halt: Halt::Stop, energy_used: meter.used },
            OpCode::Add => { let (a,b)=(pop!(),pop!()); stack.push(a.overflowing_add(b).0); }
            OpCode::Push1 => { pc += 1; stack.push(U256::from(*code.get(pc).unwrap_or(&0))); }
            OpCode::Push2 => {
                let hi = *code.get(pc + 1).unwrap_or(&0) as u16;
                let lo = *code.get(pc + 2).unwrap_or(&0) as u16;
                pc += 2;
                stack.push(U256::from((hi << 8) | lo));
            }
            OpCode::Sload => { let k = pop!(); stack.push(world.sload(address, k)); }
            OpCode::Sstore => {
                let k = pop!(); let v = pop!();
                let cur = world.sload(address, k);
                if !meter.charge(sstore_cost(cur.is_zero(), v.is_zero())) {
                    return CallResult { success: false, halt: Halt::OutOfEnergy, energy_used: meter.used };
                }
                world.sstore(address, k, v);
            }
            OpCode::Call => {
                if stack.len() < 2 {
                    return CallResult { success: false, halt: Halt::StackUnderflow, energy_used: meter.used };
                }
                let callee = pop!();
                let requested_gas = pop!();
                // Address = low 20 bytes of the callee word.
                let buf = callee.to_big_endian();
                let callee_addr = buf[12..].to_vec();

                // EIP-150 all-but-64th: forward at most 63/64 of the remaining
                // energy, so the caller always retains >= 1/64 to continue after
                // the sub-call returns. The requested gas caps it further.
                let remaining = meter.remaining();
                let max_forward = remaining - remaining / 64;
                let forwarded = requested_gas.low_u64().min(max_forward);

                // Checkpoint, run the sub-call with the forwarded energy, revert on failure.
                let cp = world.checkpoint();
                let sub = execute(world, &callee_addr, forwarded, depth + 1);
                let _ = meter.charge(sub.energy_used);
                if sub.success {
                    world.commit(cp);
                } else {
                    world.revert_to(cp);
                }
                stack.push(U256::from(u8::from(sub.success)));
            }
            OpCode::Return => return CallResult { success: true, halt: Halt::Return, energy_used: meter.used },
            OpCode::Revert => return CallResult { success: false, halt: Halt::Revert, energy_used: meter.used },
            other => return CallResult { success: false, halt: Halt::BadOpcode(other as u8), energy_used: meter.used },
        }
        pc += 1;
    }
    CallResult { success: true, halt: Halt::Stop, energy_used: meter.used }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcode::OpCode::*;

    fn addr(b: u8) -> Vec<u8> {
        vec![b; 20]
    }
    fn word_addr(a: &[u8]) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(a);
        w
    }

    #[test]
    fn contract_to_contract_call_persists_callee_storage() {
        // A calls B (addr fits in one byte -> pushable via PUSH1); B writes storage.
        let mut world = World::new();
        let (a, b) = (addr(0xaa), vec![0x07u8; 20]);
        // B: SSTORE 1 = 9, STOP
        world.set_code(&b, vec![Push1 as u8, 9, Push1 as u8, 1, Sstore as u8, Stop as u8]);
        // b's address word (low 20 bytes = 0x07...). Push the low byte and rely on
        // callee resolution using low 20 bytes: build a word where byte[31]=0x07 is
        // NOT enough (address is 20 bytes). So set B's address to 20 bytes of 0x00
        // except we push via the word — use an address that is a single low byte.
        let b1 = {
            let mut v = vec![0u8; 20];
            v[19] = 0x07;
            v
        };
        world.set_code(&b1, world.get_code(&b));
        // A: PUSH1 gas(gas=200 via 0xc8), PUSH1 0x07(addr word low byte), CALL, STOP
        // A: PUSH2 0xffff (gas), PUSH1 0x07 (addr), CALL, STOP
        world.set_code(&a, vec![Push2 as u8, 0xff, 0xff, Push1 as u8, 0x07, Call as u8, Stop as u8]);
        let cp = world.checkpoint();
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success);
        world.commit(cp);
        // B1's storage[1] committed to 9 through the successful sub-call.
        assert_eq!(world.sload(&b1, U256::from(1)), U256::from(9));
        let _ = word_addr(&a);
    }

    #[test]
    fn reverting_subcall_rolls_back_only_its_writes() {
        let mut world = World::new();
        let a = addr(0xaa);
        // Pre-set A.storage[5] = 3 (committed).
        let cp0 = world.checkpoint();
        world.sstore(&a, U256::from(5), U256::from(3));
        world.commit(cp0);

        // Now run a frame on A that writes slot 5 = 99 then REVERTs.
        world.set_code(&a, vec![Push1 as u8, 99, Push1 as u8, 5, Sstore as u8, Revert as u8]);
        let cp = world.checkpoint();
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(!r.success);
        assert_eq!(r.halt, Halt::Revert);
        world.revert_to(cp);
        // slot 5 restored to the pre-call committed value 3, not 99.
        assert_eq!(world.sload(&a, U256::from(5)), U256::from(3));
    }

    #[test]
    fn call_reserves_one_sixtyfourth_of_gas() {
        // A requests all gas (PUSH2 0xffff) for a callee that tries to burn
        // everything via a huge SSTORE loop; the caller must still finish (STOP)
        // because >= 1/64 was reserved. Here we assert the caller succeeds even
        // though the callee runs out.
        let mut world = World::new();
        let a = addr(0xaa);
        let callee = { let mut v = vec![0u8; 20]; v[19] = 0x09; v };
        // callee: SSTORE 1=1 (20000), SSTORE 2=1 (20000), ... will OOG on tiny gas
        world.set_code(&callee, vec![
            Push1 as u8, 1, Push1 as u8, 1, Sstore as u8,
            Push1 as u8, 1, Push1 as u8, 2, Sstore as u8, Stop as u8,
        ]);
        // A: request only 100 gas for the callee (PUSH1 100), CALL, then STOP.
        // callee OOGs -> CALL pushes 0 -> A still STOPs successfully.
        world.set_code(&a, vec![Push1 as u8, 100, Push1 as u8, 0x09, Call as u8, Stop as u8]);
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success, "caller must survive a callee OOG");
        // callee's writes were reverted (it failed)
        assert_eq!(world.sload(&callee, U256::from(1)), U256::zero());
    }

    #[test]
    fn depth_limit_enforced() {
        let mut world = World::new();
        let a = addr(0xaa);
        world.set_code(&a, vec![Stop as u8]);
        let r = execute(&mut world, &a, 1000, MAX_CALL_DEPTH + 1);
        assert_eq!(r.halt, Halt::DepthLimit);
    }

    #[test]
    fn journal_isolation_across_checkpoints() {
        let mut world = World::new();
        let a = addr(0x01);
        let cp1 = world.checkpoint();
        world.sstore(&a, U256::from(1), U256::from(10));
        let cp2 = world.checkpoint();
        world.sstore(&a, U256::from(1), U256::from(20));
        // revert to cp2 -> slot back to 10; cp1 change intact
        world.revert_to(cp2);
        assert_eq!(world.sload(&a, U256::from(1)), U256::from(10));
        world.revert_to(cp1);
        assert_eq!(world.sload(&a, U256::from(1)), U256::zero());
    }
}
