//! World state (P1).
//!
//! Wraps a [`KvStore`] with the typed Tron stores — accounts, resources, votes,
//! witnesses, TRC10 assets, contracts + code + storage, proposals, exchanges, and
//! the dynamic-properties (global tunables). Column-family names mirror java-tron's
//! `chainbase` stores; encodings are validated by the differential harness (SPEC §7).

use tron_storage::KvStore;

/// Column-family names (subset; extended in P1).
pub mod cf {
    pub const ACCOUNT: &str = "account";
    pub const CONTRACT: &str = "contract";
    pub const CONTRACT_CODE: &str = "contract_code";
    pub const CONTRACT_STORAGE: &str = "contract_storage";
    pub const WITNESS: &str = "witness";
    pub const VOTES: &str = "votes";
    pub const ASSET: &str = "asset";
    pub const PROPOSAL: &str = "proposal";
    pub const DYNAMIC_PROPERTIES: &str = "dynamic_properties";
}

/// The mutable world state, backed by a key-value store.
pub struct WorldState<S: KvStore> {
    pub db: S,
}

impl<S: KvStore> WorldState<S> {
    pub fn new(db: S) -> Self {
        Self { db }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;

    #[test]
    fn constructs_over_a_kvstore() {
        let ws = WorldState::new(MemoryStore::new());
        ws.db.put(cf::ACCOUNT, b"a", b"1").unwrap();
        assert_eq!(ws.db.get(cf::ACCOUNT, b"a").unwrap(), Some(b"1".to_vec()));
    }
}
