//! Smart-contract execution (T02): full TriggerSmartContract / CreateSmartContract on
//! the unified multi-account VM engine.
//!
//! `TriggerSmartContract` builds a root call frame (caller, `call_value` moved
//! caller→contract, `data` calldata, energy limit) on a journaled [`World`] whose reads
//! fall through to real chain state ([`WorldStateBackend`]). It runs the unified engine
//! — memory, inter-contract CALL, precompiles, SELFDESTRUCT, journaled storage. On
//! success the World's dirty writes (storage, balances, deployed code, suicides) are
//! flushed to state ([`flush_world`]); on revert nothing is flushed but the spent energy
//! is still charged (`energy_used * energy_price` sun, burned). `CreateSmartContract`
//! runs the init bytecode, deploys the returned runtime code + `SmartContract` record.
//!
//! **Deviations (documented):** TVM time-limit (`OutOfTime`) and the precise refund
//! schedule are not modeled. CreateSmartContract address derivation still uses the
//! contract's provided address (else the owner) — the full `owner || tx_hash` derivation
//! needs the tx id, which isn't threaded into the actuator. If CREATE init bytecode does
//! not `RETURN` runtime code, the provided bytecode is deployed as-is. A self-destructed
//! contract's storage rows are left in place (unreachable once the account is deleted).
//! On the host-bound single-contract path SELFDESTRUCT is unsupported (see engine docs);
//! this actuator uses the multi-account path, where it works.

use crate::vm_host::{flush_world, WorldStateBackend, WorldWrites};
use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::{CreateSmartContract, TriggerSmartContract};
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};
use tron_vm::engine::{execute_call, execute_code};
use tron_vm::frame::World;

/// Energy limit for running CREATE constructor bytecode (deployment).
const DEPLOY_ENERGY_LIMIT: u64 = 3_000_000;

fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))
}

/// 20-byte EVM body of a 21-byte Tron address (the engine works in 20-byte addresses).
fn body20(a: &Address) -> Vec<u8> {
    a.as_bytes()[1..].to_vec()
}

/// Energy price in sun/energy (dynamic property `ENERGY_FEE`, default 100).
fn energy_price<S: KvStore>(state: &WorldState<S>) -> i64 {
    let p = state.get_prop_i64("ENERGY_FEE").unwrap_or(0);
    if p > 0 { p } else { tron_vm::DEFAULT_ENERGY_FEE_SUN }
}

/// Charge `fee` sun to `owner` and burn it (java-tron energy fee). Skipped when `fee`
/// is non-positive or the owner account doesn't exist (nothing to charge).
fn charge_energy_fee<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &Address,
    fee: i64,
) -> Result<(), ActuatorError> {
    if fee <= 0 {
        return Ok(());
    }
    let Some(mut acc) = state.get_account(owner)? else {
        return Ok(());
    };
    acc.balance = acc
        .balance
        .checked_sub(fee)
        .filter(|b| *b >= 0)
        .ok_or_else(|| ActuatorError::Execute("insufficient balance for energy fee".into()))?;
    state.put_account(owner, &acc)?;
    state.burn_trx(fee)?;
    Ok(())
}

/// Execute a `TriggerSmartContract`: run the target contract on the unified VM.
pub struct TriggerSmartContractActuator<'a> {
    contract: &'a TriggerSmartContract,
    energy_limit: u64,
}

