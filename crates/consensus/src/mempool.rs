//! Pending-transaction pool (java-tron `PendingManager` / opentron mempool).
//!
//! Holds not-yet-included transactions keyed by their id ([`tron_chain::tx_id`]),
//! deduplicated, insertion-ordered, and capacity-bounded. Block production drains
//! it in order; tx relay and block application evict included txs.

use std::collections::HashSet;
use tron_chain::tx_id;
use tron_proto::protocol;
use tron_types::H256;

/// Default mempool capacity (bounded to resist flooding).
pub const DEFAULT_CAPACITY: usize = 10_000;

/// An insertion-ordered, deduplicated transaction pool.
pub struct Mempool {
    order: Vec<(H256, protocol::Transaction)>,
    seen: HashSet<H256>,
    capacity: usize,
}

impl Mempool {
    pub fn new(capacity: usize) -> Self {
        Self { order: Vec::new(), seen: HashSet::new(), capacity }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, id: &H256) -> bool {
        self.seen.contains(id)
    }

    /// Add a transaction. Returns `false` if it is a duplicate or the pool is full.
    pub fn add(&mut self, tx: protocol::Transaction) -> bool {
        if self.order.len() >= self.capacity {
            return false;
        }
        let id = tx_id(&tx);
        if !self.seen.insert(id) {
            return false; // duplicate
        }
        self.order.push((id, tx));
        true
    }

    /// Remove a transaction by id (e.g. once included in a block). Returns whether
    /// it was present.
    pub fn remove(&mut self, id: &H256) -> bool {
        if !self.seen.remove(id) {
            return false;
        }
        if let Some(pos) = self.order.iter().position(|(i, _)| i == id) {
            self.order.remove(pos);
        }
        true
    }

    /// Peek the first `n` transactions in insertion order (block assembly).
    pub fn peek(&self, n: usize) -> Vec<protocol::Transaction> {
        self.order.iter().take(n).map(|(_, tx)| tx.clone()).collect()
    }

    /// Evict every transaction included in `block`.
    pub fn evict_included(&mut self, block: &protocol::Block) {
        for tx in &block.transactions {
            self.remove(&tx_id(tx));
        }
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(n: u64) -> protocol::Transaction {
        protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw {
                ref_block_num: n as i64,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn add_dedups_and_orders() {
        let mut pool = Mempool::default();
        assert!(pool.add(tx(1)));
        assert!(pool.add(tx(2)));
        assert!(!pool.add(tx(1))); // duplicate id
        assert_eq!(pool.len(), 2);
        let peeked = pool.peek(10);
        assert_eq!(peeked.len(), 2);
        // insertion order preserved
        assert_eq!(peeked[0].raw_data.as_ref().unwrap().ref_block_num, 1);
    }

    #[test]
    fn capacity_bound_rejects_when_full() {
        let mut pool = Mempool::new(2);
        assert!(pool.add(tx(1)));
        assert!(pool.add(tx(2)));
        assert!(!pool.add(tx(3))); // full
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn remove_and_contains() {
        let mut pool = Mempool::default();
        pool.add(tx(1));
        let id = tx_id(&tx(1));
        assert!(pool.contains(&id));
        assert!(pool.remove(&id));
        assert!(!pool.contains(&id));
        assert!(!pool.remove(&id)); // already gone
        assert!(pool.is_empty());
    }

    #[test]
    fn evict_included_clears_block_txs() {
        let mut pool = Mempool::default();
        pool.add(tx(1));
        pool.add(tx(2));
        pool.add(tx(3));
        let block = protocol::Block {
            transactions: vec![tx(1), tx(3)],
            ..Default::default()
        };
        pool.evict_included(&block);
        assert_eq!(pool.len(), 1);
        assert!(pool.contains(&tx_id(&tx(2))));
    }

    #[test]
    fn peek_takes_prefix_for_block_assembly() {
        let mut pool = Mempool::default();
        for i in 0..5 {
            pool.add(tx(i));
        }
        assert_eq!(pool.peek(3).len(), 3);
        assert_eq!(pool.peek(100).len(), 5);
    }
}
