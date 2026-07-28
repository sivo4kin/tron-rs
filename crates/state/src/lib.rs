//! World state (P1): typed stores over a [`KvStore`].
//!
//! Layout mirrors java-tron's `chainbase`:
//! - **accounts** — key = 21-byte address, value = prost-encoded `protocol.Account`
//!   (java-tron's own schema, so values stay byte-compatible with its stores).
//! - **dynamic properties** — named global tunables (`DynamicPropertiesStore`),
//!   little chain parameters the committee can change plus running counters.
//!
//! Encodings are guarded by the differential harness (SPEC section 7).

pub mod blocks;
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
    pub const DYNAMIC_PROPERTIES: &str = "properties";
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

    // -- contract code ----------------------------------------------------

    /// Store deployed bytecode for a contract address (java-tron `CodeStore`).
    pub fn put_code(&self, addr: &Address, code: &[u8]) -> Result<(), StateError> {
        self.db.put(cf::CONTRACT_CODE, addr.as_bytes(), code).map_err(Into::into)
    }

    /// Fetch a contract's deployed bytecode (empty if none).
    pub fn get_code(&self, addr: &Address) -> Result<Vec<u8>, StateError> {
        Ok(self.db.get(cf::CONTRACT_CODE, addr.as_bytes())?.unwrap_or_default())
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

    /// Burn TRX (blackhole-optimization path): accumulate into `BURN_TRX_AMOUNT`.
    pub fn burn_trx(&self, amount: i64) -> Result<(), StateError> {
        let total = self.get_prop_i64(props::BURN_TRX_AMOUNT)?;
        self.put_prop_i64(props::BURN_TRX_AMOUNT, total.saturating_add(amount))
    }
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
}
