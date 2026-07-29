//! The journaled multi-account **state model** for the unified engine ([`crate::engine`]).
//!
//! Holds every contract's storage and code, per-account balances / existence /
//! deletion marks, and a revert journal enabling checkpoint/commit/revert — mirroring
//! java-tron's `Repository` behind `Program`. The execution loop that drives it lives in
//! [`crate::engine`]; the single-contract Host entry is [`crate::interp`].
//!
//! **Storage backend.** By default storage is a journaled map (multi-account). For the
//! single-contract persistent path the actuators use, the World is bound to a
//! [`crate::interp::Host`] via [`World::with_host`]: SLOAD/SSTORE route to that host
//! (immediate, unjournaled — the actuator commits the whole tx atomically), and
//! inter-contract CALL then reaches precompiles only (a single Host exposes no other
//! contract's code).

use crate::interp::Host;
use primitive_types::U256;
use std::collections::{HashMap, HashSet};
use tron_types::Address;

/// EVM call-depth limit.
pub const MAX_CALL_DEPTH: usize = 1024;
/// EVM stack depth limit.
pub const STACK_LIMIT: usize = 1024;

type Key = (Vec<u8>, U256);

enum JournalEntry {
    Storage { key: Key, prev: Option<U256> },
    Balance { addr: Address, prev: i64 },
    AccountCreate { addr: Address },
    Suicide { addr: Address },
    Burn { amount: i64 },
}

/// Storage backend: the journaled in-World map, or a bound single-contract Host.
enum Backend<'h> {
    World,
    Host(&'h mut dyn Host),
}

/// The multi-contract world: per-(address,slot) storage, per-address code, per-account
/// balances / existence / deletion marks, and a journal enabling checkpoint/revert of
/// every mutation. Accounts are keyed by the full 21-byte [`Address`], so SELFDESTRUCT's
/// self-inheritance check compares all 21 bytes (audit CS-JTRON-012).
pub struct World<'h> {
    storage: HashMap<Key, U256>,
    code: HashMap<Vec<u8>, Vec<u8>>,
    balances: HashMap<Address, i64>,
    accounts: HashSet<Address>,
    suicides: Vec<Address>,
    burned: i64,
    journal: Vec<JournalEntry>,
    backend: Backend<'h>,
}

impl World<'static> {
    pub fn new() -> Self {
        World {
            storage: HashMap::new(),
            code: HashMap::new(),
            balances: HashMap::new(),
            accounts: HashSet::new(),
            suicides: Vec::new(),
            burned: 0,
            journal: Vec::new(),
            backend: Backend::World,
        }
    }
}

