//! `UpdateAssetContract` — update a TRC10 token's description, url, and
//! free-bandwidth limits.
//!
//! Mirrors java-tron `UpdateAssetActuator` (V2 path): the owner account must
//! exist and have issued an asset (`Account.asset_issued_id`), the asset record
//! must exist, the url/description must be within bounds, and the new limits must
//! be in `[0, oneDayNetLimit)`. Execute writes the fields onto the stored
//! `AssetIssueContract`.
//!
//! Deviations from java-tron:
//! - V2 asset store only (see I02); the legacy name-keyed store is not modelled.
//! - `oneDayNetLimit` reads a dynamic property, defaulting to
//!   [`DEFAULT_ONE_DAY_NET_LIMIT`] when unset.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::UpdateAssetContract;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// java-tron `TransactionUtil.MAX_URL_LEN`.
const MAX_URL_LEN: usize = 256;
/// java-tron `TransactionUtil.MAX_ASSET_DESCRIPTION` (assetV2 description bound).
const MAX_ASSET_DESCRIPTION_LEN: usize = 200;
/// Dynamic-property key for the one-day free-net ceiling.
const ONE_DAY_NET_LIMIT: &str = "ONE_DAY_NET_LIMIT";
/// java-tron genesis default `getOneDayNetLimit()` (57_600_000_000).
pub const DEFAULT_ONE_DAY_NET_LIMIT: i64 = 57_600_000_000;

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid ownerAddress".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid ownerAddress".into()))
}

/// java-tron `TransactionUtil.validUrl`: non-empty, `<= MAX_URL_LEN`.
fn valid_url(url: &[u8]) -> bool {
    !url.is_empty() && url.len() <= MAX_URL_LEN
}

/// java-tron `TransactionUtil.validAssetDescription`: empty allowed, `<= 200`.
fn valid_description(desc: &[u8]) -> bool {
    desc.len() <= MAX_ASSET_DESCRIPTION_LEN
}

/// Parse the account's issued-asset id (ascii token id) to a numeric id.
fn issued_asset_id(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok().and_then(|s| s.parse::<i64>().ok())
}

pub struct UpdateAssetActuator<'a> {
    contract: &'a UpdateAssetContract,
}

impl<'a> UpdateAssetActuator<'a> {
    pub fn new(contract: &'a UpdateAssetContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist".into()))?;

        // V2 path: the owner must have issued an asset that still exists.
        if account.asset_issued_id.is_empty() {
            return Err(ActuatorError::Validate("Account has not issued any asset".into()));
        }
        let id = issued_asset_id(&account.asset_issued_id)
            .ok_or_else(|| ActuatorError::Validate("Account has not issued any asset".into()))?;
        if state.get_asset_issue(id)?.is_none() {
            return Err(ActuatorError::Validate(
                "Asset is not existed in AssetIssueV2Store".into(),
            ));
        }

        if !valid_url(&self.contract.url) {
            return Err(ActuatorError::Validate("Invalid url".into()));
        }
        if !valid_description(&self.contract.description) {
            return Err(ActuatorError::Validate("Invalid description".into()));
        }

        let day_limit = self.one_day_net_limit(state)?;
        if self.contract.new_limit < 0 || self.contract.new_limit >= day_limit {
            return Err(ActuatorError::Validate("Invalid FreeAssetNetLimit".into()));
        }
        if self.contract.new_public_limit < 0 || self.contract.new_public_limit >= day_limit {
            return Err(ActuatorError::Validate("Invalid PublicFreeAssetNetLimit".into()));
        }

        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        let id = issued_asset_id(&account.asset_issued_id)
            .ok_or_else(|| ActuatorError::Execute("owner has no issued asset".into()))?;
        let mut asset = state
            .get_asset_issue(id)?
            .ok_or_else(|| ActuatorError::Execute("asset record missing".into()))?;

        asset.free_asset_net_limit = self.contract.new_limit;
        asset.public_free_asset_net_limit = self.contract.new_public_limit;
        asset.url = self.contract.url.clone();
        asset.description = self.contract.description.clone();
        state.put_asset_issue(id, &asset)?;

        Ok(ExecutionResult { fee: 0 })
    }

