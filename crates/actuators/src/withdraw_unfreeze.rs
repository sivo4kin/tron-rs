//! `WithdrawExpireUnfreezeContract` — claim matured Stake 2.0 unfrozen balances.
//!
//! Mirrors java-tron `WithdrawExpireUnfreezeActuator`: sum the account's
//! `unfrozen_v2` entries whose `unfreeze_expire_time <= now`, add them back to
//! spendable `balance`, and drop those entries. Non-matured entries stay pending.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::WithdrawExpireUnfreezeContract;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct WithdrawExpireUnfreezeActuator<'a> {
    contract: &'a WithdrawExpireUnfreezeContract,
}

impl<'a> WithdrawExpireUnfreezeActuator<'a> {
    pub fn new(contract: &'a WithdrawExpireUnfreezeContract) -> Self {
        Self { contract }
    }

    fn matured_total<S: KvStore>(&self, state: &WorldState<S>) -> Result<(Address, i64), ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist".into()))?;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let total: i64 = account
            .unfrozen_v2
            .iter()
            .filter(|u| u.unfreeze_expire_time <= now)
            .map(|u| u.unfreeze_amount)
            .sum();
        Ok((owner, total))
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let (_, total) = self.matured_total(state)?;
        if total <= 0 {
            return Err(ActuatorError::Validate(
                "no unFreeze balance to withdraw".into(),
            ));
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

        let (matured, remaining): (Vec<_>, Vec<_>) = account
            .unfrozen_v2
            .iter()
            .cloned()
            .partition(|u| u.unfreeze_expire_time <= now);
        let total: i64 = matured.iter().map(|u| u.unfreeze_amount).sum();

        account.balance = account
            .balance
            .checked_add(total)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        account.unfrozen_v2 = remaining;
        state.put_account(&owner, &account)?;
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

    fn unfreeze(amount: i64, expire: i64) -> protocol::account::UnFreezeV2 {
        protocol::account::UnFreezeV2 { r#type: 0, unfreeze_amount: amount, unfreeze_expire_time: expire }
    }

    fn seed(owner: &Address, entries: Vec<protocol::account::UnFreezeV2>, now: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();
        ws.put_account(owner, &protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance: 1_000,
            unfrozen_v2: entries,
            ..Default::default()
        }).unwrap();
        ws
    }

    fn contract(owner: &Address) -> WithdrawExpireUnfreezeContract {
        WithdrawExpireUnfreezeContract { owner_address: owner.as_bytes().to_vec() }
    }

    #[test]
    fn withdraws_only_matured_entries() {
        let o = addr(1);
        // now = 100: two matured (expire 50, 80), one pending (expire 200)
        let mut ws = seed(&o, vec![unfreeze(300, 50), unfreeze(200, 80), unfreeze(500, 200)], 100);
        let c = contract(&o);
        let a = WithdrawExpireUnfreezeActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        let acc = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acc.balance, 1_000 + 300 + 200); // matured added
        assert_eq!(acc.unfrozen_v2.len(), 1); // pending entry remains
        assert_eq!(acc.unfrozen_v2[0].unfreeze_amount, 500);
    }

    #[test]
    fn rejects_when_nothing_matured() {
        let o = addr(1);
        let ws = seed(&o, vec![unfreeze(300, 200)], 100); // not yet expired
        assert!(matches!(
            WithdrawExpireUnfreezeActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no unFreeze balance")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(WithdrawExpireUnfreezeActuator::new(&contract(&addr(1))).validate(&ws).is_err());
    }
}
