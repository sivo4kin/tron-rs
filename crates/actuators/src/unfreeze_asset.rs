//! `UnfreezeAssetContract` — return an owner's matured frozen TRC10 supply to
//! their spendable token balance.
//!
//! Mirrors java-tron `UnfreezeAssetActuator` (V2 path): the owner must exist,
//! hold at least one `frozen_supply` entry, have issued an asset, and have at
//! least one frozen entry whose `expire_time <= now`
//! ([`props::LATEST_BLOCK_HEADER_TIMESTAMP`]). Execute sums the matured entries
//! back into `Account.asset_v2[issued_id]` and drops them from `frozen_supply`.
//!
//! Deviations from java-tron: V2 asset store only (see I02).

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::UnfreezeAssetContract;
use tron_state::{asset_v2_balance, props, set_asset_v2_balance, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn issued_asset_id(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok().and_then(|s| s.parse::<i64>().ok())
}

pub struct UnfreezeAssetActuator<'a> {
    contract: &'a UnfreezeAssetContract,
}

impl<'a> UnfreezeAssetActuator<'a> {
    pub fn new(contract: &'a UnfreezeAssetContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("Account[{}] does not exist", hex(owner.as_bytes())))
        })?;

        if account.frozen_supply.is_empty() {
            return Err(ActuatorError::Validate("no frozen supply balance".into()));
        }

        if account.asset_issued_id.is_empty() {
            return Err(ActuatorError::Validate("this account has not issued any asset".into()));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let matured = account.frozen_supply.iter().any(|f| f.expire_time <= now);
        if !matured {
            return Err(ActuatorError::Validate("It's not time to unfreeze asset supply".into()));
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
        let id = issued_asset_id(&account.asset_issued_id)
            .ok_or_else(|| ActuatorError::Execute("owner has no issued asset".into()))?;

        // Split matured (expire_time <= now) from the still-frozen entries.
        let mut unfreeze_amount: i64 = 0;
        let mut remaining = Vec::with_capacity(account.frozen_supply.len());
        for frozen in &account.frozen_supply {
            if frozen.expire_time <= now {
                unfreeze_amount = unfreeze_amount
                    .checked_add(frozen.frozen_balance)
                    .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
            } else {
                remaining.push(*frozen);
            }
        }

        let new_balance = asset_v2_balance(&account, id)
            .checked_add(unfreeze_amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        set_asset_v2_balance(&mut account, id, new_balance);
        account.frozen_supply = remaining;
        state.put_account(&owner, &account)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_proto::protocol::account::Frozen;
    use tron_storage::MemoryStore;

    const ID: i64 = 1_000_001;
    const NOW: i64 = 1_700_000_000_000;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// Account that issued token ID, holding `asset_bal` spendable and the given
    /// frozen-supply entries (balance, expire_time).
    fn state_with(
        owner: &Address,
        asset_bal: i64,
        frozen: &[(i64, i64)],
    ) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        let mut account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            asset_issued_id: b"1000001".to_vec(),
            frozen_supply: frozen
                .iter()
                .map(|(bal, exp)| Frozen { frozen_balance: *bal, expire_time: *exp })
                .collect(),
            ..Default::default()
        };
        if asset_bal > 0 {
            set_asset_v2_balance(&mut account, ID, asset_bal);
        }
        ws.put_account(owner, &account).unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        ws
    }

    fn contract(owner: &Address) -> UnfreezeAssetContract {
        UnfreezeAssetContract { owner_address: owner.as_bytes().to_vec() }
    }

    #[test]
    fn matured_freeze_returns_to_balance() {
        let o = addr(1);
        // One matured (3000, expired) + one still frozen (5000, future).
        let mut ws = state_with(&o, 1_000, &[(3_000, NOW - 1), (5_000, NOW + 1_000)]);
        let c = contract(&o);
        let a = UnfreezeAssetActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let account = ws.get_account(&o).unwrap().unwrap();
        // 1000 + 3000 matured = 4000 spendable.
        assert_eq!(asset_v2_balance(&account, ID), 4_000);
        // Only the future entry remains frozen.
        assert_eq!(account.frozen_supply.len(), 1);
        assert_eq!(account.frozen_supply[0].frozen_balance, 5_000);
    }

    #[test]
    fn asset_balance_is_conserved() {
        let o = addr(1);
        let mut ws = state_with(&o, 2_000, &[(3_000, NOW - 1), (4_000, NOW - 5), (9_000, NOW + 10)]);
        let before = {
            let a = ws.get_account(&o).unwrap().unwrap();
            asset_v2_balance(&a, ID) + a.frozen_supply.iter().map(|f| f.frozen_balance).sum::<i64>()
        };
        UnfreezeAssetActuator::new(&contract(&o)).execute(&mut ws).unwrap();
        let after = {
            let a = ws.get_account(&o).unwrap().unwrap();
            asset_v2_balance(&a, ID) + a.frozen_supply.iter().map(|f| f.frozen_balance).sum::<i64>()
        };
        assert_eq!(before, after);
        // 2000 + 3000 + 4000 matured = 9000 spendable; 9000 still frozen.
        let account = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(asset_v2_balance(&account, ID), 9_000);
        assert_eq!(account.frozen_supply.iter().map(|f| f.frozen_balance).sum::<i64>(), 9_000);
    }

    #[test]
    fn rejects_when_nothing_matured() {
        let o = addr(1);
        // All entries in the future.
        let ws = state_with(&o, 0, &[(3_000, NOW + 1), (5_000, NOW + 100)]);
        assert!(matches!(
            UnfreezeAssetActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("It's not time to unfreeze asset supply")
        ));
    }

    #[test]
    fn rejects_no_frozen_supply() {
        let o = addr(1);
        let ws = state_with(&o, 0, &[]);
        assert!(matches!(
            UnfreezeAssetActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no frozen supply balance")
        ));
    }

    #[test]
    fn rejects_owner_without_issued_asset() {
        let o = addr(1);
        // Has frozen supply but never issued an asset.
        let ws = {
            let ws = WorldState::new(MemoryStore::new());
            let account = protocol::Account {
                address: o.as_bytes().to_vec(),
                frozen_supply: vec![Frozen { frozen_balance: 3_000, expire_time: NOW - 1 }],
                ..Default::default()
            };
            ws.put_account(&o, &account).unwrap();
            ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
            ws
        };
        assert!(matches!(
            UnfreezeAssetActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("has not issued any asset")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(matches!(
            UnfreezeAssetActuator::new(&contract(&addr(1))).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }
}
