//! Storage abstraction for the node's persistent state.
//!
//! Tron state is organized as many typed key-value stores (java-tron's ~37 `*Store`
//! classes; opentron's 17 RocksDB column families). We model that as a [`KvStore`]
//! trait over named **column families**, with an in-memory implementation for tests
//! and early phases. A RocksDB-backed implementation lands in P1 behind the
//! `rocksdb` feature.

use std::collections::HashMap;
use std::sync::RwLock;
use thiserror::Error;

#[cfg(feature = "rocksdb")]
pub mod rocks;
#[cfg(feature = "rocksdb")]
pub use rocks::RocksStore;

/// The canonical set of column families the node uses. This is the single
/// source of truth for [`rocks::RocksStore`], which must declare every family at
/// open time (unlike [`MemoryStore`], which auto-creates on first write). It
/// lives here — not in `tron_state::cf` — because `tron-storage` cannot depend on
/// `tron-state` (the dependency runs the other way). `tron_state::cf` re-exports
/// this as `cf::ALL` and a test there asserts every named `cf::*` const is in it.
///
/// Keep this in sync with `tron_state::cf`: every named family constant must
/// appear here so the RocksDB backend can serve it.
pub const ALL_CFS: &[&str] = &[
    "account",
    "contract",
    "contract_code",
    "contract_storage",
    "witness",
    "votes",
    "asset",
    "proposal",
    "exchange",
    "block",
    "transaction",
    "block_index",
    "brokerage",
    "delegation",
    "market_order",
    "market_pair_price",
    "market_pair",
    "properties",
];

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("unknown column family: {0}")]
    UnknownCf(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A key-value store partitioned into named column families.
pub trait KvStore: Send + Sync {
    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError>;
    fn put(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<(), StorageError>;
    fn delete(&self, cf: &str, key: &[u8]) -> Result<(), StorageError>;
    fn exists(&self, cf: &str, key: &[u8]) -> Result<bool, StorageError> {
        Ok(self.get(cf, key)?.is_some())
    }
}

type Cf = HashMap<Vec<u8>, Vec<u8>>;

/// In-memory [`KvStore`]. Column families are created on first write.
#[derive(Default)]
pub struct MemoryStore {
    inner: RwLock<HashMap<String, Cf>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvStore for MemoryStore {
    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        let map = self.inner.read().map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(map.get(cf).and_then(|c| c.get(key).cloned()))
    }

    fn put(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let mut map = self.inner.write().map_err(|e| StorageError::Backend(e.to_string()))?;
        map.entry(cf.to_string()).or_default().insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, cf: &str, key: &[u8]) -> Result<(), StorageError> {
        let mut map = self.inner.write().map_err(|e| StorageError::Backend(e.to_string()))?;
        if let Some(c) = map.get_mut(cf) {
            c.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let db = MemoryStore::new();
        assert_eq!(db.get("account", b"k").unwrap(), None);
        db.put("account", b"k", b"v").unwrap();
        assert_eq!(db.get("account", b"k").unwrap(), Some(b"v".to_vec()));
        assert!(db.exists("account", b"k").unwrap());
        db.delete("account", b"k").unwrap();
        assert_eq!(db.get("account", b"k").unwrap(), None);
    }

    #[test]
    fn column_families_are_isolated() {
        let db = MemoryStore::new();
        db.put("account", b"k", b"a").unwrap();
        db.put("contract", b"k", b"c").unwrap();
        assert_eq!(db.get("account", b"k").unwrap(), Some(b"a".to_vec()));
        assert_eq!(db.get("contract", b"k").unwrap(), Some(b"c".to_vec()));
    }
}
