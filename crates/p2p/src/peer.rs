//! Peer manager: track connected peers and choose sync targets.
//!
//! The stateful half of the sync driver (java-tron `PeerConnection` table): each
//! peer advertises its head via `SyncBlockChain`/`BlockInventory`; the manager
//! records it and picks the best peer to sync from (the highest head above ours).
//! Stale peers are pruned. The async channel loop drives add/update/remove.

use crate::PeerAddr;
use std::collections::HashMap;

/// What we track about a connected peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub addr: PeerAddr,
    /// The peer's advertised head block number (-1 if unknown).
    pub head: i64,
    /// Monotonic "last seen" tick (the caller supplies its clock).
    pub last_seen: u64,
}

/// Manages the set of connected peers.
#[derive(Default)]
pub struct PeerManager {
    peers: HashMap<String, PeerInfo>,
}

impl PeerManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Peer key = "host:port".
    fn key(addr: &PeerAddr) -> String {
        format!("{}:{}", addr.host, addr.port)
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Add or refresh a peer with its advertised head.
    pub fn upsert(&mut self, addr: PeerAddr, head: i64, now: u64) {
        let key = Self::key(&addr);
        self.peers.insert(key, PeerInfo { addr, head, last_seen: now });
    }

    /// Update a known peer's head (no-op if unknown).
    pub fn update_head(&mut self, addr: &PeerAddr, head: i64, now: u64) {
        if let Some(p) = self.peers.get_mut(&Self::key(addr)) {
            p.head = head;
            p.last_seen = now;
        }
    }

    pub fn remove(&mut self, addr: &PeerAddr) -> bool {
        self.peers.remove(&Self::key(addr)).is_some()
    }

    /// The best peer to sync from: the one with the highest head strictly above
    /// `our_head`. `None` if no peer is ahead.
    pub fn best_sync_target(&self, our_head: i64) -> Option<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.head > our_head)
            .max_by_key(|p| p.head)
    }

    /// Drop peers not seen since `cutoff` (stale-peer eviction).
    pub fn prune_older_than(&mut self, cutoff: u64) -> usize {
        let before = self.peers.len();
        self.peers.retain(|_, p| p.last_seen >= cutoff);
        before - self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(port: u16) -> PeerAddr {
        PeerAddr::new("10.0.0.1", port)
    }

    #[test]
    fn upsert_update_remove() {
        let mut pm = PeerManager::new();
        pm.upsert(peer(1), 100, 1);
        pm.upsert(peer(2), 200, 1);
        assert_eq!(pm.len(), 2);
        // upsert same addr updates in place
        pm.upsert(peer(1), 150, 2);
        assert_eq!(pm.len(), 2);
        pm.update_head(&peer(1), 175, 3);
        assert_eq!(pm.best_sync_target(0).unwrap().head, 200); // peer 2 still highest
        assert!(pm.remove(&peer(2)));
        assert_eq!(pm.best_sync_target(0).unwrap().head, 175); // now peer 1
    }

    #[test]
    fn best_sync_target_requires_being_ahead() {
        let mut pm = PeerManager::new();
        pm.upsert(peer(1), 50, 1);
        pm.upsert(peer(2), 80, 1);
        assert_eq!(pm.best_sync_target(80).map(|p| p.head), None); // nobody ahead of 80
        assert_eq!(pm.best_sync_target(60).unwrap().head, 80);
        assert_eq!(pm.best_sync_target(-1).unwrap().head, 80);
    }

    #[test]
    fn prune_stale_peers() {
        let mut pm = PeerManager::new();
        pm.upsert(peer(1), 10, 1);
        pm.upsert(peer(2), 20, 5);
        pm.upsert(peer(3), 30, 9);
        // prune anything last seen before tick 5
        assert_eq!(pm.prune_older_than(5), 1); // peer 1 dropped
        assert_eq!(pm.len(), 2);
    }
}
