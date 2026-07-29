//! Bridge the TVM [`Host`] to persistent contract storage in [`WorldState`].
//!
//! Contract storage is keyed `address(21) || slot(32-byte BE)` in the
//! `contract_storage` column family (java-tron `StorageRowStore`; opentron uses
//! the same address||H256 layout). This adapter lets the clean-room interpreter
//! execute against real chain state — the P2→P1 integration point.

use primitive_types::U256;
use tron_proto::protocol;
use tron_state::{cf, StateError, WorldState};
use tron_storage::KvStore;
use tron_types::Address;
use tron_vm::frame::{StateBackend, World};
use tron_vm::interp::Host;

const CF_CONTRACT_STORAGE: &str = tron_state::cf::CONTRACT_STORAGE;

/// Map a 20-byte engine address body to its 21-byte Tron address (`0x41 || body`).
fn addr21(body: &[u8]) -> Option<Address> {
    let b: [u8; 20] = body.try_into().ok()?;
    Some(Address::from_body(b))
}

/// Contract-storage key: `address(21) || slot(32-byte BE)` (matches [`StateHost`]).
fn storage_key(addr: &Address, slot: U256) -> Vec<u8> {
    let mut key = Vec::with_capacity(21 + 32);
    key.extend_from_slice(addr.as_bytes());
    key.extend_from_slice(&slot.to_big_endian());
    key
}

/// Read-through [`StateBackend`] over the world state: the multi-account VM engine
/// (T02) reads code/storage/balances it hasn't written straight from chain state.
/// Engine addresses are 20-byte EVM bodies; this maps them to 21-byte Tron addresses.
pub struct WorldStateBackend<'a, S: KvStore> {
    state: &'a WorldState<S>,
}

impl<'a, S: KvStore> WorldStateBackend<'a, S> {
    pub fn new(state: &'a WorldState<S>) -> Self {
        Self { state }
    }
}

impl<S: KvStore> StateBackend for WorldStateBackend<'_, S> {
    fn get_code(&self, addr: &[u8]) -> Vec<u8> {
        addr21(addr).and_then(|a| self.state.get_code(&a).ok()).unwrap_or_default()
    }
    fn sload(&self, addr: &[u8], slot: U256) -> U256 {
        let Some(a) = addr21(addr) else { return U256::zero() };
        match self.state.db.get(CF_CONTRACT_STORAGE, &storage_key(&a, slot)) {
            Ok(Some(b)) if b.len() == 32 => U256::from_big_endian(&b),
            _ => U256::zero(),
        }
    }
    fn balance(&self, addr: &Address) -> i64 {
        self.state.get_account(addr).ok().flatten().map(|a| a.balance).unwrap_or(0)
    }
    fn account_exists(&self, addr: &Address) -> bool {
        self.state.account_exists(addr).unwrap_or(false)
    }
}

/// The dirty writes extracted from a finished [`World`] (owned, so the World's borrow
/// of the read-through state can end before we flush with `&mut state`).
pub struct WorldWrites {
    pub storage: Vec<(Vec<u8>, U256, U256)>,
    pub balances: Vec<(Address, i64)>,
    pub code: Vec<(Vec<u8>, Vec<u8>)>,
    pub suicided: Vec<Address>,
}

impl WorldWrites {
    pub fn from_world(world: &World) -> Self {
        Self {
            storage: world.dirty_storage(),
            balances: world.dirty_balances(),
            code: world.dirty_code(),
            suicided: world.suicided(),
        }
    }
}

