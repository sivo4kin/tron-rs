//! `UpdateEnergyLimitContract` — update a contract's `origin_energy_limit`.
//!
//! Mirrors java-tron `UpdateEnergyLimitContractActuator`: the owner account must
//! exist, `origin_energy_limit` must be `> 0`, the contract record must exist,
//! and the owner must be the contract's owner (`SmartContract.origin_address`).
//! Execute writes the new limit onto the stored record.
//!
//! The record is stored inline and keyed by the stub `contract_address` (I01).
//!
//! Deviations from java-tron:
//! - Feature gate not modelled — java rejects unless the energy-limit feature
//!   (`ReceiptCapsule.checkForEnergyLimit`) is enabled.
//!   See `// TODO(feature-gate): energy-limit / ALLOW_TVM_CONSTANTINOPLE`.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::UpdateEnergyLimitContract;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct UpdateEnergyLimitActuator<'a> {
    contract: &'a UpdateEnergyLimitContract,
}

impl<'a> UpdateEnergyLimitActuator<'a> {
    pub fn new(contract: &'a UpdateEnergyLimitContract) -> Self {
        Self { contract }
    }

    // TODO(feature-gate): energy-limit / ALLOW_TVM_CONSTANTINOPLE — java rejects
    // UpdateEnergyLimitContract unless the energy-limit feature is enabled.
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let contract_addr = parse_address(&self.contract.contract_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                owner.to_hex()
            )));
        }

        let new_limit = self.contract.origin_energy_limit;
        if new_limit <= 0 {
            return Err(ActuatorError::Validate("origin energy limit must be > 0".into()));
        }

        let record = state
            .get_contract(&contract_addr)?
            .ok_or_else(|| ActuatorError::Validate("Contract does not exist".into()))?;

        if record.origin_address.as_slice() != owner.as_bytes() {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] is not the owner of the contract",
                owner.to_hex()
            )));
        }

        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let contract_addr = parse_address(&self.contract.contract_address)?;
        let mut record = state
            .get_contract(&contract_addr)?
            .ok_or_else(|| ActuatorError::Execute("Contract does not exist".into()))?;
        record.origin_energy_limit = self.contract.origin_energy_limit;
        state.put_contract(&contract_addr, &record)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn seeded(owner: &Address, contract: &Address, origin: &Address) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            owner,
            &protocol::Account { address: owner.as_bytes().to_vec(), ..Default::default() },
        )
        .unwrap();
        let record = protocol::SmartContract {
            origin_address: origin.as_bytes().to_vec(),
            contract_address: contract.as_bytes().to_vec(),
            origin_energy_limit: 1_000,
            ..Default::default()
        };
        ws.put_contract(contract, &record).unwrap();
        ws
    }

    fn contract(owner: &Address, c: &Address, limit: i64) -> UpdateEnergyLimitContract {
        UpdateEnergyLimitContract {
            owner_address: owner.as_bytes().to_vec(),
            contract_address: c.as_bytes().to_vec(),
            origin_energy_limit: limit,
        }
    }

    #[test]
    fn happy_path_sets_limit() {
        let (o, c) = (addr(1), addr(9));
        let mut ws = seeded(&o, &c, &o);
        let ct = contract(&o, &c, 5_000_000);
        let a = UpdateEnergyLimitActuator::new(&ct);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_contract(&c).unwrap().unwrap().origin_energy_limit, 5_000_000);
    }

    #[test]
    fn rejects_non_positive_limit() {
        let (o, c) = (addr(1), addr(9));
        let ws = seeded(&o, &c, &o);
        for limit in [0, -1, i64::MIN] {
            assert!(matches!(
                UpdateEnergyLimitActuator::new(&contract(&o, &c, limit)).validate(&ws),
                Err(ActuatorError::Validate(m)) if m.contains("origin energy limit must be > 0")
            ), "limit {limit} must be rejected");
        }
    }

    #[test]
    fn rejects_non_owner() {
        let (o, c, other) = (addr(1), addr(9), addr(2));
        let ws = seeded(&o, &c, &other);
        assert!(matches!(
            UpdateEnergyLimitActuator::new(&contract(&o, &c, 5_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is not the owner of the contract")
        ));
    }

    #[test]
    fn rejects_missing_contract() {
        let (o, c) = (addr(1), addr(9));
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(&o, &protocol::Account { address: o.as_bytes().to_vec(), ..Default::default() })
            .unwrap();
        assert!(matches!(
            UpdateEnergyLimitActuator::new(&contract(&o, &c, 5_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Contract does not exist")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let (o, c) = (addr(1), addr(9));
        let ws = {
            let ws = WorldState::new(MemoryStore::new());
            let record = protocol::SmartContract {
                origin_address: o.as_bytes().to_vec(),
                contract_address: c.as_bytes().to_vec(),
                ..Default::default()
            };
            ws.put_contract(&c, &record).unwrap();
            ws
        };
        assert!(matches!(
            UpdateEnergyLimitActuator::new(&contract(&o, &c, 5_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }
}
