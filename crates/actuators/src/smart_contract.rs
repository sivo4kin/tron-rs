//! Smart-contract execution path (P1↔P2 integration).
//!
//! `TriggerSmartContract` loads the target contract's deployed bytecode and runs
//! it on the TVM ([`tron_vm::call::call`]) against that contract's persistent
//! storage (via [`crate::vm_host::StateHost`]), metering energy. `CreateSmartContract`
//! deploys bytecode to the new contract address.
//!
//! This is a real executor→VM bridge. Full parity (calldata/value/CALL frames,
//! energy price→sun accounting, deploy address derivation) is layered in later;
//! deviations are documented.

use crate::vm_host::StateHost;
use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::{CreateSmartContract, TriggerSmartContract};
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};
use tron_vm::call::call;

fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))
}

/// Execute a `TriggerSmartContract`: run the target contract's code on the VM.
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
        let code = state.get_code(&target).map_err(ActuatorError::from)?;
        let input = self.contract.data.clone();

        // Energy price (sun/energy) from dynamic properties, default 100.
        let energy_price = {
            let p = state.get_prop_i64("ENERGY_FEE").unwrap_or(0);
            if p > 0 { p } else { tron_vm::DEFAULT_ENERGY_FEE_SUN }
        };

        // Non-precompile address -> run bytecode against the contract's storage.
        let energy_used = {
            let mut host = StateHost::new(state, target);
            let result = call(0x00, &input, &code, self.energy_limit, &mut host);
            if !result.success {
                return Err(ActuatorError::Execute("contract execution reverted".into()));
            }
            result.energy_used
        };

        // Charge the caller energy_used * energy_price sun (java-tron energy fee).
        let fee = tron_vm::energy_to_sun(energy_used, energy_price);
        if fee > 0 {
            let mut owner_account = state
                .get_account(&owner)?
                .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
            owner_account.balance = owner_account
                .balance
                .checked_sub(fee)
                .filter(|b| *b >= 0)
                .ok_or_else(|| ActuatorError::Execute("insufficient balance for energy fee".into()))?;
            state.put_account(&owner, &owner_account)?;
            state.burn_trx(fee)?;
        }
        Ok(ExecutionResult { fee })
    }
}

/// Deploy a `CreateSmartContract`: store the bytecode at the new contract address.
///
/// Deviation: java-tron derives the contract address from the tx and runs the
/// constructor; here we store the provided bytecode at the owner-derived address
/// (address derivation + constructor execution are a documented follow-up).
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
        let addr = parse_address(&sc.contract_address, "contractAddress")
            .or_else(|_| parse_address(&self.contract.owner_address, "ownerAddress"))?;
        state.put_code(&addr, &sc.bytecode).map_err(ActuatorError::from)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use primitive_types::U256;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_vm::interp::Host;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    #[test]
    fn trigger_runs_stored_bytecode_and_persists_storage() {
        use tron_vm::opcode::OpCode::*;
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0xcc);
        // deploy: SSTORE 9 = 5, STOP
        let code = vec![Push1 as u8, 5, Push1 as u8, 9, Sstore as u8, Stop as u8];
        ws.put_code(&contract, &code).unwrap();

        let owner = addr(1);
        ws.put_account(&owner, &protocol::Account {
            address: owner.as_bytes().to_vec(), balance: 100_000_000, ..Default::default()
        }).unwrap();
        let trigger = TriggerSmartContract {
            owner_address: owner.as_bytes().to_vec(),
            contract_address: contract.as_bytes().to_vec(),
            ..Default::default()
        };
        let act = TriggerSmartContractActuator::new(&trigger, 1_000_000);
        assert_eq!(act.validate(&ws).unwrap(), 0);
        let res = act.execute(&mut ws).unwrap();
        // energy fee charged = energy_used * 100 sun (> 0, dominated by SSTORE)
        assert!(res.fee > 0);
        assert_eq!(ws.get_account(&owner).unwrap().unwrap().balance, 100_000_000 - res.fee);

        // storage[9] = 5 persisted to the contract's storage
        let host = StateHost::new(&mut ws, contract);
        assert_eq!(host.sload(U256::from(9)), U256::from(5));
    }

    #[test]
    fn trigger_missing_contract_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let trigger = TriggerSmartContract {
            owner_address: addr(1).as_bytes().to_vec(),
            contract_address: addr(0xcc).as_bytes().to_vec(),
            ..Default::default()
        };
        assert!(matches!(
            TriggerSmartContractActuator::new(&trigger, 1_000_000).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn create_deploys_bytecode() {
        let mut ws = WorldState::new(MemoryStore::new());
        let target = addr(0xab);
        let create = CreateSmartContract {
            owner_address: addr(1).as_bytes().to_vec(),
            new_contract: Some(protocol::SmartContract {
                contract_address: target.as_bytes().to_vec(),
                bytecode: vec![0x00],
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
    fn reverting_contract_fails_execution() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = addr(0xdd);
        ws.put_code(&contract, &[tron_vm::opcode::OpCode::Revert as u8]).unwrap();
        let trigger = TriggerSmartContract {
            owner_address: addr(1).as_bytes().to_vec(),
            contract_address: contract.as_bytes().to_vec(),
            ..Default::default()
        };
        let act = TriggerSmartContractActuator::new(&trigger, 1_000_000);
        assert!(act.execute(&mut ws).is_err());
    }
}
