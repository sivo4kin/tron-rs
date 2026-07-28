//! RocksDB-backed [`KvStore`] (feature `rocksdb`).
//!
//! Matches the storage direction of the java-tron JDK 17 profile (RocksDB-only).
//! Column families are created on open from a fixed list; opening an existing DB
//! discovers and reuses its families.

use crate::{KvStore, StorageError};
use rocksdb::{ColumnFamilyDescriptor, Options, DB};
use std::path::Path;

/// Column families created on open. Mirrors `tron_state::cf` plus room to grow —
/// RocksDB requires families to exist before use.
pub const DEFAULT_CFS: &[&str] = &[
    "account",
    "contract",
    "contract_code",
    "contract_storage",
    "witness",
    "votes",
    "asset",
    "proposal",
    "properties",
    "block",
    "transaction",
];

pub struct RocksStore {
    db: DB,
}

impl RocksStore {
    /// Open (or create) a database at `path` with the default column families.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        // Reuse existing families if present (a previously created DB may have more).
        let existing = DB::list_cf(&Options::default(), &path).unwrap_or_default();
        let mut names: Vec<String> = DEFAULT_CFS.iter().map(|s| s.to_string()).collect();
        for cf in existing {
            if !names.contains(&cf) {
                names.push(cf);
            }
        }
        let descriptors: Vec<ColumnFamilyDescriptor> = names
            .into_iter()
            .map(|n| ColumnFamilyDescriptor::new(n, Options::default()))
            .collect();

        let db = DB::open_cf_descriptors(&opts, path, descriptors)
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(Self { db })
    }

    fn cf(&self, name: &str) -> Result<&rocksdb::ColumnFamily, StorageError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| StorageError::UnknownCf(name.to_string()))
    }
}

impl KvStore for RocksStore {
    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.db
            .get_cf(self.cf(cf)?, key)
            .map_err(|e| StorageError::Backend(e.to_string()))
    }

    fn put(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.db
            .put_cf(self.cf(cf)?, key, value)
            .map_err(|e| StorageError::Backend(e.to_string()))
    }

    fn delete(&self, cf: &str, key: &[u8]) -> Result<(), StorageError> {
        self.db
            .delete_cf(self.cf(cf)?, key)
            .map_err(|e| StorageError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = RocksStore::open(dir.path()).unwrap();
            db.put("account", b"k", b"v").unwrap();
            assert_eq!(db.get("account", b"k").unwrap(), Some(b"v".to_vec()));
            db.put("properties", b"p", b"1").unwrap();
            db.delete("account", b"k").unwrap();
            assert_eq!(db.get("account", b"k").unwrap(), None);
        }
        // Reopen: data persists across process lifetimes.
        let db = RocksStore::open(dir.path()).unwrap();
        assert_eq!(db.get("properties", b"p").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn unknown_cf_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksStore::open(dir.path()).unwrap();
        assert!(matches!(
            db.get("nope", b"k"),
            Err(StorageError::UnknownCf(_))
        ));
    }

    #[test]
    fn families_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let db = RocksStore::open(dir.path()).unwrap();
        db.put("account", b"k", b"a").unwrap();
        db.put("contract", b"k", b"c").unwrap();
        assert_eq!(db.get("account", b"k").unwrap(), Some(b"a".to_vec()));
        assert_eq!(db.get("contract", b"k").unwrap(), Some(b"c".to_vec()));
    }
}
