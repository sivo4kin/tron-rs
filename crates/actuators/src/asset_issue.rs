//! `AssetIssueContract` — create a new TRC10 token.
//!
//! Mirrors java-tron `AssetIssueActuator` (V2 path). Validate checks the token
//! parameters (name/abbr/url/description bounds, positive supply/exchange rate,
//! a valid future sale window, frozen-supply entries) and that the owner exists,
//! has not already issued a token, and can pay the asset-issue fee. Execute
//! allocates a token id (the I02 allocator), stores the `AssetIssueContract`,
//! credits the owner's `asset_v2[id]` with the non-frozen supply, records frozen
//! supply as `Account.frozen_supply`, sets `Account.asset_issued_id`/`_name`, and
//! burns the fee.
//!
//! Deviations from java-tron:
//! - V2 asset store only (see I02); the legacy name-keyed store / V1 `asset` map
//!   are not modelled.
//! - The fee is always burned via [`WorldState::burn_trx`] (accumulates into
//!   `BURN_TRX_AMOUNT`); the non-blackhole "credit blackhole account" path and
//!   fork-gated frozen-expire overflow guard are not modelled.
//! - Dynamic tunables (asset-issue fee, one-day net limit, frozen-supply bounds)
//!   read a dynamic property, falling back to the java genesis defaults below.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::account::Frozen;
use tron_proto::protocol::AssetIssueContract;
use tron_state::{props, set_asset_v2_balance, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Dynamic-property key: TRX fee to issue a TRC10 token.
pub const ASSET_ISSUE_FEE: &str = "ASSET_ISSUE_FEE";
/// java-tron genesis default asset-issue fee: 1024 TRX.
pub const DEFAULT_ASSET_ISSUE_FEE: i64 = 1_024_000_000;

const ONE_DAY_NET_LIMIT_KEY: &str = "ONE_DAY_NET_LIMIT";
const DEFAULT_ONE_DAY_NET_LIMIT: i64 = 57_600_000_000;

const MAX_ASSET_NAME_LEN: usize = 32;
const MAX_URL_LEN: usize = 256;
const MAX_ASSET_DESCRIPTION_LEN: usize = 200;
const PRECISION_DECIMAL: i32 = 6;
/// java-tron `DynamicPropertiesStore` defaults for frozen supply.
const MAX_FROZEN_SUPPLY_NUMBER: usize = 10;
const MIN_FROZEN_SUPPLY_TIME: i64 = 1;
const MAX_FROZEN_SUPPLY_TIME: i64 = 3652;
/// One day in ms (java-tron `ChainConstant.FROZEN_PERIOD`).
const FROZEN_PERIOD_MS: i64 = 86_400_000;

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid ownerAddress".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid ownerAddress".into()))
}

/// java-tron `TransactionUtil.validReadableBytes`: non-empty, `<= max`, every
/// byte a visible ASCII char (`0x21..=0x7e`).
fn valid_readable(bytes: &[u8], max: usize) -> bool {
    !bytes.is_empty() && bytes.len() <= max && bytes.iter().all(|b| (0x21..=0x7e).contains(b))
}

fn valid_url(url: &[u8]) -> bool {
    !url.is_empty() && url.len() <= MAX_URL_LEN
}

fn valid_description(desc: &[u8]) -> bool {
    desc.len() <= MAX_ASSET_DESCRIPTION_LEN
}

pub struct AssetIssueActuator<'a> {
    contract: &'a AssetIssueContract,
}

impl<'a> AssetIssueActuator<'a> {
    pub fn new(contract: &'a AssetIssueContract) -> Self {
        Self { contract }
    }

