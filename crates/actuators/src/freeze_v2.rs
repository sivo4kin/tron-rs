//! Stake 2.0 freeze/unfreeze — `FreezeBalanceV2Contract` and
//! `UnfreezeBalanceV2Contract`.
//!
//! Semantics mirror java-tron's `FreezeBalanceV2Actuator` and
//! `UnfreezeBalanceV2Actuator`.
//!
//! **Freeze — validate** — owner address is a valid 21-byte `0x41…`; owner
//! account exists; `frozen_balance > 0`; `frozen_balance >= 1 TRX`;
//! `frozen_balance <= owner.balance`; resource is `BANDWIDTH`/`ENERGY`, or
//! `TRON_POWER` only when the new-resource-model property is enabled.
//!
//! **Freeze — execute** — move `frozen_balance` out of `Account.balance` into
//! the `Account.frozen_v2` entry for that resource type, accumulating with any
//! existing entry of the same type.
//!
//! **Unfreeze — validate** — owner exists; there is a positive `frozen_v2`
//! amount of the requested type; `0 < unfreeze_balance <= that frozen amount`;
//! the number of pending (not-yet-matured) `unfrozen_v2` entries is below
//! [`UNFREEZE_MAX_TIMES`].
//!
//! **Unfreeze — execute** — decrease the `frozen_v2` amount of that type by
//! `unfreeze_balance` and append an `unfrozen_v2` entry with
//! `unfreeze_expire_time = now + delay`, where `now` is
//! [`props::LATEST_BLOCK_HEADER_TIMESTAMP`] and `delay` is the
//! `UNFREEZE_DELAY_DAYS` property (days) × [`FROZEN_PERIOD_MS`], falling back to
//! [`DEFAULT_UNFREEZE_DELAY_DAYS`] when the property is unset.
//!
//! Deviations from java-tron (differences are data-only, documented here):
//! - The `supportUnfreezeDelay` committee gate (both actuators) is not modelled;
//!   FreezeV2 is treated as always enabled.
//! - Unfreeze execution does not run java-tron's `unfreezeExpire` step (auto
//!   withdrawing matured `unfrozen_v2` entries back to balance), nor
//!   `withdrawReward`/vote/total-weight bookkeeping. It performs only the
//!   frozen→unfrozen move described above.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::account::{FreezeV2, UnFreezeV2};
use tron_proto::protocol::{FreezeBalanceV2Contract, ResourceCode, UnfreezeBalanceV2Contract};
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// java-tron `Parameter.ChainConstant.TRX_PRECISION` — sun per TRX.
pub const TRX_PRECISION: i64 = 1_000_000;

/// java-tron `Parameter.ChainConstant.FROZEN_PERIOD` — one day, in ms.
pub const FROZEN_PERIOD_MS: i64 = 86_400_000;

/// Fallback unfreeze delay (days) when the `UNFREEZE_DELAY_DAYS` dynamic
/// property is unset. Matches java-tron's governance-set mainnet value.
pub const DEFAULT_UNFREEZE_DELAY_DAYS: i64 = 14;

/// java-tron `UnfreezeBalanceV2Actuator.UNFREEZE_MAX_TIMES`.
pub const UNFREEZE_MAX_TIMES: usize = 32;

/// Dynamic-property key: committee-set unfreeze delay, in days.
const UNFREEZE_DELAY_DAYS_KEY: &str = "UNFREEZE_DELAY_DAYS";

/// Dynamic-property key: when > 0, `TRON_POWER` is a valid freeze resource.
const ALLOW_NEW_RESOURCE_MODEL_KEY: &str = "ALLOW_NEW_RESOURCE_MODEL";

const BANDWIDTH: i32 = ResourceCode::Bandwidth as i32;
const ENERGY: i32 = ResourceCode::Energy as i32;
const TRON_POWER: i32 = ResourceCode::TronPower as i32;

/// Global staked-weight total (dynamic property) for a resource type, or `None`
/// for an unknown code. Mirrors java-tron's per-resource `addTotal*Weight` calls.
fn total_weight_key(resource: i32) -> Option<&'static str> {
    match resource {
        BANDWIDTH => Some(props::TOTAL_NET_WEIGHT),
        ENERGY => Some(props::TOTAL_ENERGY_WEIGHT),
        TRON_POWER => Some(props::TOTAL_TRON_POWER_WEIGHT),
        _ => None,
    }
}

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

// ---------------------------------------------------------------------------
// Freeze
// ---------------------------------------------------------------------------