impl<'a> TriggerSmartContractActuator<'a> {
    pub fn new(contract: &'a TriggerSmartContract, energy_limit: u64) -> Self {
        Self { contract, energy_limit }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        parse_address(&self.contract.owner_address, "ownerAddress")?;
        let target = parse_address(&self.contract.contract_address, "contractAddress")?;
        if state.get_code(&target).map_err(ActuatorError::from)?.is_empty() {
            return Err(ActuatorError::Validate("Contract does not exist".into()));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let target = parse_address(&self.contract.contract_address, "contractAddress")?;
        let data = self.contract.data.clone();
        let call_value = self.contract.call_value;
        let price = energy_price(state);
        let body = body20(&target);

        // Run on a state-backed journaled World; extract the result + dirty writes
        // (owned) so the read-through borrow of `state` ends before we flush.
        let (success, energy_used, writes) = {
            let backend = WorldStateBackend::new(state);
            let mut world = World::with_state(&backend);
            if call_value != 0 {
                // call_value moves caller -> contract (journaled; rolled back on revert).
                world.transfer(&owner, &target, call_value);
            }
            let r = execute_call(&mut world, &body, &data, self.energy_limit, 0);
            (r.success, r.energy_used, WorldWrites::from_world(&world))
        };

        // Commit storage/balances/etc. only on success; a revert leaves state untouched.
        if success {
            flush_world(state, &writes).map_err(ActuatorError::from)?;
        }
        // Energy is charged either way (java-tron consumes energy on revert too).
        let fee = tron_vm::energy_to_sun(energy_used, price);
        charge_energy_fee(state, &owner, fee)?;
        Ok(ExecutionResult { fee })
    }
}

/// Deploy a `CreateSmartContract`: run the init bytecode and store the runtime code.
pub struct CreateSmartContractActuator<'a> {
    contract: &'a CreateSmartContract,
}

impl<'a> CreateSmartContractActuator<'a> {
    pub fn new(contract: &'a CreateSmartContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, _state: &WorldState<S>) -> Result<i64, ActuatorError> {
        parse_address(&self.contract.owner_address, "ownerAddress")?;
        if self.contract.new_contract.is_none() {
            return Err(ActuatorError::Validate("No contract body".into()));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let sc = self
            .contract
            .new_contract
            .as_ref()
            .ok_or_else(|| ActuatorError::Execute("No contract body".into()))?;
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let addr = parse_address(&sc.contract_address, "contractAddress")
            .or_else(|_| parse_address(&self.contract.owner_address, "ownerAddress"))?;
        let init = sc.bytecode.clone();
        let price = energy_price(state);
        let body = body20(&addr);

        // Run the constructor on a state-backed World; capture its RETURN (runtime code)
        // and constructor storage writes.
        let (success, energy_used, return_data, writes) = {
            let backend = WorldStateBackend::new(state);
            let mut world = World::with_state(&backend);
            let r = execute_code(&mut world, &body, &init, &[], DEPLOY_ENERGY_LIMIT, 0);
            (r.success, r.energy_used, r.return_data, WorldWrites::from_world(&world))
        };

        if !success {
            let fee = tron_vm::energy_to_sun(energy_used, price);
            charge_energy_fee(state, &owner, fee)?;
            return Err(ActuatorError::Execute("contract constructor reverted".into()));
        }

        // Runtime code = constructor RETURN output, else the provided bytecode (deviation).
        let runtime = if return_data.is_empty() { sc.bytecode.clone() } else { return_data };
        flush_world(state, &writes).map_err(ActuatorError::from)?;
        // Persist the full SmartContract record (java-tron ContractStore) + runtime code.
        let mut record = sc.clone();
        record.contract_address = addr.as_bytes().to_vec();
        state.put_contract(&addr, &record).map_err(ActuatorError::from)?;
        state.put_code(&addr, &runtime).map_err(ActuatorError::from)?;
        let fee = tron_vm::energy_to_sun(energy_used, price);
        charge_energy_fee(state, &owner, fee)?;
        Ok(ExecutionResult { fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm_host::StateHost;
    use primitive_types::U256;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_vm::interp::Host;
    use tron_vm::opcode::OpCode::*;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }
    /// 21-byte address whose 20-byte body is all zero except byte 19 = `lo` (matches how
    /// the engine derives a callee address from the low byte of a CALL word).
    fn addr_lo(lo: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = lo;
        Address::from_body(b)
    }
    fn seed_owner(ws: &mut WorldState<MemoryStore>, o: &Address, bal: i64) {
        ws.put_account(o, &protocol::Account { address: o.as_bytes().to_vec(), balance: bal, ..Default::default() })
            .unwrap();
    }
    fn trigger(owner: &Address, target: &Address, value: i64) -> TriggerSmartContract {
        TriggerSmartContract {
            owner_address: owner.as_bytes().to_vec(),
            contract_address: target.as_bytes().to_vec(),
            call_value: value,
            ..Default::default()
        }
    }
    /// 7-arg CALL to one-byte address `lo` forwarding all gas (0xffff).
    fn call_bytes(lo: u8) -> Vec<u8> {
        vec![
            Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0,
            Push1 as u8, lo, Push2 as u8, 0xff, 0xff, Call as u8,
        ]
    }

    #[test]
    fn trigger_runs_stored_bytecode_and_persists_storage() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0xcc);
        ws.put_code(&contract, &[Push1 as u8, 5, Push1 as u8, 9, Sstore as u8, Stop as u8]).unwrap();
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 100_000_000);

        let t = trigger(&owner, &contract, 0);
        let act = TriggerSmartContractActuator::new(&t, 1_000_000);
        assert_eq!(act.validate(&ws).unwrap(), 0);
        let res = act.execute(&mut ws).unwrap();
        assert!(res.fee > 0);
        assert_eq!(ws.get_account(&owner).unwrap().unwrap().balance, 100_000_000 - res.fee);
        // storage[9] = 5 flushed to the contract's storage.
        let host = StateHost::new(&mut ws, contract);
        assert_eq!(host.sload(U256::from(9)), U256::from(5));
    }

