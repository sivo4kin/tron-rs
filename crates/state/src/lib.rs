//! World state (P1): typed stores over a [`KvStore`].
//!
//! Layout mirrors java-tron's `chainbase`:
//! - **accounts** — key = 21-byte address, value = prost-encoded `protocol.Account`
//!   (java-tron's own schema, so values stay byte-compatible with its stores).
//! - **dynamic properties** — named global tunables (`DynamicPropertiesStore`),
//!   little chain parameters the committee can change plus running counters.
//! - **TRC10 asset issues** — token definitions keyed by numeric token id, with a
//!   `TOKEN_ID_NUM` allocator; only the V2 (post-`allowSameTokenName`) path is
//!   modelled (the deprecated V1 name-keyed store is omitted). Account token
//!   balances live in `Account.assetV2`.
//!
//! Encodings are guarded by the differential harness (SPEC section 7).

pub mod blocks;
pub mod features;
pub mod genesis;

use prost::Message;
use thiserror::Error;
use tron_proto::protocol;
use tron_storage::{KvStore, StorageError};
use tron_types::Address;

/// Column-family names (mirroring java-tron store names).
pub mod cf {
    pub const ACCOUNT: &str = "account";
    pub const CONTRACT: &str = "contract";
    pub const CONTRACT_CODE: &str = "contract_code";
    pub const CONTRACT_STORAGE: &str = "contract_storage";
    pub const WITNESS: &str = "witness";
    pub const VOTES: &str = "votes";
    pub const ASSET: &str = "asset";
    pub const PROPOSAL: &str = "proposal";
    pub const EXCHANGE: &str = "exchange";
    pub const BLOCK: &str = "block";
    pub const TRANSACTION: &str = "transaction";
    pub const BLOCK_INDEX: &str = "block_index";
    pub const BROKERAGE: &str = "brokerage";
    /// Stake 2.0 delegated-resource records (java-tron `DelegatedResourceStore`).
    pub const DELEGATION: &str = "delegation";
    /// DEX order records, keyed by order id (java-tron `MarketOrderStore`).
    pub const MARKET_ORDER: &str = "market_order";
    /// DEX per-(pair,price) FIFO order-id lists (`MarketPairPriceToOrderStore`).
    pub const MARKET_PAIR_PRICE: &str = "market_pair_price";
    /// DEX per-pair sorted price index (`MarketPairToPriceStore`).
    pub const MARKET_PAIR: &str = "market_pair";
    pub const DYNAMIC_PROPERTIES: &str = "properties";

    /// Every column family the node uses — the RocksDB backend opens exactly
    /// these. Re-exported from `tron_storage` (the single source of truth, since
    /// storage cannot depend on state); the `cf_all_covers_every_const` test
    /// asserts every named constant above appears here.
    pub const ALL: &[&str] = tron_storage::ALL_CFS;
}

