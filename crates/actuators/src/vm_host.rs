//! Bridge the TVM [`Host`] to persistent contract storage in [`WorldState`].
//!
//! Contract storage is keyed `address(21) || slot(32-byte BE)` in the
//! `contract_storage` column family (java-tron `StorageRowStore`; opentron uses
//! the same address||H256 layout). This adapter lets the clean-room interpreter
//! execute against real chain state — the P2→P1 integration point.

use primitive_types::U256;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::Address;
use tron_vm::interp::Host;

const CF_CONTRACT_STORAGE: &str = "contract_storage";

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