    fn one_day_net_limit<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let v = state.get_prop_i64(ONE_DAY_NET_LIMIT)?;
        Ok(if v > 0 { v } else { DEFAULT_ONE_DAY_NET_LIMIT })
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

    const ID: i64 = 1_000_001;

    /// State with an owner who issued token `ID`, and the asset record present.
    fn seeded(owner: &Address, issued: &[u8]) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            asset_issued_id: issued.to_vec(),
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        if !issued.is_empty() {
            let asset = protocol::AssetIssueContract {
                id: String::from_utf8_lossy(issued).into_owned(),
                owner_address: owner.as_bytes().to_vec(),
                name: b"MyToken".to_vec(),
                total_supply: 1_000_000,
                url: b"http://old.example".to_vec(),
                description: b"old".to_vec(),
                ..Default::default()
            };
            let id = issued_asset_id(issued).unwrap();
            ws.put_asset_issue(id, &asset).unwrap();
        }
        ws
    }

    fn contract(owner: &Address, limit: i64, public_limit: i64) -> UpdateAssetContract {
        UpdateAssetContract {
            owner_address: owner.as_bytes().to_vec(),
            description: b"a new description".to_vec(),
            url: b"http://new.example.com".to_vec(),
            new_limit: limit,
            new_public_limit: public_limit,
        }
    }

    #[test]
    fn happy_path_updates_fields() {
        let o = addr(1);
        let mut ws = seeded(&o, b"1000001");
        let c = contract(&o, 5_000, 6_000);
        let a = UpdateAssetActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let asset = ws.get_asset_issue(ID).unwrap().unwrap();
        assert_eq!(asset.free_asset_net_limit, 5_000);
        assert_eq!(asset.public_free_asset_net_limit, 6_000);
        assert_eq!(asset.url, b"http://new.example.com".to_vec());
        assert_eq!(asset.description, b"a new description".to_vec());
    }

    #[test]
    fn rejects_negative_limit() {
        let o = addr(1);
        let ws = seeded(&o, b"1000001");
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&o, -1, 0)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid FreeAssetNetLimit")
        ));
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&o, 0, -1)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid PublicFreeAssetNetLimit")
        ));
    }

    #[test]
    fn rejects_limit_at_or_above_day_ceiling() {
        let o = addr(1);
        let ws = seeded(&o, b"1000001");
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&o, DEFAULT_ONE_DAY_NET_LIMIT, 0)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid FreeAssetNetLimit")
        ));
    }

    #[test]
    fn rejects_owner_with_no_asset() {
        let o = addr(1);
        let ws = seeded(&o, b""); // account exists but issued nothing
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&o, 1, 1)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Account has not issued any asset")
        ));
    }

    #[test]
    fn rejects_missing_asset_record() {
        let o = addr(1);
        // Account claims issued id but the record is absent.
        let ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: o.as_bytes().to_vec(),
            asset_issued_id: b"1000001".to_vec(),
            ..Default::default()
        };
        ws.put_account(&o, &account).unwrap();
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&o, 1, 1)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Asset is not existed")
        ));
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(matches!(
            UpdateAssetActuator::new(&contract(&addr(1), 1, 1)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Account does not exist")
        ));
    }

    #[test]
    fn rejects_bad_url_and_description() {
        let o = addr(1);
        let ws = seeded(&o, b"1000001");
        // empty url
        let mut c = contract(&o, 1, 1);
        c.url = b"".to_vec();
        assert!(matches!(
            UpdateAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid url")
        ));
        // over-long description
        let mut c = contract(&o, 1, 1);
        c.description = vec![b'a'; MAX_ASSET_DESCRIPTION_LEN + 1];
        assert!(matches!(
            UpdateAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid description")
        ));
    }
}