/// Dynamic-property keys (java-tron `DynamicPropertiesStore` byte-key names).
pub mod props {
    /// Fee charged when a system contract implicitly creates the target account.
    /// java-tron default 0; committee-adjusted on live networks.
    pub const CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT: &str =
        "CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT";
    /// Total TRX burned (blackhole-optimization path).
    pub const BURN_TRX_AMOUNT: &str = "BURN_TRX_AMOUNT";
    /// Latest block header timestamp (ms) — stamped on implicitly created accounts.
    pub const LATEST_BLOCK_HEADER_TIMESTAMP: &str = "LATEST_BLOCK_HEADER_TIMESTAMP";
    /// Total staked bandwidth weight (TRX) across all accounts — feeds resource pricing.
    pub const TOTAL_NET_WEIGHT: &str = "TOTAL_NET_WEIGHT";
    /// Total staked energy weight (TRX) across all accounts.
    pub const TOTAL_ENERGY_WEIGHT: &str = "TOTAL_ENERGY_WEIGHT";
    /// Total staked Tron-Power weight (TRX) across all accounts.
    pub const TOTAL_TRON_POWER_WEIGHT: &str = "TOTAL_TRON_POWER_WEIGHT";
    /// TRC10 token-id counter (java-tron `TOKEN_ID_NUM`). Base `1_000_000`; each
    /// asset issue increments it and the new token takes `counter + 1`.
    pub const TOKEN_ID_NUM: &str = "TOKEN_ID_NUM";
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("decode error: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// The mutable world state, backed by a key-value store.
pub struct WorldState<S: KvStore> {
    pub db: S,
}

impl<S: KvStore> WorldState<S> {
    pub fn new(db: S) -> Self {
        Self { db }
    }

    // -- accounts ---------------------------------------------------------

    pub fn get_account(&self, addr: &Address) -> Result<Option<protocol::Account>, StateError> {
        match self.db.get(cf::ACCOUNT, addr.as_bytes())? {
            Some(bytes) => Ok(Some(protocol::Account::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    pub fn put_account(
        &self,
        addr: &Address,
        account: &protocol::Account,
    ) -> Result<(), StateError> {
        self.db
            .put(cf::ACCOUNT, addr.as_bytes(), &account.encode_to_vec())
            .map_err(Into::into)
    }

    pub fn account_exists(&self, addr: &Address) -> Result<bool, StateError> {
        Ok(self.db.exists(cf::ACCOUNT, addr.as_bytes())?)
    }

    /// Create a fresh Normal account (java-tron `AccountCapsule` defaults):
    /// zero balance, creation time = latest block timestamp.
    pub fn create_account(&self, addr: &Address) -> Result<protocol::Account, StateError> {
        let account = protocol::Account {
            address: addr.as_bytes().to_vec(),
            r#type: protocol::AccountType::Normal as i32,
            create_time: self.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?,
            ..Default::default()
        };
        self.put_account(addr, &account)?;
        Ok(account)
    }

    // -- brokerage --------------------------------------------------------

    /// Set a witness's brokerage percentage (0..=100), keyed by address.
    pub fn put_brokerage(&self, addr: &Address, pct: i64) -> Result<(), StateError> {
        self.db.put(cf::BROKERAGE, addr.as_bytes(), &pct.to_be_bytes()).map_err(Into::into)
    }

    /// Get a witness's brokerage percentage, defaulting to 20 when unset
    /// (java-tron `DEFAULT_BROKERAGE`).
    pub fn get_brokerage(&self, addr: &Address) -> Result<i64, StateError> {
        match self.db.get(cf::BROKERAGE, addr.as_bytes())? {
            Some(b) if b.len() == 8 => Ok(i64::from_be_bytes(b.as_slice().try_into().unwrap())),
            _ => Ok(20),
        }
    }

    // -- contract code ----------------------------------------------------

    /// Store deployed bytecode for a contract address (java-tron `CodeStore`).
    pub fn put_code(&self, addr: &Address, code: &[u8]) -> Result<(), StateError> {
        self.db.put(cf::CONTRACT_CODE, addr.as_bytes(), code).map_err(Into::into)
    }

    /// Fetch a contract's deployed bytecode (empty if none).
    pub fn get_code(&self, addr: &Address) -> Result<Vec<u8>, StateError> {
        Ok(self.db.get(cf::CONTRACT_CODE, addr.as_bytes())?.unwrap_or_default())
    }

    // -- contract records -------------------------------------------------

    /// Store the full `SmartContract` record (java-tron `ContractStore`), keyed by
    /// the 21-byte contract address. Unlike code, this carries the ABI, resource
    /// settings (`consume_user_resource_percent`, `origin_energy_limit`), etc.
    pub fn put_contract(
        &self,
        addr: &Address,
        contract: &protocol::SmartContract,
    ) -> Result<(), StateError> {
        self.db
            .put(cf::CONTRACT, addr.as_bytes(), &contract.encode_to_vec())
            .map_err(Into::into)
    }

    /// Fetch a contract's `SmartContract` record (`Ok(None)` when absent).
    pub fn get_contract(
        &self,
        addr: &Address,
    ) -> Result<Option<protocol::SmartContract>, StateError> {
        match self.db.get(cf::CONTRACT, addr.as_bytes())? {
            Some(bytes) => Ok(Some(protocol::SmartContract::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    // -- TRC10 asset issues (V2 path) -------------------------------------
    //
    // Only the V2 (post-`allowSameTokenName`) store is modelled: the canonical
    // key is the numeric token id, and account balances live in
    // `Account.assetV2`. The deprecated V1 name-keyed `AssetIssueStore` is not
    // modelled (see the module docs).

    /// Store a TRC10 token definition (java-tron `AssetIssueV2Store`), keyed by
    /// the numeric token id rendered as ascii bytes (e.g. `1000001`).
    pub fn put_asset_issue(
        &self,
        id: i64,
        asset: &protocol::AssetIssueContract,
    ) -> Result<(), StateError> {
        self.db
            .put(cf::ASSET, id.to_string().as_bytes(), &asset.encode_to_vec())
            .map_err(Into::into)
    }

    /// Fetch a TRC10 token definition by id (`Ok(None)` when absent).
    pub fn get_asset_issue(
        &self,
        id: i64,
    ) -> Result<Option<protocol::AssetIssueContract>, StateError> {
        match self.db.get(cf::ASSET, id.to_string().as_bytes())? {
            Some(bytes) => Ok(Some(protocol::AssetIssueContract::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Current token-id counter (java-tron `getTokenIdNum`). Defaults to the
    /// [`TOKEN_ID_BASE`] (`1_000_000`) when unset.
    pub fn get_token_id_num(&self) -> Result<i64, StateError> {
        let v = self.get_prop_i64(props::TOKEN_ID_NUM)?;
        Ok(if v == 0 { TOKEN_ID_BASE } else { v })
    }

    /// Persist the token-id counter (java-tron `saveTokenIdNum`).
    pub fn save_token_id_num(&self, num: i64) -> Result<(), StateError> {
        self.put_prop_i64(props::TOKEN_ID_NUM, num)
    }

    /// Allocate the next token id: increment the counter, persist it, and return
    /// the new id (java-tron AssetIssue flow: `tokenIdNum++; saveTokenIdNum(...)`).
    /// The first allocation returns `TOKEN_ID_BASE + 1` = `1_000_001`.
    pub fn allocate_token_id(&self) -> Result<i64, StateError> {
        let next = self.get_token_id_num()? + 1;
        self.save_token_id_num(next)?;
        Ok(next)
    }

    // -- dynamic properties ----------------------------------------------

    pub fn get_prop_i64(&self, key: &str) -> Result<i64, StateError> {
        match self.db.get(cf::DYNAMIC_PROPERTIES, key.as_bytes())? {
            Some(bytes) if bytes.len() == 8 => {
                Ok(i64::from_be_bytes(bytes.as_slice().try_into().unwrap()))
            }
            _ => Ok(0),
        }
    }

    pub fn put_prop_i64(&self, key: &str, value: i64) -> Result<(), StateError> {
        self.db
            .put(cf::DYNAMIC_PROPERTIES, key.as_bytes(), &value.to_be_bytes())
            .map_err(Into::into)
    }

    /// Add `delta` (may be negative) to a dynamic i64 property, returning the new value.
    /// Used for the global resource-weight totals java-tron maintains on stake changes.
    pub fn add_prop_i64(&self, key: &str, delta: i64) -> Result<i64, StateError> {
        let v = self.get_prop_i64(key)?.saturating_add(delta);
        self.put_prop_i64(key, v)?;
        Ok(v)
    }

    /// Burn TRX (blackhole-optimization path): accumulate into `BURN_TRX_AMOUNT`.
    pub fn burn_trx(&self, amount: i64) -> Result<(), StateError> {
        let total = self.get_prop_i64(props::BURN_TRX_AMOUNT)?;
        self.put_prop_i64(props::BURN_TRX_AMOUNT, total.saturating_add(amount))
    }
}

/// Genesis base of the TRC10 token-id counter (java-tron `getTokenIdNum`
/// default). The next allocated token id is `TOKEN_ID_BASE + 1`.
pub const TOKEN_ID_BASE: i64 = 1_000_000;

/// Read an account's TRC10 balance for token `id` (0 when unheld). Keys the
/// `Account.assetV2` map by the ascii token id (V2 path).
pub fn asset_v2_balance(account: &protocol::Account, id: i64) -> i64 {
    account.asset_v2.get(&id.to_string()).copied().unwrap_or(0)
}

/// Set an account's TRC10 balance for token `id` in its `Account.assetV2` map.
pub fn set_asset_v2_balance(account: &mut protocol::Account, id: i64, amount: i64) {
    account.asset_v2.insert(id.to_string(), amount);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    #[test]
    fn account_roundtrip_prost_encoding() {
        let ws = WorldState::new(MemoryStore::new());
        let a = addr(1);
        assert_eq!(ws.get_account(&a).unwrap(), None);

        let account = protocol::Account {
            address: a.as_bytes().to_vec(),
            balance: 42_000_000,
            ..Default::default()
        };
        ws.put_account(&a, &account).unwrap();
        let loaded = ws.get_account(&a).unwrap().unwrap();
        assert_eq!(loaded.balance, 42_000_000);
        assert_eq!(loaded.address, a.as_bytes().to_vec());
    }

    #[test]
    fn create_account_defaults() {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, 1_700_000_000_000).unwrap();
        let a = addr(2);
        let created = ws.create_account(&a).unwrap();
        assert_eq!(created.balance, 0);
        assert_eq!(created.r#type, protocol::AccountType::Normal as i32);
        assert_eq!(created.create_time, 1_700_000_000_000);
        assert!(ws.account_exists(&a).unwrap());
    }

    #[test]
    fn contract_record_roundtrip() {
        let ws = WorldState::new(MemoryStore::new());
        let a = addr(0xab);
        assert_eq!(ws.get_contract(&a).unwrap(), None);

        let contract = protocol::SmartContract {
            contract_address: a.as_bytes().to_vec(),
            origin_energy_limit: 5_000_000,
            consume_user_resource_percent: 30,
            ..Default::default()
        };
        ws.put_contract(&a, &contract).unwrap();
        let loaded = ws.get_contract(&a).unwrap().unwrap();
        assert_eq!(loaded, contract);
        assert_eq!(loaded.origin_energy_limit, 5_000_000);
    }

    #[test]
    fn props_default_zero_and_roundtrip() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), 0);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1_000_000).unwrap();
        assert_eq!(
            ws.get_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT).unwrap(),
            1_000_000
        );
    }

    #[test]
    fn burn_accumulates() {
        let ws = WorldState::new(MemoryStore::new());
        ws.burn_trx(100).unwrap();
        ws.burn_trx(50).unwrap();
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), 150);
    }

    /// Single-source-of-truth guard: every named `cf::*` constant must appear in
    /// `cf::ALL` (= `tron_storage::ALL_CFS`), so the RocksDB backend opens it.
    #[test]
    fn cf_all_covers_every_const() {
        let named = [
            cf::ACCOUNT, cf::CONTRACT, cf::CONTRACT_CODE, cf::CONTRACT_STORAGE,
            cf::WITNESS, cf::VOTES, cf::ASSET, cf::PROPOSAL, cf::EXCHANGE, cf::BLOCK,
            cf::TRANSACTION, cf::BLOCK_INDEX, cf::BROKERAGE, cf::DELEGATION,
            cf::MARKET_ORDER, cf::MARKET_PAIR_PRICE, cf::MARKET_PAIR,
            cf::DYNAMIC_PROPERTIES,
        ];
        for name in named {
            assert!(cf::ALL.contains(&name), "cf::ALL is missing `{name}`");
        }
        // No stray families: ALL is exactly the named set.
        assert_eq!(cf::ALL.len(), named.len(), "cf::ALL has entries with no named const");
    }

    #[test]
    fn asset_issue_roundtrip() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(ws.get_asset_issue(1_000_001).unwrap(), None);

        let asset = protocol::AssetIssueContract {
            id: "1000001".into(),
            owner_address: addr(7).as_bytes().to_vec(),
            name: b"MyToken".to_vec(),
            abbr: b"MTK".to_vec(),
            total_supply: 1_000_000_000,
            trx_num: 1,
            num: 100,
            precision: 6,
            ..Default::default()
        };
        ws.put_asset_issue(1_000_001, &asset).unwrap();
        let loaded = ws.get_asset_issue(1_000_001).unwrap().unwrap();
        assert_eq!(loaded, asset);
        assert_eq!(loaded.total_supply, 1_000_000_000);
        // A different id is independent.
        assert_eq!(ws.get_asset_issue(1_000_002).unwrap(), None);
    }

    #[test]
    fn token_id_allocator_increments_and_persists() {
        let ws = WorldState::new(MemoryStore::new());
        // Unset counter defaults to the base.
        assert_eq!(ws.get_token_id_num().unwrap(), TOKEN_ID_BASE);
        assert_eq!(TOKEN_ID_BASE, 1_000_000);

        // Each allocation increments by one and persists.
        assert_eq!(ws.allocate_token_id().unwrap(), 1_000_001);
        assert_eq!(ws.get_token_id_num().unwrap(), 1_000_001);
        assert_eq!(ws.allocate_token_id().unwrap(), 1_000_002);
        assert_eq!(ws.get_token_id_num().unwrap(), 1_000_002);

        // Persistence is observable through a fresh WorldState over the same db.
        let raw = ws.get_prop_i64(props::TOKEN_ID_NUM).unwrap();
        assert_eq!(raw, 1_000_002);
    }

    #[test]
    fn asset_v2_balance_helpers() {
        let mut account = protocol::Account {
            address: addr(1).as_bytes().to_vec(),
            ..Default::default()
        };
        // Unheld token reads 0.
        assert_eq!(asset_v2_balance(&account, 1_000_001), 0);
        set_asset_v2_balance(&mut account, 1_000_001, 500);
        assert_eq!(asset_v2_balance(&account, 1_000_001), 500);
        // Map is keyed by the ascii token id.
        assert_eq!(account.asset_v2.get("1000001").copied(), Some(500));
        // Overwrite updates in place.
        set_asset_v2_balance(&mut account, 1_000_001, 900);
        assert_eq!(asset_v2_balance(&account, 1_000_001), 900);
    }
}
