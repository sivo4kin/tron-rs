//! Legacy Stake 1.0 — `FreezeBalanceContract` (11) and
//! `UnfreezeBalanceContract` (12).
//!
//! Mirrors java-tron `FreezeBalanceActuator` / `UnfreezeBalanceActuator` for the
//! **self** (non-delegated) path. Frozen balance is held in the V1 account
//! fields: bandwidth in `Account.frozen`, energy in
//! `Account.account_resource.frozen_balance_for_energy`, tron-power in
//! `Account.tron_power`. Each freeze bumps the matching global weight total
//! (`TOTAL_NET_WEIGHT` / `TOTAL_ENERGY_WEIGHT` / `TOTAL_TRON_POWER_WEIGHT`) by
//! `frozenBalance / TRX_PRECISION`; unfreeze drops it symmetrically — mirroring
//! the V2 wiring in [`crate::freeze_v2`].
//!
//! Deviations from java-tron (data-only):
//! - **Delegated V1 is scoped out**: a contract with `receiver_address` set is
//!   rejected with a typed error (historical delegate-resource path not modelled).
//! - The `supportUnfreezeDelay` gate (which disables V1 once Stake 2.0 is on) is
//!   not modelled, so V1 stays usable.
//! - `TRON_POWER` freeze is gated on the `ALLOW_NEW_RESOURCE_MODEL` dynamic
//!   property (java `supportAllowNewResourceModel`), mirroring `freeze_v2`.
//! - min/max frozen duration read `MIN_FROZEN_TIME`/`MAX_FROZEN_TIME` props,
//!   defaulting to java's genesis 3/3 days.

use crate::freeze_v2::{FROZEN_PERIOD_MS, TRX_PRECISION};
use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::account::Frozen;
use tron_proto::protocol::{Account, FreezeBalanceContract, ResourceCode, UnfreezeBalanceContract};
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

const ALLOW_NEW_RESOURCE_MODEL_KEY: &str = "ALLOW_NEW_RESOURCE_MODEL";
const MIN_FROZEN_TIME_KEY: &str = "MIN_FROZEN_TIME";
const MAX_FROZEN_TIME_KEY: &str = "MAX_FROZEN_TIME";
const DEFAULT_MIN_FROZEN_TIME: i64 = 3;
const DEFAULT_MAX_FROZEN_TIME: i64 = 3;

const BANDWIDTH: i32 = ResourceCode::Bandwidth as i32;
const ENERGY: i32 = ResourceCode::Energy as i32;
const TRON_POWER: i32 = ResourceCode::TronPower as i32;

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

fn weight_key(resource: i32) -> &'static str {
    match resource {
        ENERGY => props::TOTAL_ENERGY_WEIGHT,
        TRON_POWER => props::TOTAL_TRON_POWER_WEIGHT,
        _ => props::TOTAL_NET_WEIGHT,
    }
}

/// Current V1 frozen balance (sun) held for `resource`.
fn frozen_balance(account: &Account, resource: i32) -> i64 {
    match resource {
        ENERGY => account
            .account_resource
            .as_ref()
            .and_then(|r| r.frozen_balance_for_energy.as_ref())
            .map(|f| f.frozen_balance)
            .unwrap_or(0),
        TRON_POWER => account.tron_power.as_ref().map(|f| f.frozen_balance).unwrap_or(0),
        _ => account.frozen.iter().map(|f| f.frozen_balance).sum(),
    }
}

/// Overwrite the V1 frozen entry for `resource` with `(balance, expire_time)`.
fn set_frozen(account: &mut Account, resource: i32, balance: i64, expire_time: i64) {
    let frozen = Frozen { frozen_balance: balance, expire_time };
    match resource {
        ENERGY => {
            let mut res = account.account_resource.take().unwrap_or_default();
            res.frozen_balance_for_energy = Some(frozen);
            account.account_resource = Some(res);
        }
        TRON_POWER => account.tron_power = Some(frozen),
        _ => account.frozen = vec![frozen],
    }
}

// ---------------------------------------------------------------------------
// Freeze
// ---------------------------------------------------------------------------

