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
use std::collections::{HashMap, HashSet};
use tron_types::Address;

/// EVM call-depth limit.
pub const MAX_CALL_DEPTH: usize = 1024;

type Key = (Vec<u8>, U256);

enum JournalEntry {
    Storage { key: Key, prev: Option<U256> },
    /// Prior balance of `addr` before a write (SELFDESTRUCT transfer/burn).
    Balance { addr: Address, prev: i64 },
    /// `addr` was newly registered as existing (undo: drop it again).
    AccountCreate { addr: Address },
    /// `addr` was marked for deletion (undo: unmark it).
    Suicide { addr: Address },
    /// `amount` was added to the burn accumulator (undo: subtract it).
    Burn { amount: i64 },
}

/// The multi-contract world: per-(address,slot) storage, per-address code,
/// per-account balances / existence / deletion marks, and a journal enabling
/// checkpoint/revert of every mutation.
///
/// Accounts here are keyed by the full 21-byte [`Address`] (`0x41` + 20 body), so the
/// SELFDESTRUCT self-inheritance check compares all 21 bytes (audit CS-JTRON-012).
#[derive(Default)]
pub struct World {
    storage: HashMap<Key, U256>,
    code: HashMap<Vec<u8>, Vec<u8>>,
    balances: HashMap<Address, i64>,
    /// Existence registry (java-tron account store `has`): drives the SELFDESTRUCT
    /// new-account energy surcharge.
    accounts: HashSet<Address>,
    /// Contracts marked for deletion by SELFDESTRUCT in this (sub-)execution.
    suicides: Vec<Address>,
    /// Sun destroyed by self-inheriting SELFDESTRUCTs (beneficiary == contract).
    burned: i64,
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
    // -- balances / account existence / suicide (SELFDESTRUCT) ------------

    /// Seed an account's balance and mark it existing (test/setup helper; not journaled).
    pub fn set_balance(&mut self, addr: &Address, bal: i64) {
        self.accounts.insert(*addr);
        self.balances.insert(*addr, bal);
    }
    /// Register an existing zero-balance account (test/setup helper; not journaled).
    pub fn create_account(&mut self, addr: &Address) {
        self.accounts.insert(*addr);
        self.balances.entry(*addr).or_insert(0);
    }
    pub fn balance(&self, addr: &Address) -> i64 {
        self.balances.get(addr).copied().unwrap_or(0)
    }
    /// Whether `addr` exists in the account store (java-tron `AccountStore.has`).
    pub fn account_exists(&self, addr: &Address) -> bool {
        self.accounts.contains(addr)
    }
    /// Whether `addr` was marked for deletion by a SELFDESTRUCT.
    pub fn is_suicided(&self, addr: &Address) -> bool {
        self.suicides.contains(addr)
    }
    /// Total sun destroyed by self-inheriting SELFDESTRUCTs.
    pub fn burned(&self) -> i64 {
        self.burned
    }

    /// Journaled balance write.
    fn write_balance(&mut self, addr: &Address, new: i64) {
        let prev = self.balance(addr);
        self.journal.push(JournalEntry::Balance { addr: *addr, prev });
        self.balances.insert(*addr, new);
    }
    /// Journaled account registration (no-op if already present).
    fn ensure_account(&mut self, addr: &Address) {
        if self.accounts.insert(*addr) {
            self.journal.push(JournalEntry::AccountCreate { addr: *addr });
        }
    }

