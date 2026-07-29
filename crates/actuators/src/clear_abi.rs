//! `ClearABIContract` — clear a deployed contract's ABI.
//!
//! Mirrors java-tron `ClearABIContractActuator`: the owner account must exist,
//! the contract record must exist at `contract_address`, and the owner must be
//! the contract's owner (`SmartContract.origin_address`). Execute clears the
//! stored `SmartContract.abi`.
//!
//! The record is stored inline with its ABI (see I01 deviations), so we clear
//! `record.abi` directly and `put_contract`, rather than writing a separate
//! AbiStore. The contract is keyed by whatever `contract_address` the record
//! holds (I01's stub derivation).
//!
//! Deviations from java-tron:
//! - Feature gate not modelled — java rejects unless `allowTvmConstantinople`.
//!   Gated on `ALLOW_TVM_CONSTANTINOPLE` (java `getAllowTvmConstantinople`).

use crate::{require_feature, ActuatorError, ExecutionResult};
use tron_proto::protocol::ClearAbiContract;
use tron_state::features::flags;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct ClearAbiActuator<'a> {
    contract: &'a ClearAbiContract,
}

impl<'a> ClearAbiActuator<'a> {
    pub fn new(contract: &'a ClearAbiContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        // java rejects ClearABIContract unless the contract-state feature is on.
        require_feature(
            state,
            flags::ALLOW_TVM_CONSTANTINOPLE,
            "contract type error,unexpected type [ClearABIContract]",
        )?;

        let owner = parse_address(&self.contract.owner_address)?;
        let contract_addr = parse_address(&self.contract.contract_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] not exists",
                owner.to_hex()
            )));
        }

        let record = state
            .get_contract(&contract_addr)?
            .ok_or_else(|| ActuatorError::Validate("Contract not exists".into()))?;

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
            .ok_or_else(|| ActuatorError::Execute("Contract not exists".into()))?;
        record.abi = None;
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

    /// A store with the TVM-Constantinople feature enabled (gate precondition).
    fn feature_ws() -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(flags::ALLOW_TVM_CONSTANTINOPLE, 1).unwrap();
        ws
    }

    /// State with owner account and a contract record (owned by `origin`) whose
    /// ABI is non-empty, feature enabled.
    fn seeded(owner: &Address, contract: &Address, origin: &Address) -> WorldState<MemoryStore> {
        let ws = feature_ws();
        ws.put_account(
            owner,
            &protocol::Account { address: owner.as_bytes().to_vec(), ..Default::default() },
        )
        .unwrap();
        let record = protocol::SmartContract {
            origin_address: origin.as_bytes().to_vec(),
            contract_address: contract.as_bytes().to_vec(),
            abi: Some(protocol::smart_contract::Abi::default()),
            ..Default::default()
        };
        ws.put_contract(contract, &record).unwrap();
        ws
    }

    fn contract(owner: &Address, c: &Address) -> ClearAbiContract {
        ClearAbiContract {
            owner_address: owner.as_bytes().to_vec(),
            contract_address: c.as_bytes().to_vec(),
        }
    }

    #[test]
    fn happy_path_clears_abi() {
        let (o, c) = (addr(1), addr(9));
        let mut ws = seeded(&o, &c, &o);
        assert!(ws.get_contract(&c).unwrap().unwrap().abi.is_some());
        let ct = contract(&o, &c);
        let a = ClearAbiActuator::new(&ct);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert!(ws.get_contract(&c).unwrap().unwrap().abi.is_none());
    }

    #[test]
    fn rejects_missing_contract() {
        let (o, c) = (addr(1), addr(9));
        let ws = feature_ws();
        ws.put_account(&o, &protocol::Account { address: o.as_bytes().to_vec(), ..Default::default() })
            .unwrap();
        // no contract record at c
        assert!(matches!(
            ClearAbiActuator::new(&contract(&o, &c)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Contract not exists")
        ));
    }

    #[test]
    fn rejects_non_owner() {
        let (o, c, other) = (addr(1), addr(9), addr(2));
        // contract owned by `other`, not `o`.
        let ws = seeded(&o, &c, &other);
        assert!(matches!(
            ClearAbiActuator::new(&contract(&o, &c)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is not the owner of the contract")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let (o, c) = (addr(1), addr(9));
        let ws = feature_ws();
        let record = protocol::SmartContract {
            origin_address: o.as_bytes().to_vec(),
            contract_address: c.as_bytes().to_vec(),
            abi: Some(protocol::smart_contract::Abi::default()),
            ..Default::default()
        };
        ws.put_contract(&c, &record).unwrap();
        assert!(matches!(
            ClearAbiActuator::new(&contract(&o, &c)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn rejects_malformed_address() {
        let ws = feature_ws();
        let ct = ClearAbiContract {
            owner_address: vec![0x41; 20],
            contract_address: addr(9).as_bytes().to_vec(),
        };
        assert!(matches!(
            ClearAbiActuator::new(&ct).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    #[test]
    fn rejects_when_feature_disabled() {
        // Same happy-path setup but without ALLOW_TVM_CONSTANTINOPLE.
        let (o, c) = (addr(1), addr(9));
        let ws = seeded(&o, &c, &o);
        ws.put_prop_i64(flags::ALLOW_TVM_CONSTANTINOPLE, 0).unwrap();
        assert!(matches!(
            ClearAbiActuator::new(&contract(&o, &c)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("unexpected type [ClearABIContract]")
        ));
    }
}