    fn fee<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let f = state.get_prop_i64(ASSET_ISSUE_FEE)?;
        Ok(if f > 0 { f } else { DEFAULT_ASSET_ISSUE_FEE })
    }

    fn one_day_net_limit<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let v = state.get_prop_i64(ONE_DAY_NET_LIMIT_KEY)?;
        Ok(if v > 0 { v } else { DEFAULT_ONE_DAY_NET_LIMIT })
    }

    /// java-tron `AssetIssueActuator.validate`. Returns the asset-issue fee.
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let c = self.contract;
        let owner = parse_address(&c.owner_address)?;

        if !valid_readable(&c.name, MAX_ASSET_NAME_LEN) {
            return Err(ActuatorError::Validate("Invalid assetName".into()));
        }
        if c.name.eq_ignore_ascii_case(b"trx") {
            return Err(ActuatorError::Validate("assetName can't be trx".into()));
        }
        if c.precision != 0 && (c.precision < 0 || c.precision > PRECISION_DECIMAL) {
            return Err(ActuatorError::Validate("precision cannot exceed 6".into()));
        }
        if !c.abbr.is_empty() && !valid_readable(&c.abbr, MAX_ASSET_NAME_LEN) {
            return Err(ActuatorError::Validate("Invalid abbreviation for token".into()));
        }
        if !valid_url(&c.url) {
            return Err(ActuatorError::Validate("Invalid url".into()));
        }
        if !valid_description(&c.description) {
            return Err(ActuatorError::Validate("Invalid description".into()));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        if c.start_time == 0 {
            return Err(ActuatorError::Validate("Start time should be not empty".into()));
        }
        if c.end_time == 0 {
            return Err(ActuatorError::Validate("End time should be not empty".into()));
        }
        if c.end_time <= c.start_time {
            return Err(ActuatorError::Validate("End time should be greater than start time".into()));
        }
        if c.start_time <= now {
            return Err(ActuatorError::Validate("Start time should be greater than HeadBlockTime".into()));
        }

        if c.total_supply <= 0 {
            return Err(ActuatorError::Validate("TotalSupply must greater than 0!".into()));
        }
        if c.trx_num <= 0 {
            return Err(ActuatorError::Validate("TrxNum must greater than 0!".into()));
        }
        if c.num <= 0 {
            return Err(ActuatorError::Validate("Num must greater than 0!".into()));
        }
        if c.public_free_asset_net_usage != 0 {
            return Err(ActuatorError::Validate("PublicFreeAssetNetUsage must be 0!".into()));
        }
        if c.frozen_supply.len() > MAX_FROZEN_SUPPLY_NUMBER {
            return Err(ActuatorError::Validate("Frozen supply list length is too long".into()));
        }

        let day_limit = Self::one_day_net_limit(state)?;
        if c.free_asset_net_limit < 0 || c.free_asset_net_limit >= day_limit {
            return Err(ActuatorError::Validate("Invalid FreeAssetNetLimit".into()));
        }
        if c.public_free_asset_net_limit < 0 || c.public_free_asset_net_limit >= day_limit {
            return Err(ActuatorError::Validate("Invalid PublicFreeAssetNetLimit".into()));
        }

        // Frozen-supply entries must be positive, fit under the remaining supply,
        // and have a duration within the allowed bounds.
        let mut remain = c.total_supply;
        for f in &c.frozen_supply {
            if f.frozen_amount <= 0 {
                return Err(ActuatorError::Validate("Frozen supply must be greater than 0!".into()));
            }
            if f.frozen_amount > remain {
                return Err(ActuatorError::Validate("Frozen supply cannot exceed total supply".into()));
            }
            if !(MIN_FROZEN_SUPPLY_TIME..=MAX_FROZEN_SUPPLY_TIME).contains(&f.frozen_days) {
                return Err(ActuatorError::Validate(format!(
                    "frozenDuration must be less than {MAX_FROZEN_SUPPLY_TIME} days and more than {MIN_FROZEN_SUPPLY_TIME} days"
                )));
            }
            remain -= f.frozen_amount;
        }

        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account not exists".into()))?;
        if !account.asset_issued_id.is_empty() {
            return Err(ActuatorError::Validate("An account can only issue one asset".into()));
        }
        let fee = Self::fee(state)?;
        if account.balance < fee {
            return Err(ActuatorError::Validate("No enough balance for fee!".into()));
        }

        Ok(fee)
    }

    /// java-tron `AssetIssueActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let c = self.contract;
        let owner = parse_address(&c.owner_address)?;
        let fee = Self::fee(state)?;

        // Allocate the token id (I02 allocator: tokenIdNum++ then persist).
        let id = state.allocate_token_id()?;

        // Store the token definition under its new id.
        let mut asset = c.clone();
        asset.id = id.to_string();
        state.put_asset_issue(id, &asset)?;

        // Debit + burn the fee.
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.balance = account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        state.burn_trx(fee)?;

        // Split supply into frozen (recorded as Account.frozen_supply, maturing at
        // start_time + days) and the non-frozen remainder credited to the owner.
        let mut remain = c.total_supply;
        for f in &c.frozen_supply {
            let expire_time = c
                .start_time
                .checked_add(f.frozen_days.checked_mul(FROZEN_PERIOD_MS).unwrap_or(i64::MAX))
                .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
            account.frozen_supply.push(Frozen {
                frozen_balance: f.frozen_amount,
                expire_time,
            });
            remain -= f.frozen_amount;
        }

        set_asset_v2_balance(&mut account, id, remain);
        account.asset_issued_id = id.to_string().into_bytes();
        account.asset_issued_name = c.name.clone();
        state.put_account(&owner, &account)?;

        Ok(ExecutionResult { fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_proto::protocol::asset_issue_contract::FrozenSupply;
    use tron_state::asset_v2_balance;
    use tron_storage::MemoryStore;

    const NOW: i64 = 1_700_000_000_000;
    const FEE: i64 = 1_000_000;
    const FIRST_ID: i64 = 1_000_001;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn state_with_owner(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            owner,
            &protocol::Account { address: owner.as_bytes().to_vec(), balance, ..Default::default() },
        )
        .unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        ws.put_prop_i64(ASSET_ISSUE_FEE, FEE).unwrap();
        ws
    }

    fn contract(owner: &Address, total_supply: i64) -> AssetIssueContract {
        AssetIssueContract {
            owner_address: owner.as_bytes().to_vec(),
            name: b"MyToken".to_vec(),
            abbr: b"MTK".to_vec(),
            total_supply,
            trx_num: 1,
            num: 100,
            precision: 6,
            start_time: NOW + 1_000,
            end_time: NOW + 2_000,
            url: b"http://token.example".to_vec(),
            description: b"a token".to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn happy_path_creates_token_credits_supply_deducts_fee() {
        let o = addr(1);
        let mut ws = state_with_owner(&o, 10_000_000);
        let c = contract(&o, 1_000_000);
        let a = AssetIssueActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), FEE);
        assert_eq!(a.execute(&mut ws).unwrap().fee, FEE);

        // Token id allocated + stored.
        assert_eq!(ws.get_token_id_num().unwrap(), FIRST_ID);
        let stored = ws.get_asset_issue(FIRST_ID).unwrap().unwrap();
        assert_eq!(stored.id, "1000001");
        assert_eq!(stored.total_supply, 1_000_000);

        // Owner: full supply credited, fee burned, issued-id set.
        let account = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(asset_v2_balance(&account, FIRST_ID), 1_000_000);
        assert_eq!(account.balance, 10_000_000 - FEE);
        assert_eq!(account.asset_issued_id, b"1000001".to_vec());
        assert_eq!(account.asset_issued_name, b"MyToken".to_vec());
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), FEE);
    }

    #[test]
    fn frozen_supply_reduces_credited_amount_and_records_frozen() {
        let o = addr(1);
        let mut ws = state_with_owner(&o, 10_000_000);
        let mut c = contract(&o, 1_000_000);
        c.frozen_supply = vec![FrozenSupply { frozen_amount: 300_000, frozen_days: 10 }];
        let a = AssetIssueActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();

        let account = ws.get_account(&o).unwrap().unwrap();
        // 1_000_000 - 300_000 frozen = 700_000 spendable.
        assert_eq!(asset_v2_balance(&account, FIRST_ID), 700_000);
        assert_eq!(account.frozen_supply.len(), 1);
        assert_eq!(account.frozen_supply[0].frozen_balance, 300_000);
        assert_eq!(account.frozen_supply[0].expire_time, (NOW + 1_000) + 10 * FROZEN_PERIOD_MS);
    }

    #[test]
    fn rejects_zero_supply() {
        let o = addr(1);
        let ws = state_with_owner(&o, 10_000_000);
        assert!(matches!(
            AssetIssueActuator::new(&contract(&o, 0)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("TotalSupply must greater than 0!")
        ));
    }

    #[test]
    fn rejects_bad_time_window() {
        let o = addr(1);
        let ws = state_with_owner(&o, 10_000_000);
        // end <= start
        let mut c = contract(&o, 1_000_000);
        c.end_time = c.start_time;
        assert!(matches!(
            AssetIssueActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("End time should be greater than start time")
        ));
        // start in the past
        let mut c = contract(&o, 1_000_000);
        c.start_time = NOW - 1;
        c.end_time = NOW + 10_000;
        assert!(matches!(
            AssetIssueActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Start time should be greater than HeadBlockTime")
        ));
    }

    #[test]
    fn rejects_insufficient_balance() {
        let o = addr(1);
        let ws = state_with_owner(&o, FEE - 1);
        assert!(matches!(
            AssetIssueActuator::new(&contract(&o, 1_000_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("No enough balance for fee!")
        ));
    }

    #[test]
    fn rejects_second_issue_by_same_owner() {
        let o = addr(1);
        let mut ws = state_with_owner(&o, 10_000_000);
        let c = contract(&o, 1_000_000);
        AssetIssueActuator::new(&c).execute(&mut ws).unwrap(); // first issue sets asset_issued_id
        // A second issue must be rejected in validate.
        let c2 = contract(&o, 500_000);
        assert!(matches!(
            AssetIssueActuator::new(&c2).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("An account can only issue one asset")
        ));
    }

    #[test]
    fn rejects_invalid_name_and_trx_name() {
        let o = addr(1);
        let ws = state_with_owner(&o, 10_000_000);
        let mut c = contract(&o, 1_000_000);
        c.name = b"".to_vec();
        assert!(matches!(
            AssetIssueActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid assetName")
        ));
        let mut c = contract(&o, 1_000_000);
        c.name = b"TRX".to_vec();
        assert!(matches!(
            AssetIssueActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("assetName can't be trx")
        ));
    }

    #[test]
    fn rejects_non_positive_exchange_rate() {
        let o = addr(1);
        let ws = state_with_owner(&o, 10_000_000);
        let mut c = contract(&o, 1_000_000);
        c.num = 0;
        assert!(matches!(
            AssetIssueActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Num must greater than 0!")
        ));
    }
}
