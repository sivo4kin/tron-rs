//! `UpdateSettingContract` — update a contract's `consume_user_resource_percent`.
//!
//! Mirrors java-tron `UpdateSettingContractActuator`: the owner account must
//! exist, the new percent must be in `[0, 100]`, the contract record must exist,
//! and the owner must be the contract's owner (`SmartContract.origin_address`).
//! Execute writes the new percent onto the stored record.
//!
//! The record is stored inline and keyed by the stub `contract_address` (I01).

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::UpdateSettingContract;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct UpdateSettingActuator<'a> {
    contract: &'a UpdateSettingContract,
}

impl<'a> UpdateSettingActuator<'a> {
    pub fn new(contract: &'a UpdateSettingContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let contract_addr = parse_address(&self.contract.contract_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                owner.to_hex()
            )));
        }

        let new_percent = self.contract.consume_user_resource_percent;
        if !(0..=100).contains(&new_percent) {
            return Err(ActuatorError::Validate("percent not in [0, 100]".into()));
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
        record.consume_user_resource_percent = self.contract.consume_user_resource_percent;
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
            consume_user_resource_percent: 50,
            ..Default::default()
        };
        ws.put_contract(contract, &record).unwrap();
        ws
    }

    fn contract(owner: &Address, c: &Address, percent: i64) -> UpdateSettingContract {
        UpdateSettingContract {
            owner_address: owner.as_bytes().to_vec(),
            contract_address: c.as_bytes().to_vec(),
            consume_user_resource_percent: percent,
        }
    }

    #[test]
    fn happy_path_sets_percent() {
        let (o, c) = (addr(1), addr(9));
        let mut ws = seeded(&o, &c, &o);
        let ct = contract(&o, &c, 30);
        let a = UpdateSettingActuator::new(&ct);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(
            ws.get_contract(&c).unwrap().unwrap().consume_user_resource_percent,
            30
        );
    }

    #[test]
    fn accepts_boundaries_0_and_100() {
        let (o, c) = (addr(1), addr(9));
        let mut ws = seeded(&o, &c, &o);
        for p in [0, 100] {
            let ct = contract(&o, &c, p);
            let a = UpdateSettingActuator::new(&ct);
            a.validate(&ws).unwrap();
            a.execute(&mut ws).unwrap();
            assert_eq!(ws.get_contract(&c).unwrap().unwrap().consume_user_resource_percent, p);
        }
    }

    #[test]
    fn rejects_out_of_range() {
        let (o, c) = (addr(1), addr(9));
        let ws = seeded(&o, &c, &o);
        for p in [-1, 101] {
            assert!(matches!(
                UpdateSettingActuator::new(&contract(&o, &c, p)).validate(&ws),
                Err(ActuatorError::Validate(m)) if m.contains("percent not in [0, 100]")
            ), "percent {p} must be rejected");
        }
    }

    #[test]
    fn rejects_non_owner() {
        let (o, c, other) = (addr(1), addr(9), addr(2));
        let ws = seeded(&o, &c, &other);
        assert!(matches!(
            UpdateSettingActuator::new(&contract(&o, &c, 30)).validate(&ws),
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
            UpdateSettingActuator::new(&contract(&o, &c, 30)).validate(&ws),
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
            UpdateSettingActuator::new(&contract(&o, &c, 30)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }
}
