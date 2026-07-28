//! `WithdrawBalanceContract` — claim accumulated reward allowance to balance.
//!
//! Mirrors java-tron `WithdrawBalanceActuator`: an SR/voter withdraws its
//! mortgage `allowance` (accrued by [`tron_consensus::reward`]) into its spendable
//! `balance`, at most once per withdraw window (default 24h). Execute moves the
//! allowance and stamps `latest_withdraw_time`.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::WithdrawBalanceContract;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Minimum interval between withdrawals (java-tron `WITHDRAW_BALANCE_LIMIT`, 24h ms).
pub const WITHDRAW_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

pub struct WithdrawBalanceActuator<'a> {
    contract: &'a WithdrawBalanceContract,
}

impl<'a> WithdrawBalanceActuator<'a> {
    pub fn new(contract: &'a WithdrawBalanceContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist".into()))?;
        if account.allowance <= 0 {
            return Err(ActuatorError::Validate("witnessAccount does not have any allowance".into()));
        }
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        if now - account.latest_withdraw_time < WITHDRAW_INTERVAL_MS {
            return Err(ActuatorError::Validate(
                "The last withdraw time is less than 24 hours".into(),
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
        let allowance = account.allowance;
        account.balance = account
            .balance
            .checked_add(allowance)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        account.allowance = 0;
        account.latest_withdraw_time = now;
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

    fn seed(owner: &Address, allowance: i64, last_withdraw: i64, now: i64) -> WorldState<MemoryStore> {
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();
        ws.put_account(owner, &protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance: 1_000,
            allowance,
            latest_withdraw_time: last_withdraw,
            ..Default::default()
        }).unwrap();
        ws
    }

    fn contract(owner: &Address) -> WithdrawBalanceContract {
        WithdrawBalanceContract { owner_address: owner.as_bytes().to_vec() }
    }

    #[test]
    fn withdraw_moves_allowance_to_balance() {
        let o = addr(1);
        let now = 2 * WITHDRAW_INTERVAL_MS;
        let mut ws = seed(&o, 500_000, 0, now);
        let c = contract(&o);
        let act = WithdrawBalanceActuator::new(&c);
        assert_eq!(act.validate(&ws).unwrap(), 0);
        act.execute(&mut ws).unwrap();
        let a = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(a.balance, 500_000 + 1_000);
        assert_eq!(a.allowance, 0);
        assert_eq!(a.latest_withdraw_time, now);
    }

    #[test]
    fn rejects_zero_allowance() {
        let o = addr(1);
        let ws = seed(&o, 0, 0, WITHDRAW_INTERVAL_MS * 2);
        assert!(matches!(
            WithdrawBalanceActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("allowance")
        ));
    }

    #[test]
    fn rejects_within_24h() {
        let o = addr(1);
        // last withdraw at 1000, now 1000 + 1h < 24h
        let ws = seed(&o, 500, 1000, 1000 + 60 * 60 * 1000);
        assert!(matches!(
            WithdrawBalanceActuator::new(&contract(&o)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("24 hours")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(WithdrawBalanceActuator::new(&contract(&addr(1))).validate(&ws).is_err());
    }
}