pub struct FreezeBalanceV2Actuator<'a> {
    contract: &'a FreezeBalanceV2Contract,
}

impl<'a> FreezeBalanceV2Actuator<'a> {
    pub fn new(contract: &'a FreezeBalanceV2Contract) -> Self {
        Self { contract }
    }

    /// java-tron `FreezeBalanceV2Actuator.validate`. Returns the fee (always 0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let owner_account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("Account[{}] not exists", hex(owner.as_bytes())))
        })?;

        let frozen_balance = self.contract.frozen_balance;
        if frozen_balance <= 0 {
            return Err(ActuatorError::Validate("frozenBalance must be positive".into()));
        }
        if frozen_balance < TRX_PRECISION {
            return Err(ActuatorError::Validate(
                "frozenBalance must be greater than or equal to 1 TRX".into(),
            ));
        }
        if frozen_balance > owner_account.balance {
            return Err(ActuatorError::Validate(
                "frozenBalance must be less than or equal to accountBalance".into(),
            ));
        }

        let allow_new_resource = state.get_prop_i64(ALLOW_NEW_RESOURCE_MODEL_KEY)? > 0;
        match self.contract.resource {
            BANDWIDTH | ENERGY => {}
            TRON_POWER if allow_new_resource => {}
            _ => {
                let valid = if allow_new_resource {
                    "ResourceCode error, valid ResourceCode[BANDWIDTH、ENERGY、TRON_POWER]"
                } else {
                    "ResourceCode error, valid ResourceCode[BANDWIDTH、ENERGY]"
                };
                return Err(ActuatorError::Validate(valid.into()));
            }
        }

        Ok(0)
    }

    /// java-tron `FreezeBalanceV2Actuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let frozen_balance = self.contract.frozen_balance;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        let new_balance = account
            .balance
            .checked_sub(frozen_balance)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;

        // Accumulate into the frozen_v2 entry for this resource type.
        let resource = self.contract.resource;
        if let Some(entry) = account.frozen_v2.iter_mut().find(|f| f.r#type == resource) {
            entry.amount = entry
                .amount
                .checked_add(frozen_balance)
                .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        } else {
            account.frozen_v2.push(FreezeV2 {
                r#type: resource,
                amount: frozen_balance,
            });
        }

        account.balance = new_balance;
        state.put_account(&owner, &account)?;

        // Add to the network staked-weight total (java addTotal*Weight); weight is
        // TRX, i.e. sun / TRX_PRECISION.
        if let Some(key) = total_weight_key(resource) {
            state.add_prop_i64(key, frozen_balance / TRX_PRECISION)?;
        }

        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// Unfreeze
// ---------------------------------------------------------------------------

pub struct UnfreezeBalanceV2Actuator<'a> {
    contract: &'a UnfreezeBalanceV2Contract,
}

impl<'a> UnfreezeBalanceV2Actuator<'a> {
    pub fn new(contract: &'a UnfreezeBalanceV2Contract) -> Self {
        Self { contract }
    }