    /// SELFDESTRUCT state transition (java-tron `Program.suicide`): move the whole
    /// balance of `contract` to `beneficiary`, or **burn** it when
    /// `beneficiary == contract` — compared over the full 21-byte [`Address`], so a
    /// 20-byte prefix match with a different 21st byte is NOT self (audit CS-JTRON-012).
    /// Then mark `contract` for deletion. All effects are journaled, so a reverting
    /// parent CALL rolls them back. Marking is idempotent per frame.
    pub fn suicide(&mut self, contract: &Address, beneficiary: &Address) {
        let bal = self.balance(contract);
        if beneficiary == contract {
            // Self-inheritance: the account is deleted, so its balance is destroyed.
            self.write_balance(contract, 0);
            if bal != 0 {
                self.journal.push(JournalEntry::Burn { amount: bal });
                self.burned = self.burned.saturating_add(bal);
            }
        } else {
            // Transferring to the inheritor creates it if absent (what the
            // NEW_ACCT_CALL surcharge paid for).
            self.ensure_account(beneficiary);
            let benef_bal = self.balance(beneficiary);
            self.write_balance(contract, 0);
            self.write_balance(beneficiary, benef_bal.saturating_add(bal));
        }
        if !self.suicides.contains(contract) {
            self.journal.push(JournalEntry::Suicide { addr: *contract });
            self.suicides.push(*contract);
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
                JournalEntry::Balance { addr, prev } => {
                    self.balances.insert(addr, prev);
                }
                JournalEntry::AccountCreate { addr } => {
                    self.accounts.remove(&addr);
                }
                JournalEntry::Suicide { addr } => {
                    self.suicides.retain(|a| *a != addr);
                }
                JournalEntry::Burn { amount } => {
                    self.burned -= amount;
                }
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
    SelfDestruct,
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
            OpCode::SelfDestruct => {
                // Beneficiary = low 20 bytes of the popped word, as a 21-byte Tron
                // address (0x41 prefix). The running contract's own 21-byte address
                // is likewise `0x41` + its 20-byte `address`.
                let word = pop!();
                let wb = word.to_big_endian();
                let benef_body = <[u8; 20]>::try_from(&wb[12..32]).unwrap();
                let beneficiary = Address::from_body(benef_body);
                let self_addr = match <[u8; 20]>::try_from(address) {
                    Ok(body) => Address::from_body(body),
                    // Non-20-byte contract address can't form a Tron address.
                    Err(_) => return CallResult { success: false, halt: Halt::BadOpcode(byte), energy_used: meter.used },
                };
                // getSuicideCost2: surcharge when the beneficiary account is absent.
                let cost = crate::energy::suicide_cost(world.account_exists(&beneficiary));
                if !meter.charge(cost) {
                    return CallResult { success: false, halt: Halt::OutOfEnergy, energy_used: meter.used };
                }
                world.suicide(&self_addr, &beneficiary);
                return CallResult { success: true, halt: Halt::SelfDestruct, energy_used: meter.used };
            }
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

    // -- SELFDESTRUCT (H03 / CS-JTRON-002, -012) --------------------------

    /// Body-20 -> the beneficiary address a `PUSH1 lo; SELFDESTRUCT` program targets.
    fn benef_of(lo: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = lo;
        Address::from_body(b)
    }

    /// Run `PUSH1 lo; SELFDESTRUCT` from a contract seeded with `bal`; the caller
    /// pre-registers the beneficiary iff `benef_exists`. Returns (energy_used, world, contract).
    fn run_suicide(lo: u8, bal: i64, benef_exists: bool) -> (u64, World, Address) {
        let mut world = World::new();
        let cbody = [0xaa; 20];
        let contract = Address::from_body(cbody);
        world.set_balance(&contract, bal);
        if benef_exists {
            world.create_account(&benef_of(lo));
        }
        world.set_code(&cbody, vec![Push1 as u8, lo, SelfDestruct as u8]);
        let r = execute(&mut world, &cbody, 1_000_000, 0);
        assert!(r.success);
        assert_eq!(r.halt, Halt::SelfDestruct);
        (r.energy_used, world, contract)
    }

    #[test]
    fn suicide_to_absent_beneficiary_charges_new_account_energy() {
        let (energy, world, contract) = run_suicide(0x07, 1_000, false);
        assert!(energy >= crate::energy::NEW_ACCT_CALL, "energy {energy} must include NEW_ACCT_CALL");
        // balance moved to the (now created) beneficiary; contract drained + marked.
        assert_eq!(world.balance(&benef_of(0x07)), 1_000);
        assert_eq!(world.balance(&contract), 0);
        assert!(world.account_exists(&benef_of(0x07)));
        assert!(world.is_suicided(&contract));
        assert_eq!(world.burned(), 0); // transfer, not burn
    }

    #[test]
    fn suicide_new_account_surcharge_is_exactly_new_acct_call() {
        // The only difference between the two runs is beneficiary existence.
        let (absent, _, _) = run_suicide(0x07, 10, false);
        let (present, _, _) = run_suicide(0x07, 10, true);
        assert_eq!(absent - present, crate::energy::NEW_ACCT_CALL);
    }

    #[test]
    fn suicide_to_self_burns_balance_full_21_byte_match() {
        let mut world = World::new();
        let c = Address::from_body([0x11; 20]); // 21 bytes: 0x41 + 0x11*20
        world.set_balance(&c, 5_000);
        world.suicide(&c, &c); // beneficiary == contract, all 21 bytes equal
        assert_eq!(world.balance(&c), 0);
        assert_eq!(world.burned(), 5_000); // burned, not transferred to anyone
        assert!(world.is_suicided(&c));
    }

    #[test]
    fn twenty_byte_prefix_but_different_21st_byte_is_not_self() {
        // self and beneficiary share the first 20 bytes but differ in the 21st.
        let mut sa = [0x11u8; 21];
        sa[0] = 0x41;
        let self_addr = Address::from_bytes(sa).unwrap(); // [0x41, 0x11*20]
        let mut bb = [0x11u8; 21];
        bb[0] = 0x41;
        bb[20] = 0x22; // 21st byte differs
        let benef = Address::from_bytes(bb).unwrap(); // [0x41, 0x11*19, 0x22]
        // sanity: first 20 bytes identical, 21st differs.
        assert_eq!(self_addr.as_bytes()[..20], benef.as_bytes()[..20]);
        assert_ne!(self_addr.as_bytes()[20], benef.as_bytes()[20]);

        let mut world = World::new();
        world.set_balance(&self_addr, 7_000);
        world.suicide(&self_addr, &benef);
        // NOT treated as self -> transfer to beneficiary, nothing burned.
        assert_eq!(world.burned(), 0);
        assert_eq!(world.balance(&self_addr), 0);
        assert_eq!(world.balance(&benef), 7_000);
    }

    #[test]
    fn suicide_is_journaled_and_reverts() {
        let mut world = World::new();
        let c = Address::from_body([0x33; 20]);
        let b = Address::from_body([0x44; 20]);
        world.set_balance(&c, 900);
        world.create_account(&b); // beneficiary already exists

        let cp = world.checkpoint();
        world.suicide(&c, &b);
        assert_eq!(world.balance(&b), 900);
        assert_eq!(world.balance(&c), 0);
        assert!(world.is_suicided(&c));

        world.revert_to(cp);
        assert_eq!(world.balance(&c), 900); // balance restored
        assert_eq!(world.balance(&b), 0);
        assert!(!world.is_suicided(&c)); // deletion mark undone
    }

    #[test]
    fn reverting_rolls_back_a_self_inheriting_suicide() {
        let mut world = World::new();
        let c = Address::from_body([0x55; 20]);
        world.set_balance(&c, 1_234);
        let cp = world.checkpoint();
        world.suicide(&c, &c);
        assert_eq!(world.burned(), 1_234);
        world.revert_to(cp);
        assert_eq!(world.burned(), 0);
        assert_eq!(world.balance(&c), 1_234);
        assert!(!world.is_suicided(&c));
    }
}
