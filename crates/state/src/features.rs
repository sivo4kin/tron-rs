//! Committee feature gates — java-tron `DynamicPropertiesStore` `getAllowX()` /
//! `supportX()` methods, each of which reads an `ALLOW_*` dynamic property and
//! compares it to `1`. Actuators gated behind a not-yet-activated feature reject
//! the transaction (see `tron_actuators::require_feature`).
//!
//! Defaults: every flag is **off** (0) until the committee activates it — a fresh
//! [`WorldState`] returns 0 for an unset property, matching a genesis chain before
//! the corresponding proposal passes. Tests that exercise a gated actuator's happy
//! path must enable the flag with `put_prop_i64(KEY, 1)`.

use crate::WorldState;
use tron_storage::KvStore;

/// `ALLOW_*` dynamic-property keys (java-tron `DynamicPropertiesStore` byte-keys).
pub mod flags {
    /// TVM Constantinople upgrade — gates `ClearABIContract` and, via
    /// `checkForEnergyLimit`, `UpdateEnergyLimitContract`. Mainnet-activated.
    pub const ALLOW_TVM_CONSTANTINOPLE: &str = "ALLOW_TVM_CONSTANTINOPLE";
    /// On-chain market (DEX) transactions — gates `MarketSellAsset` / `MarketCancelOrder`.
    pub const ALLOW_MARKET_TRANSACTION: &str = "ALLOW_MARKET_TRANSACTION";
    /// Stake 2.0 "cancel all unfreeze" — gates `CancelAllUnfreezeV2Contract`.
    pub const ALLOW_CANCEL_ALL_UNFREEZE_V2: &str = "ALLOW_CANCEL_ALL_UNFREEZE_V2";
    /// Voter-reward delegation change — gates `UpdateBrokerageContract`
    /// (java `allowChangeDelegation`, key `CHANGE_DELEGATION`).
    pub const CHANGE_DELEGATION: &str = "CHANGE_DELEGATION";
    /// New resource model — permits `TRON_POWER` freezing (V1/V2).
    pub const ALLOW_NEW_RESOURCE_MODEL: &str = "ALLOW_NEW_RESOURCE_MODEL";
    /// Same-token-name (TRC10 id) model — affects the asset-family V2 path.
    pub const ALLOW_SAME_TOKEN_NAME: &str = "ALLOW_SAME_TOKEN_NAME";
}

impl<S: KvStore> WorldState<S> {
    /// java-tron `getAllowX()`/`supportX()`: the feature is on iff its dynamic
    /// property equals `1`. An unset property (default 0) reads as off. Storage
    /// errors are treated as off (they cannot occur for `MemoryStore`).
    pub fn feature_enabled(&self, key: &str) -> bool {
        self.get_prop_i64(key).unwrap_or(0) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::flags;
    use crate::WorldState;
    use tron_storage::MemoryStore;

    #[test]
    fn feature_off_by_default_on_by_value_one() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(!ws.feature_enabled(flags::ALLOW_MARKET_TRANSACTION));
        ws.put_prop_i64(flags::ALLOW_MARKET_TRANSACTION, 1).unwrap();
        assert!(ws.feature_enabled(flags::ALLOW_MARKET_TRANSACTION));
        // Any value other than exactly 1 is off (matches java `== 1`).
        ws.put_prop_i64(flags::ALLOW_MARKET_TRANSACTION, 2).unwrap();
        assert!(!ws.feature_enabled(flags::ALLOW_MARKET_TRANSACTION));
    }
}