    /// java-tron `UnfreezeBalanceV2Actuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                hex(owner.as_bytes())
            ))
        })?;

        let allow_new_resource = state.get_prop_i64(ALLOW_NEW_RESOURCE_MODEL_KEY)? > 0;
        let resource = self.contract.resource;
        match resource {
            BANDWIDTH => {
                if !Self::has_frozen(&account, BANDWIDTH) {
                    return Err(ActuatorError::Validate("no frozenBalance(BANDWIDTH)".into()));
                }
            }
            ENERGY => {
                if !Self::has_frozen(&account, ENERGY) {
                    return Err(ActuatorError::Validate("no frozenBalance(Energy)".into()));
                }
            }
            TRON_POWER if allow_new_resource => {
                if !Self::has_frozen(&account, TRON_POWER) {
                    return Err(ActuatorError::Validate("no frozenBalance(TronPower)".into()));
                }
            }
            _ => {
                let valid = if allow_new_resource {
                    "ResourceCode error.valid ResourceCode[BANDWIDTH、Energy、TRON_POWER]"
                } else {
                    "ResourceCode error.valid ResourceCode[BANDWIDTH、Energy]"
                };
                return Err(ActuatorError::Validate(valid.into()));
            }
        }

        let frozen_amount = Self::frozen_amount(&account, resource);
        let unfreeze_balance = self.contract.unfreeze_balance;
        if !(unfreeze_balance > 0 && unfreeze_balance <= frozen_amount) {
            return Err(ActuatorError::Validate(format!(
                "Invalid unfreeze_balance, [{unfreeze_balance}] is error"
            )));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let unfreezing_count = account
            .unfrozen_v2
            .iter()
            .filter(|u| u.unfreeze_expire_time > now)
            .count();
        if unfreezing_count >= UNFREEZE_MAX_TIMES {
            return Err(ActuatorError::Validate(
                "Invalid unfreeze operation, unfreezing times is over limit".into(),
            ));
        }

        Ok(0)
    }

    /// java-tron `UnfreezeBalanceV2Actuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let resource = self.contract.resource;
        let unfreeze_balance = self.contract.unfreeze_balance;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        // Decrease the frozen_v2 entry of this type (java updateAccountFrozenInfo).
        let entry = account
            .frozen_v2
            .iter_mut()
            .find(|f| f.r#type == resource)
            .ok_or_else(|| ActuatorError::Execute("no frozenBalance".into()))?;
        entry.amount = entry
            .amount
            .checked_sub(unfreeze_balance)
            .filter(|a| *a >= 0)
            .ok_or_else(|| ActuatorError::Execute("frozenBalance is not sufficient".into()))?;

        // Append an unfrozen_v2 entry maturing after the configured delay.
        let expire_time = now
            .checked_add(Self::unfreeze_delay_ms(state)?)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        account.unfrozen_v2.push(UnFreezeV2 {
            r#type: resource,
            unfreeze_amount: unfreeze_balance,
            unfreeze_expire_time: expire_time,
        });

        state.put_account(&owner, &account)?;

        // Remove from the network staked-weight total (java subtracts the same
        // weight it added on freeze); weight is TRX = sun / TRX_PRECISION.
        if let Some(key) = total_weight_key(resource) {
            state.add_prop_i64(key, -(unfreeze_balance / TRX_PRECISION))?;
        }

        Ok(ExecutionResult { fee: 0 })
    }

    fn unfreeze_delay_ms<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let days = state.get_prop_i64(UNFREEZE_DELAY_DAYS_KEY)?;
        let days = if days > 0 { days } else { DEFAULT_UNFREEZE_DELAY_DAYS };
        days.checked_mul(FROZEN_PERIOD_MS)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))
    }

    fn has_frozen(account: &tron_proto::protocol::Account, resource: i32) -> bool {
        account
            .frozen_v2
            .iter()
            .any(|f| f.r#type == resource && f.amount > 0)
    }

    fn frozen_amount(account: &tron_proto::protocol::Account, resource: i32) -> i64 {
        account
            .frozen_v2
            .iter()
            .find(|f| f.r#type == resource)
            .map(|f| f.amount)
            .unwrap_or(0)
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

    fn seeded_state(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance,
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        ws
    }

    fn freeze_contract(owner: &Address, amount: i64, resource: ResourceCode) -> FreezeBalanceV2Contract {
        FreezeBalanceV2Contract {
            owner_address: owner.as_bytes().to_vec(),
            frozen_balance: amount,
            resource: resource as i32,
        }
    }

    fn unfreeze_contract(
        owner: &Address,
        amount: i64,
        resource: ResourceCode,
    ) -> UnfreezeBalanceV2Contract {
        UnfreezeBalanceV2Contract {
            owner_address: owner.as_bytes().to_vec(),
            unfreeze_balance: amount,
            resource: resource as i32,
        }
    }

    fn frozen_of(ws: &WorldState<MemoryStore>, owner: &Address, resource: ResourceCode) -> i64 {
        ws.get_account(owner)
            .unwrap()
            .unwrap()
            .frozen_v2
            .iter()
            .find(|f| f.r#type == resource as i32)
            .map(|f| f.amount)
            .unwrap_or(0)
    }

    // -- freeze ----------------------------------------------------------

    #[test]
    fn freeze_happy_path_moves_balance_to_bandwidth() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 10_000_000);
        let c = freeze_contract(&o, 3_000_000, ResourceCode::Bandwidth);
        let a = FreezeBalanceV2Actuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        assert_eq!(a.execute(&mut ws).unwrap().fee, 0);

        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 7_000_000);
        assert_eq!(frozen_of(&ws, &o, ResourceCode::Bandwidth), 3_000_000);
    }

    #[test]
    fn freeze_accumulates_on_second_freeze() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 10_000_000);

        let c1 = freeze_contract(&o, 3_000_000, ResourceCode::Energy);
        FreezeBalanceV2Actuator::new(&c1).execute(&mut ws).unwrap();
        let c2 = freeze_contract(&o, 2_000_000, ResourceCode::Energy);
        let a2 = FreezeBalanceV2Actuator::new(&c2);
        a2.validate(&ws).unwrap();
        a2.execute(&mut ws).unwrap();

        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 5_000_000);
        // A single accumulated entry, not two.
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.frozen_v2.len(), 1);
        assert_eq!(frozen_of(&ws, &o, ResourceCode::Energy), 5_000_000);
    }

    #[test]
    fn freeze_more_than_balance_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 2_000_000);
        let c = freeze_contract(&o, 3_000_000, ResourceCode::Bandwidth);
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("less than or equal to accountBalance")
        ));
    }

    #[test]
    fn freeze_non_positive_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        for amount in [0, -1, i64::MIN] {
            let c = freeze_contract(&o, amount, ResourceCode::Bandwidth);
            assert!(
                matches!(
                    FreezeBalanceV2Actuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("must be positive")
                ),
                "amount {amount} must be rejected"
            );
        }
    }

    #[test]
    fn freeze_below_one_trx_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        let c = freeze_contract(&o, 500_000, ResourceCode::Bandwidth);
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("greater than or equal to 1 TRX")
        ));
    }

    #[test]
    fn freeze_invalid_resource_code_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        // Out-of-range resource code.
        let c = FreezeBalanceV2Contract {
            owner_address: o.as_bytes().to_vec(),
            frozen_balance: 3_000_000,
            resource: 7,
        };
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("ResourceCode error")
        ));
        // TRON_POWER without the new-resource-model property enabled.
        let c = freeze_contract(&o, 3_000_000, ResourceCode::TronPower);
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("ResourceCode error")
        ));
    }

    #[test]
    fn freeze_tron_power_allowed_when_new_resource_model() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 10_000_000);
        ws.put_prop_i64(ALLOW_NEW_RESOURCE_MODEL_KEY, 1).unwrap();
        let c = freeze_contract(&o, 4_000_000, ResourceCode::TronPower);
        let a = FreezeBalanceV2Actuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(frozen_of(&ws, &o, ResourceCode::TronPower), 4_000_000);
    }

    #[test]
    fn freeze_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = freeze_contract(&addr(1), 3_000_000, ResourceCode::Bandwidth);
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn freeze_malformed_address_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        // Wrong length (20 bytes, not 21).
        let c = FreezeBalanceV2Contract {
            owner_address: vec![0x41; 20],
            frozen_balance: 3_000_000,
            resource: ResourceCode::Bandwidth as i32,
        };
        assert!(matches!(
            FreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    // -- unfreeze --------------------------------------------------------

    /// Freeze `amount` of `resource` for `owner` on `ws` (execute only).
    fn do_freeze(
        ws: &mut WorldState<MemoryStore>,
        owner: &Address,
        amount: i64,
        resource: ResourceCode,
    ) {
        let c = freeze_contract(owner, amount, resource);
        FreezeBalanceV2Actuator::new(&c).execute(ws).unwrap();
    }

    #[test]
    fn unfreeze_happy_path_decreases_frozen_and_appends_unfrozen() {
        let o = addr(1);
        let now = 1_700_000_000_000;
        let mut ws = seeded_state(&o, 10_000_000);
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();
        do_freeze(&mut ws, &o, 6_000_000, ResourceCode::Bandwidth);

        let c = unfreeze_contract(&o, 4_000_000, ResourceCode::Bandwidth);
        let a = UnfreezeBalanceV2Actuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        // Frozen decreased.
        assert_eq!(frozen_of(&ws, &o, ResourceCode::Bandwidth), 2_000_000);
        // One unfrozen entry with the correct amount, type, and expire time.
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.unfrozen_v2.len(), 1);
        let u = &acct.unfrozen_v2[0];
        assert_eq!(u.r#type, ResourceCode::Bandwidth as i32);
        assert_eq!(u.unfreeze_amount, 4_000_000);
        let expected_expire = now + DEFAULT_UNFREEZE_DELAY_DAYS * FROZEN_PERIOD_MS;
        assert_eq!(u.unfreeze_expire_time, expected_expire);
        // Balance is unchanged by unfreeze.
        assert_eq!(acct.balance, 4_000_000);
    }

    #[test]
    fn unfreeze_respects_committee_delay_property() {
        let o = addr(1);
        let now = 1_700_000_000_000;
        let mut ws = seeded_state(&o, 10_000_000);
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();
        ws.put_prop_i64(UNFREEZE_DELAY_DAYS_KEY, 3).unwrap();
        do_freeze(&mut ws, &o, 6_000_000, ResourceCode::Energy);

        let c = unfreeze_contract(&o, 1_000_000, ResourceCode::Energy);
        UnfreezeBalanceV2Actuator::new(&c).execute(&mut ws).unwrap();

        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.unfrozen_v2[0].unfreeze_expire_time, now + 3 * FROZEN_PERIOD_MS);
    }

    #[test]
    fn unfreeze_more_than_frozen_rejected() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 10_000_000);
        do_freeze(&mut ws, &o, 3_000_000, ResourceCode::Bandwidth);
        let c = unfreeze_contract(&o, 4_000_000, ResourceCode::Bandwidth);
        assert!(matches!(
            UnfreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is error")
        ));
    }

    #[test]
    fn unfreeze_from_empty_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        // No frozen_v2 of BANDWIDTH exists at all.
        let c = unfreeze_contract(&o, 1_000_000, ResourceCode::Bandwidth);
        assert!(matches!(
            UnfreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no frozenBalance(BANDWIDTH)")
        ));
    }

    #[test]
    fn unfreeze_non_positive_rejected() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 10_000_000);
        do_freeze(&mut ws, &o, 3_000_000, ResourceCode::Bandwidth);
        for amount in [0, -1] {
            let c = unfreeze_contract(&o, amount, ResourceCode::Bandwidth);
            assert!(
                matches!(
                    UnfreezeBalanceV2Actuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("is error")
                ),
                "unfreeze amount {amount} must be rejected"
            );
        }
    }

    #[test]
    fn unfreeze_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = unfreeze_contract(&addr(1), 1_000_000, ResourceCode::Bandwidth);
        assert!(matches!(
            UnfreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn unfreeze_malformed_address_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 10_000_000);
        let c = UnfreezeBalanceV2Contract {
            owner_address: vec![0x41; 20],
            unfreeze_balance: 1_000_000,
            resource: ResourceCode::Bandwidth as i32,
        };
        assert!(matches!(
            UnfreezeBalanceV2Actuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    // -- invariant -------------------------------------------------------

    #[test]
    fn freeze_and_unfreeze_move_global_resource_weight() {
        let o = addr(1);
        let now = 1_700_000_000_000;
        let mut ws = seeded_state(&o, 100 * TRX_PRECISION);
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();

        // Freeze 30 TRX bandwidth + 20 TRX energy -> totals track the weights.
        do_freeze(&mut ws, &o, 30 * TRX_PRECISION, ResourceCode::Bandwidth);
        do_freeze(&mut ws, &o, 20 * TRX_PRECISION, ResourceCode::Energy);
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 30);
        assert_eq!(ws.get_prop_i64(props::TOTAL_ENERGY_WEIGHT).unwrap(), 20);

        // Unfreeze 12 TRX bandwidth -> the bandwidth total drops by 12, energy is untouched.
        let c = unfreeze_contract(&o, 12 * TRX_PRECISION, ResourceCode::Bandwidth);
        let a = UnfreezeBalanceV2Actuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_prop_i64(props::TOTAL_NET_WEIGHT).unwrap(), 18);
        assert_eq!(ws.get_prop_i64(props::TOTAL_ENERGY_WEIGHT).unwrap(), 20);
    }

    #[test]
    fn trx_conservation_across_freeze_and_unfreeze() {
        // Total TRX = balance + sum(frozen_v2) + sum(unfrozen_v2) is invariant.
        let o = addr(1);
        let initial = 10_000_000;
        let now = 1_700_000_000_000;
        let mut ws = seeded_state(&o, initial);
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, now).unwrap();

        let total = |ws: &WorldState<MemoryStore>| {
            let a = ws.get_account(&o).unwrap().unwrap();
            let frozen: i64 = a.frozen_v2.iter().map(|f| f.amount).sum();
            let unfrozen: i64 = a.unfrozen_v2.iter().map(|u| u.unfreeze_amount).sum();
            a.balance + frozen + unfrozen
        };

        assert_eq!(total(&ws), initial);

        // Freeze twice (different resources), then unfreeze part of one.
        do_freeze(&mut ws, &o, 4_000_000, ResourceCode::Bandwidth);
        assert_eq!(total(&ws), initial);
        do_freeze(&mut ws, &o, 3_000_000, ResourceCode::Energy);
        assert_eq!(total(&ws), initial);

        let c = unfreeze_contract(&o, 2_500_000, ResourceCode::Bandwidth);
        let a = UnfreezeBalanceV2Actuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        assert_eq!(total(&ws), initial);
    }
}