pub struct FreezeBalanceActuator<'a> {
    contract: &'a FreezeBalanceContract,
}

impl<'a> FreezeBalanceActuator<'a> {
    pub fn new(contract: &'a FreezeBalanceContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !self.contract.receiver_address.is_empty() {
            return Err(ActuatorError::Validate(
                "delegated V1 freeze (receiver) is not supported".into(),
            ));
        }

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("Account[{}] not exists", hex(owner.as_bytes())))
        })?;

        let frozen = self.contract.frozen_balance;
        if frozen <= 0 {
            return Err(ActuatorError::Validate("frozenBalance must be positive".into()));
        }
        if frozen < TRX_PRECISION {
            return Err(ActuatorError::Validate(
                "frozenBalance must be greater than or equal to 1 TRX".into(),
            ));
        }
        if frozen > account.balance {
            return Err(ActuatorError::Validate(
                "frozenBalance must be less than or equal to accountBalance".into(),
            ));
        }

        let (min, max) = self.frozen_time_bounds(state)?;
        let duration = self.contract.frozen_duration;
        if !(min..=max).contains(&duration) {
            return Err(ActuatorError::Validate(format!(
                "frozenDuration must be less than {max} days and more than {min} days"
            )));
        }

        let allow_new = state.get_prop_i64(ALLOW_NEW_RESOURCE_MODEL_KEY)? > 0;
        match self.contract.resource {
            BANDWIDTH | ENERGY => {}
            TRON_POWER if allow_new => {}
            _ => {
                let valid = if allow_new {
                    "ResourceCode error, valid ResourceCode[BANDWIDTH、ENERGY、TRON_POWER]"
                } else {
                    "ResourceCode error, valid ResourceCode[BANDWIDTH、ENERGY]"
                };
                return Err(ActuatorError::Validate(valid.into()));
            }
        }

        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let resource = self.contract.resource;
        let frozen = self.contract.frozen_balance;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let expire_time = now
            .checked_add(self.contract.frozen_duration.saturating_mul(FROZEN_PERIOD_MS))
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        let old = frozen_balance(&account, resource);
        let new = old
            .checked_add(frozen)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        account.balance = account
            .balance
            .checked_sub(frozen)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        set_frozen(&mut account, resource, new, expire_time);
        state.put_account(&owner, &account)?;

        // Bump the global weight total by the delta in whole-TRX weight.
        let increment = new / TRX_PRECISION - old / TRX_PRECISION;
        state.add_prop_i64(weight_key(resource), increment)?;

        Ok(ExecutionResult { fee: 0 })
    }

    fn frozen_time_bounds<S: KvStore>(
        &self,
        state: &WorldState<S>,
    ) -> Result<(i64, i64), ActuatorError> {
        let min = state.get_prop_i64(MIN_FROZEN_TIME_KEY)?;
        let max = state.get_prop_i64(MAX_FROZEN_TIME_KEY)?;
        Ok((
            if min > 0 { min } else { DEFAULT_MIN_FROZEN_TIME },
            if max > 0 { max } else { DEFAULT_MAX_FROZEN_TIME },
        ))
    }
}

// ---------------------------------------------------------------------------
// Unfreeze
// ---------------------------------------------------------------------------

pub struct UnfreezeBalanceActuator<'a> {
    contract: &'a UnfreezeBalanceContract,
}

