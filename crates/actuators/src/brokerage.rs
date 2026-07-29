//! `UpdateBrokerageContract` — a witness sets its brokerage (voter-reward cut) %.
//!
//! Mirrors java-tron `UpdateBrokerageActuator`: the owner must be a witness
//! (present in the witness store), and the brokerage must be 0..=100. Execute
//! stores the new percentage; [`tron_consensus::reward::split_reward`] then uses it.

use crate::{require_feature, ActuatorError, ExecutionResult};
use tron_proto::protocol::UpdateBrokerageContract;
use tron_state::features::flags;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

const CF_WITNESS: &str = tron_state::cf::WITNESS;

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct UpdateBrokerageActuator<'a> {
    contract: &'a UpdateBrokerageContract,
}

impl<'a> UpdateBrokerageActuator<'a> {
    pub fn new(contract: &'a UpdateBrokerageContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        // java gates UpdateBrokerage on allowChangeDelegation (CHANGE_DELEGATION == 1).
        require_feature(
            state,
            flags::CHANGE_DELEGATION,
            "contract type error, unexpected type [UpdateBrokerageContract]",
        )?;
        let owner = parse_address(&self.contract.owner_address)?;
        let b = self.contract.brokerage;
        if !(0..=100).contains(&b) {
            return Err(ActuatorError::Validate(
                "brokerage must be in the range of [0, 100]".into(),
            ));
        }
        let is_witness = state
            .db
            .exists(CF_WITNESS, owner.as_bytes())
            .map_err(|e| ActuatorError::State(e.to_string()))?;
        if !is_witness {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] is not a witness",
                owner.to_hex()
            )));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        state.put_brokerage(&owner, self.contract.brokerage as i64)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn with_witness(owner: &Address) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(flags::CHANGE_DELEGATION, 1).unwrap(); // gate on
        let w = protocol::Witness { address: owner.as_bytes().to_vec(), ..Default::default() };
        ws.db.put(CF_WITNESS, owner.as_bytes(), &w.encode_to_vec()).unwrap();
        ws
    }

    fn contract(owner: &Address, b: i32) -> UpdateBrokerageContract {
        UpdateBrokerageContract { owner_address: owner.as_bytes().to_vec(), brokerage: b }
    }

    #[test]
    fn witness_sets_brokerage() {
        let o = addr(1);
        let mut ws = with_witness(&o);
        assert_eq!(ws.get_brokerage(&o).unwrap(), 20); // default
        let c = contract(&o, 30);
        let a = UpdateBrokerageActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_brokerage(&o).unwrap(), 30);
    }

    #[test]
    fn rejects_out_of_range() {
        let o = addr(1);
        let ws = with_witness(&o);
        for b in [-1, 101] {
            assert!(matches!(
                UpdateBrokerageActuator::new(&contract(&o, b)).validate(&ws),
                Err(ActuatorError::Validate(m)) if m.contains("range")
            ));
        }
    }

    #[test]
    fn rejects_non_witness() {
        let o = addr(1);
        let ws = WorldState::new(MemoryStore::new()); // no witness
        ws.put_prop_i64(flags::CHANGE_DELEGATION, 1).unwrap(); // gate on
        assert!(matches!(
            UpdateBrokerageActuator::new(&contract(&o, 30)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not a witness")
        ));
    }

    #[test]
    fn rejects_when_feature_disabled() {
        let o = addr(1);
        let ws = with_witness(&o);
        ws.put_prop_i64(flags::CHANGE_DELEGATION, 0).unwrap();
        assert!(matches!(
            UpdateBrokerageActuator::new(&contract(&o, 30)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("unexpected type [UpdateBrokerageContract]")
        ));
    }
}