/// Flush a finished multi-account World's dirty writes back to chain state (T02):
/// storage slots (zero -> delete), balance changes, newly deployed code, and
/// SELFDESTRUCT deletions. Call only on a SUCCESSFUL execution — a revert leaves state
/// untouched. Storage/code keys are 20-byte engine bodies mapped to 21-byte addresses.
pub fn flush_world<S: KvStore>(state: &mut WorldState<S>, writes: &WorldWrites) -> Result<(), StateError> {
    for (body, slot, value) in &writes.storage {
        let Some(a) = addr21(body) else { continue };
        let key = storage_key(&a, *slot);
        if value.is_zero() {
            state.db.delete(CF_CONTRACT_STORAGE, &key)?;
        } else {
            state.db.put(CF_CONTRACT_STORAGE, &key, &value.to_big_endian())?;
        }
    }
    for (addr, bal) in &writes.balances {
        let mut acc = state
            .get_account(addr)?
            .unwrap_or(protocol::Account { address: addr.as_bytes().to_vec(), ..Default::default() });
        acc.balance = *bal;
        state.put_account(addr, &acc)?;
    }
    for (body, code) in &writes.code {
        if let Some(a) = addr21(body) {
            state.put_code(&a, code)?;
        }
    }
    // SELFDESTRUCT: the balance move is already in `balances`; delete the account + code.
    // (Storage rows of a self-destructed contract are left as a documented minor
    // deviation — they are unreachable once the account is gone.)
    for addr in &writes.suicided {
        state.db.delete(cf::ACCOUNT, addr.as_bytes())?;
        state.db.delete(cf::CONTRACT_CODE, addr.as_bytes())?;
    }
    Ok(())
}

/// A [`Host`] backed by a contract's storage in the world state.
pub struct StateHost<'a, S: KvStore> {
    state: &'a mut WorldState<S>,
    contract: Address,
}

impl<'a, S: KvStore> StateHost<'a, S> {
    pub fn new(state: &'a mut WorldState<S>, contract: Address) -> Self {
        Self { state, contract }
    }

    fn storage_key(&self, slot: U256) -> Vec<u8> {
        let mut key = Vec::with_capacity(21 + 32);
        key.extend_from_slice(self.contract.as_bytes());
        key.extend_from_slice(&slot.to_big_endian());
        key
    }
}

impl<S: KvStore> Host for StateHost<'_, S> {
    fn sload(&self, slot: U256) -> U256 {
        match self.state.db.get(CF_CONTRACT_STORAGE, &self.storage_key(slot)) {
            Ok(Some(bytes)) if bytes.len() == 32 => U256::from_big_endian(&bytes),
            _ => U256::zero(),
        }
    }

    fn sstore(&mut self, slot: U256, value: U256) {
        let key = self.storage_key(slot);
        if value.is_zero() {
            let _ = self.state.db.delete(CF_CONTRACT_STORAGE, &key);
        } else {
            let _ = self.state.db.put(CF_CONTRACT_STORAGE, &key, &value.to_big_endian());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;
    use tron_vm::interp::{run, Halt};
    use tron_vm::opcode::OpCode::*;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    #[test]
    fn bytecode_persists_to_world_state() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0xcc);

        // PUSH1 42, PUSH1 7, SSTORE, STOP  -> storage[7] = 42 in the contract
        let code = [Push1 as u8, 42, Push1 as u8, 7, Sstore as u8, Stop as u8];
        {
            let mut host = StateHost::new(&mut ws, contract);
            let out = run(&code, 100_000, &mut host);
            assert_eq!(out.halt, Halt::Stop);
        }

        // The value is in the real world state under the address||slot key.
        let host = StateHost::new(&mut ws, contract);
        assert_eq!(host.sload(U256::from(7)), U256::from(42));

        // A different contract has isolated storage.
        let other = StateHost::new(&mut ws, addr(0xdd));
        assert_eq!(other.sload(U256::from(7)), U256::zero());
    }

    #[test]
    fn sload_reads_previously_stored_state() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0xab);
        // Pre-seed storage via one run, read it back via another.
        {
            let mut host = StateHost::new(&mut ws, contract);
            host.sstore(U256::from(1), U256::from(999));
        }
        // PUSH1 1, SLOAD, STOP -> 999
        let code = [Push1 as u8, 1, Sload as u8, Stop as u8];
        let mut host = StateHost::new(&mut ws, contract);
        let out = run(&code, 100_000, &mut host);
        assert_eq!(out.stack_top, Some(U256::from(999)));
    }

    #[test]
    fn clearing_removes_from_state() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0x01);
        let mut host = StateHost::new(&mut ws, contract);
        host.sstore(U256::from(5), U256::from(7));
        assert_eq!(host.sload(U256::from(5)), U256::from(7));
        host.sstore(U256::from(5), U256::zero()); // clear
        assert_eq!(host.sload(U256::from(5)), U256::zero());
    }
}