impl<'a> UnfreezeBalanceActuator<'a> {
    pub fn new(contract: &'a UnfreezeBalanceContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !self.contract.receiver_address.is_empty() {
            return Err(ActuatorError::Validate(
                "delegated V1 unfreeze (receiver) is not supported".into(),
            ));
        }

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("Account[{}] does not exist", hex(owner.as_bytes())))
        })?;

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        match self.contract.resource {
            BANDWIDTH => {
                if account.frozen.is_empty() {
                    return Err(ActuatorError::Validate("no frozenBalance(BANDWIDTH)".into()));
                }
                if !account.frozen.iter().any(|f| f.expire_time <= now) {
                    return Err(ActuatorError::Validate("It's not time to unfreeze(BANDWIDTH).".into()));
                }
            }
            ENERGY => {
                let f = account
                    .account_resource
                    .as_ref()
                    .and_then(|r| r.frozen_balance_for_energy.as_ref());
                match f {
                    Some(f) if f.frozen_balance > 0 => {
                        if f.expire_time > now {
                            return Err(ActuatorError::Validate("It's not time to unfreeze(Energy).".into()));
                        }
                    }
                    _ => return Err(ActuatorError::Validate("no frozenBalance(Energy)".into())),
                }
            }
            TRON_POWER => match account.tron_power.as_ref() {
                Some(f) if f.frozen_balance > 0 => {
                    if f.expire_time > now {
                        return Err(ActuatorError::Validate("It's not time to unfreeze(TronPower).".into()));
                    }
                }
                _ => return Err(ActuatorError::Validate("no frozenBalance(TronPower)".into())),
            },
            _ => {
                return Err(ActuatorError::Validate(
                    "ResourceCode error, valid ResourceCode[BANDWIDTH、ENERGY]".into(),
                ))
            }
        }

        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let resource = self.contract.resource;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        let old = frozen_balance(&account, resource);
        // Sum matured frozen amounts and clear them.
        let unfreeze = match resource {
            BANDWIDTH => {
                let (matured, remaining): (Vec<Frozen>, Vec<Frozen>) =
                    account.frozen.iter().partition(|f| f.expire_time <= now);
                account.frozen = remaining;
                matured.iter().map(|f| f.frozen_balance).sum()
            }
            ENERGY => {
                let mut res = account.account_resource.take().unwrap_or_default();
                let amount = res
                    .frozen_balance_for_energy
                    .take()
                    .map(|f| f.frozen_balance)
                    .unwrap_or(0);
                account.account_resource = Some(res);
                amount
            }
            TRON_POWER => account.tron_power.take().map(|f| f.frozen_balance).unwrap_or(0),
            _ => 0,
        };

        account.balance = account
            .balance
            .checked_add(unfreeze)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        state.put_account(&owner, &account)?;

        // Drop the global weight total by the removed whole-TRX weight.
        let new = frozen_balance(&account, resource);
        let decrease = new / TRX_PRECISION - old / TRX_PRECISION;
        state.add_prop_i64(weight_key(resource), decrease)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const NOW: i64 = 1_700_000_000_000;
    const DAY: i64 = FROZEN_PERIOD_MS;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn state_with(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            owner,
            &protocol::Account { address: owner.as_bytes().to_vec(), balance, ..Default::default() },
        )
        .unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        ws
    }

    fn freeze_c(owner: &Address, amount: i64, resource: ResourceCode) -> FreezeBalanceContract {
        FreezeBalanceContract {
            owner_address: owner.as_bytes().to_vec(),
            frozen_balance: amount,
            frozen_duration: 3,
            resource: resource as i32,
            receiver_address: vec![],
        }
    }

    fn unfreeze_c(owner: &Address, resource: ResourceCode) -> UnfreezeBalanceContract {
        UnfreezeBalanceContract {
            owner_address: owner.as_bytes().to_vec(),
            resource: resource as i32,
            receiver_address: vec![],
        }
    }

    #[test]
    fn freeze_moves_balance_and_bumps_net_weight() {
        let o = addr(1);
        let mut ws = state_with(&o, 10_000_000);
        let c = freeze_c(&o, 5_000_000, ResourceCode::Bandwidth);
        let a = FreezeBalanceActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let account = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(account.balance, 5_000_000);
        assert_eq!(frozen_balance(&account, BANDWIDTH), 5_000_000);
        assert_eq!(account.frozen[0].expire_time, NOW + 3 * DAY);
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 5); // 5 TRX
    }

    #[test]
    fn freeze_energy_bumps_energy_weight() {
        let o = addr(1);
        let mut ws = state_with(&o, 10_000_000);
        let c = freeze_c(&o, 4_000_000, ResourceCode::Energy);
        FreezeBalanceActuator::new(&c).execute(&mut ws).unwrap();
        let account = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(frozen_balance(&account, ENERGY), 4_000_000);
        assert_eq!(ws.get_prop_i64(props::TOTAL_ENERGY_WEIGHT).unwrap(), 4);
    }

    #[test]
    fn unfreeze_after_maturity_returns_balance_and_drops_weight() {
        let o = addr(1);
        let mut ws = state_with(&o, 10_000_000);
        FreezeBalanceActuator::new(&freeze_c(&o, 5_000_000, ResourceCode::Bandwidth))
            .execute(&mut ws)
            .unwrap();
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 5);

        // Advance past the 3-day expiry.
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW + 3 * DAY + 1).unwrap();
        let c = unfreeze_c(&o, ResourceCode::Bandwidth);
        let a = UnfreezeBalanceActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let account = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(account.balance, 10_000_000);
        assert!(account.frozen.is_empty());
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 0);
    }

    #[test]
    fn rejects_premature_unfreeze() {
        let o = addr(1);
        let mut ws = state_with(&o, 10_000_000);
        FreezeBalanceActuator::new(&freeze_c(&o, 5_000_000, ResourceCode::Bandwidth))
            .execute(&mut ws)
            .unwrap();
        // Clock still before expiry.
        assert!(matches!(
            UnfreezeBalanceActuator::new(&unfreeze_c(&o, ResourceCode::Bandwidth)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("It's not time to unfreeze(BANDWIDTH).")
        ));
    }

    #[test]
    fn rejects_below_one_trx() {
        let o = addr(1);
        let ws = state_with(&o, 10_000_000);
        assert!(matches!(
            FreezeBalanceActuator::new(&freeze_c(&o, 500_000, ResourceCode::Bandwidth)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("greater than or equal to 1 TRX")
        ));
    }

    #[test]
    fn rejects_more_than_balance_and_missing_owner() {
        let o = addr(1);
        let ws = state_with(&o, 2_000_000);
        assert!(matches!(
            FreezeBalanceActuator::new(&freeze_c(&o, 5_000_000, ResourceCode::Bandwidth)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("less than or equal to accountBalance")
        ));
        let empty = WorldState::new(MemoryStore::new());
        assert!(matches!(
            FreezeBalanceActuator::new(&freeze_c(&addr(2), 5_000_000, ResourceCode::Bandwidth)).validate(&empty),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn rejects_delegated_receiver() {
        let o = addr(1);
        let ws = state_with(&o, 10_000_000);
        let mut c = freeze_c(&o, 5_000_000, ResourceCode::Bandwidth);
        c.receiver_address = addr(2).as_bytes().to_vec();
        assert!(matches!(
            FreezeBalanceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("delegated V1 freeze")
        ));
        let mut u = unfreeze_c(&o, ResourceCode::Bandwidth);
        u.receiver_address = addr(2).as_bytes().to_vec();
        assert!(matches!(
            UnfreezeBalanceActuator::new(&u).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("delegated V1 unfreeze")
        ));
    }

    #[test]
    fn rejects_invalid_duration() {
        let o = addr(1);
        let ws = state_with(&o, 10_000_000);
        let mut c = freeze_c(&o, 5_000_000, ResourceCode::Bandwidth);
        c.frozen_duration = 5; // outside default [3,3]
        assert!(matches!(
            FreezeBalanceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("frozenDuration must be")
        ));
    }

    #[test]
    fn value_and_weight_conserved_across_freeze_unfreeze() {
        let o = addr(1);
        let initial = 10_000_000;
        let mut ws = state_with(&o, initial);

        // Freeze: balance + frozen invariant; weight bumped.
        FreezeBalanceActuator::new(&freeze_c(&o, 6_000_000, ResourceCode::Bandwidth))
            .execute(&mut ws)
            .unwrap();
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.balance + frozen_balance(&acct, BANDWIDTH), initial);
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 6);

        // Mature + unfreeze: balance restored, weight back to 0.
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW + 3 * DAY).unwrap();
        UnfreezeBalanceActuator::new(&unfreeze_c(&o, ResourceCode::Bandwidth))
            .execute(&mut ws)
            .unwrap();
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.balance, initial);
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 0);
    }
}
