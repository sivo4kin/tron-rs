//! `CancelAllUnfreezeV2Contract` — re-freeze all pending Stake 2.0 unfreezes.
//!
//! Mirrors java-tron `CancelAllUnfreezeV2Actuator`: matured `unfrozen_v2` entries
//! are withdrawn to balance, and still-pending ones are returned to `frozen_v2`
//! (by resource type); the unfrozen queue is cleared. Requires at least one
//! pending unfreeze entry.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::account::FreezeV2;
use tron_proto::protocol::CancelAllUnfreezeV2Contract;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct CancelAllUnfreezeV2Actuator<'a> {
    contract: &'a CancelAllUnfreezeV2Contract,
}

impl<'a> CancelAllUnfreezeV2Actuator<'a> {
    pub fn new(contract: &'a CancelAllUnfreezeV2Contract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist".into()))?;
        if account.unfrozen_v2.is_empty() {
            return Err(ActuatorError::Validate("no unfreezeV2 list to cancel".into()));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        let mut withdraw = 0i64;
        for u in account.unfrozen_v2.drain(..).collect::<Vec<_>>() {
            if u.unfreeze_expire_time <= now {
                // matured -> withdraw to balance
                withdraw += u.unfreeze_amount;
            } else {
                // pending -> return to frozen_v2 of the same resource type
                match account.frozen_v2.iter_mut().find(|f| f.r#type == u.r#type) {
                    Some(f) => f.amount += u.unfreeze_amount,
                    None => account.frozen_v2.push(FreezeV2 { r#type: u.r#type, amount: u.unfreeze_amount }),
                }
            }
        }
        account.balance = account
            .balance
            .checked_add(withdraw)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        state.put_account(&owner, &account)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_proto::protocol::account::UnFreezeV2;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn u(amount: i64, expire: i64, ty: i32) -> UnFreezeV2 {
        UnFreezeV2 { r#type: ty, unfreeze_amount: amount, unfreeze_expire_time: expire }
    }

    fn seed(owner: &Address, entries: Vec<UnFreezeV2>, now: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();
        ws.put_account(owner, &protocol::Account {
            address: owner.as_bytes().to_vec(), balance: 1_000, unfrozen_v2: entries, ..Default::default()
        }).unwrap();
        ws
    }

    fn contract(owner: &Address) -> CancelAllUnfreezeV2Contract {
        CancelAllUnfreezeV2Contract { owner_address: owner.as_bytes().to_vec() }
    }

    #[test]
    fn matured_withdrawn_pending_refrozen() {
        let o = addr(1);
        // now=100: one matured (expire 50), one pending (expire 200, type 0)
        let mut ws = seed(&o, vec![u(300, 50, 0), u(500, 200, 0)], 100);
        let c = contract(&o);
        let a = CancelAllUnfreezeV2Actuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        let acc = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acc.balance, 1_000 + 300); // matured withdrawn
        assert!(acc.unfrozen_v2.is_empty()); // queue cleared
        assert_eq!(acc.frozen_v2.iter().find(|f| f.r#type == 0).unwrap().amount, 500); // pending re-frozen
    }

    #[test]
    fn rejects_when_no_unfreeze_queue() {
        let o = addr(1);
        let ws = seed(&o, vec![], 100);
        assert!(matches!(
            CancelAllUnfreezeV2Actuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no unfreezeV2")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(CancelAllUnfreezeV2Actuator::new(&contract(&addr(1))).validate(&ws).is_err());
    }
}