    #[test]
    fn trigger_missing_contract_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let t = trigger(&addr(1), &addr(0xcc), 0);
        assert!(matches!(
            TriggerSmartContractActuator::new(&t, 1_000_000).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn sstore_persists_and_is_read_back_across_executions() {
        // Exec 1 on A writes storage[9]=5; exec 2 (A's code swapped) SLOADs 9 (read
        // through to state) and SSTOREs it to slot 8 -> proves flush + read-through.
        let mut ws = WorldState::new(MemoryStore::new());
        let a = addr(0xaa);
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 100_000_000);

        ws.put_code(&a, &[Push1 as u8, 5, Push1 as u8, 9, Sstore as u8, Stop as u8]).unwrap();
        let t1 = trigger(&owner, &a, 0);
        TriggerSmartContractActuator::new(&t1, 1_000_000).execute(&mut ws).unwrap();
        assert_eq!(StateHost::new(&mut ws, a).sload(U256::from(9)), U256::from(5));

        // Swap A's code: SLOAD 9 -> SSTORE 8.
        ws.put_code(&a, &[Push1 as u8, 9, Sload as u8, Push1 as u8, 8, Sstore as u8, Stop as u8]).unwrap();
        let t2 = trigger(&owner, &a, 0);
        TriggerSmartContractActuator::new(&t2, 1_000_000).execute(&mut ws).unwrap();
        assert_eq!(StateHost::new(&mut ws, a).sload(U256::from(8)), U256::from(5));
    }

    #[test]
    fn nested_call_commits_callee_storage_with_parent() {
        let mut ws = WorldState::new(MemoryStore::new());
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 100_000_000);
        let a = addr(0xaa);
        let b = addr_lo(0xbb);
        // B writes storage[3]=7; A calls B then STOPs (commits).
        ws.put_code(&b, &[Push1 as u8, 7, Push1 as u8, 3, Sstore as u8, Stop as u8]).unwrap();
        let mut acode = call_bytes(0xbb);
        acode.push(Stop as u8);
        ws.put_code(&a, &acode).unwrap();

        let t = trigger(&owner, &a, 0);
        TriggerSmartContractActuator::new(&t, 2_000_000).execute(&mut ws).unwrap();
        // Callee B's write persisted through the parent's success.
        assert_eq!(StateHost::new(&mut ws, b).sload(U256::from(3)), U256::from(7));
    }

    #[test]
    fn reverting_parent_rolls_back_callee_storage_but_charges_energy() {
        let mut ws = WorldState::new(MemoryStore::new());
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 100_000_000);
        let a = addr(0xaa);
        let b = addr_lo(0xbb);
        ws.put_code(&b, &[Push1 as u8, 7, Push1 as u8, 3, Sstore as u8, Stop as u8]).unwrap();
        // A calls B (which succeeds) then REVERTs -> whole tx rolled back.
        let mut acode = call_bytes(0xbb);
        acode.extend([Push1 as u8, 0, Push1 as u8, 0, Revert as u8]);
        ws.put_code(&a, &acode).unwrap();