impl Default for World<'static> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'h> World<'h> {
    /// A World whose storage is backed by a single-contract [`Host`] (the actuator
    /// persistence path). SLOAD/SSTORE route to the host regardless of address; nested
    /// CALL therefore reaches precompiles only.
    pub(crate) fn with_host(host: &'h mut dyn Host) -> Self {
        World {
            storage: HashMap::new(),
            code: HashMap::new(),
            balances: HashMap::new(),
            accounts: HashSet::new(),
            suicides: Vec::new(),
            burned: 0,
            journal: Vec::new(),
            backend: Backend::Host(host),
        }
    }

    pub(crate) fn host_backed(&self) -> bool {
        matches!(self.backend, Backend::Host(_))
    }

    pub fn set_code(&mut self, addr: &[u8], code: Vec<u8>) {
        self.code.insert(addr.to_vec(), code);
    }
    pub fn get_code(&self, addr: &[u8]) -> Vec<u8> {
        self.code.get(addr).cloned().unwrap_or_default()
    }

    pub fn sload(&self, addr: &[u8], slot: U256) -> U256 {
        match &self.backend {
            Backend::Host(h) => h.sload(slot),
            Backend::World => self.storage.get(&(addr.to_vec(), slot)).copied().unwrap_or_default(),
        }
    }
    pub(crate) fn sstore(&mut self, addr: &[u8], slot: U256, value: U256) {
        match &mut self.backend {
            // Host writes are immediate + unjournaled (single-contract actuator path,
            // which commits the whole tx atomically).
            Backend::Host(h) => h.sstore(slot, value),
            Backend::World => {
                let key = (addr.to_vec(), slot);
                let prev = self.storage.get(&key).copied();
                self.journal.push(JournalEntry::Storage { key: key.clone(), prev });
                if value.is_zero() {
                    self.storage.remove(&key);
                } else {
                    self.storage.insert(key, value);
                }
            }
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
    pub fn account_exists(&self, addr: &Address) -> bool {
        self.accounts.contains(addr)
    }
    pub fn is_suicided(&self, addr: &Address) -> bool {
        self.suicides.contains(addr)
    }
    pub fn burned(&self) -> i64 {
        self.burned
    }

    fn write_balance(&mut self, addr: &Address, new: i64) {
        let prev = self.balance(addr);
        self.journal.push(JournalEntry::Balance { addr: *addr, prev });
        self.balances.insert(*addr, new);
    }
    fn ensure_account(&mut self, addr: &Address) {
        if self.accounts.insert(*addr) {
            self.journal.push(JournalEntry::AccountCreate { addr: *addr });
        }
    }

    /// SELFDESTRUCT state transition (java-tron `Program.suicide`): move the whole
    /// balance of `contract` to `beneficiary`, or **burn** it when
    /// `beneficiary == contract` (compared over the full 21-byte [`Address`], audit
    /// CS-JTRON-012), then mark `contract` for deletion. All effects are journaled.
    pub fn suicide(&mut self, contract: &Address, beneficiary: &Address) {
        let bal = self.balance(contract);
        if beneficiary == contract {
            self.write_balance(contract, 0);
            if bal != 0 {
                self.journal.push(JournalEntry::Burn { amount: bal });
                self.burned = self.burned.saturating_add(bal);
            }
        } else {
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

    pub(crate) fn checkpoint(&self) -> usize {
        self.journal.len()
    }
    /// Undo every journal entry after `cp`, restoring prior state.
    pub(crate) fn revert_to(&mut self, cp: usize) {
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
    pub(crate) fn commit(&mut self, cp: usize) {
        self.journal.truncate(cp);
    }
}

/// Why an execution frame stopped. A superset of the reasons either half-engine used;
/// [`crate::interp`] maps it onto its narrower public `Halt`.
#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    Stop,
    Return,
    Revert,
    SelfDestruct,
    OutOfEnergy,
    StackUnderflow,
    StackOverflow,
    BadOpcode(u8),
    BadJump,
    DepthLimit,
}

/// Result of the public [`crate::engine::execute`] wrapper.
#[derive(Debug)]
pub struct CallResult {
    pub success: bool,
    pub halt: Halt,
    pub energy_used: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Vec<u8> {
        vec![b; 20]
    }

    #[test]
    fn journal_isolation_across_checkpoints() {
        let mut world = World::new();
        let a = addr(0x01);
        let cp1 = world.checkpoint();
        world.sstore(&a, U256::from(1), U256::from(10));
        let cp2 = world.checkpoint();
        world.sstore(&a, U256::from(1), U256::from(20));
        world.revert_to(cp2);
        assert_eq!(world.sload(&a, U256::from(1)), U256::from(10));
        world.revert_to(cp1);
        assert_eq!(world.sload(&a, U256::from(1)), U256::zero());
    }

    // -- SELFDESTRUCT state transition (H03 / CS-JTRON-002, -012) ---------

    #[test]
    fn suicide_to_self_burns_balance_full_21_byte_match() {
        let mut world = World::new();
        let c = Address::from_body([0x11; 20]);
        world.set_balance(&c, 5_000);
        world.suicide(&c, &c);
        assert_eq!(world.balance(&c), 0);
        assert_eq!(world.burned(), 5_000);
        assert!(world.is_suicided(&c));
    }

    #[test]
    fn twenty_byte_prefix_but_different_21st_byte_is_not_self() {
        let mut sa = [0x11u8; 21];
        sa[0] = 0x41;
        let self_addr = Address::from_bytes(sa).unwrap();
        let mut bb = [0x11u8; 21];
        bb[0] = 0x41;
        bb[20] = 0x22;
        let benef = Address::from_bytes(bb).unwrap();
        assert_eq!(self_addr.as_bytes()[..20], benef.as_bytes()[..20]);
        assert_ne!(self_addr.as_bytes()[20], benef.as_bytes()[20]);

        let mut world = World::new();
        world.set_balance(&self_addr, 7_000);
        world.suicide(&self_addr, &benef);
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
        world.create_account(&b);

        let cp = world.checkpoint();
        world.suicide(&c, &b);
        assert_eq!(world.balance(&b), 900);
        assert_eq!(world.balance(&c), 0);
        assert!(world.is_suicided(&c));

        world.revert_to(cp);
        assert_eq!(world.balance(&c), 900);
        assert_eq!(world.balance(&b), 0);
        assert!(!world.is_suicided(&c));
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