        let t = trigger(&owner, &a, 0);
        let res = TriggerSmartContractActuator::new(&t, 2_000_000).execute(&mut ws).unwrap();
        // Energy charged (the callee's SSTORE + calls were metered) ...
        assert!(res.fee > 0);
        assert_eq!(ws.get_account(&owner).unwrap().unwrap().balance, 100_000_000 - res.fee);
        // ... but the callee's storage write did NOT persist (parent reverted).
        assert_eq!(StateHost::new(&mut ws, b).sload(U256::from(3)), U256::zero());
    }

    #[test]
    fn call_value_moves_trx() {
        let mut ws = WorldState::new(MemoryStore::new());
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 1_000);
        let contract = addr(0xcc);
        ws.put_code(&contract, &[Stop as u8]).unwrap(); // no-op contract

        let t = trigger(&owner, &contract, 200);
        let res = TriggerSmartContractActuator::new(&t, 1_000_000).execute(&mut ws).unwrap();
        assert_eq!(res.fee, 0); // STOP consumes ~no energy
        assert_eq!(ws.get_account(&owner).unwrap().unwrap().balance, 800); // -200 value
        assert_eq!(ws.get_account(&contract).unwrap().unwrap().balance, 200);
    }

    #[test]
    fn top_level_revert_charges_energy_and_rolls_back_state() {
        let mut ws = WorldState::new(MemoryStore::new());
        let owner = addr(1);
        seed_owner(&mut ws, &owner, 100_000_000);
        let contract = addr(0xdd);
        // SSTORE 9=5 then REVERT -> the write must NOT persist; energy still charged.
        ws.put_code(&contract, &[Push1 as u8, 5, Push1 as u8, 9, Sstore as u8, Push1 as u8, 0, Push1 as u8, 0, Revert as u8]).unwrap();
        let t = trigger(&owner, &contract, 0);
        let res = TriggerSmartContractActuator::new(&t, 1_000_000).execute(&mut ws).unwrap();
        assert!(res.fee > 0, "SSTORE energy charged even though reverted");
        assert_eq!(ws.get_account(&owner).unwrap().unwrap().balance, 100_000_000 - res.fee);
        assert_eq!(StateHost::new(&mut ws, contract).sload(U256::from(9)), U256::zero());
    }

    #[test]
    fn create_deploys_bytecode() {
        let mut ws = WorldState::new(MemoryStore::new());
        let target = addr(0xab);
        let create = CreateSmartContract {
            owner_address: addr(1).as_bytes().to_vec(),
            new_contract: Some(protocol::SmartContract {
                contract_address: target.as_bytes().to_vec(),
                bytecode: vec![0x00], // STOP -> no RETURN -> deploy as-is
                ..Default::default()
            }),
            ..Default::default()
        };
        let act = CreateSmartContractActuator::new(&create);
        act.validate(&ws).unwrap();
        act.execute(&mut ws).unwrap();
        assert_eq!(ws.get_code(&target).unwrap(), vec![0x00]);
    }

    #[test]
    fn create_persists_contract_record() {
        let mut ws = WorldState::new(MemoryStore::new());
        let target = addr(0xab);
        let create = CreateSmartContract {
            owner_address: addr(1).as_bytes().to_vec(),
            new_contract: Some(protocol::SmartContract {
                contract_address: target.as_bytes().to_vec(),
                bytecode: vec![0x60, 0x00],
                origin_energy_limit: 7_000_000,
                consume_user_resource_percent: 40,
                ..Default::default()
            }),
            ..Default::default()
        };
        let act = CreateSmartContractActuator::new(&create);
        act.validate(&ws).unwrap();
        act.execute(&mut ws).unwrap();

        let record = ws.get_contract(&target).unwrap().expect("record persisted");
        assert_eq!(record.origin_energy_limit, 7_000_000);
        assert_eq!(record.consume_user_resource_percent, 40);
        assert_eq!(record.contract_address, target.as_bytes().to_vec());
    }
}
