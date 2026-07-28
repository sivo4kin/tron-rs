//! Block-sync decision logic (java-tron `SyncBlockChainMsgHandler` essence).
//!
//! Given our current head and a peer's chain-inventory of block numbers, decide
//! which blocks to fetch: the contiguous suffix strictly above our head, capped
//! per round. Pure logic — the transport (channel actor) drives it in P3.

/// Max blocks requested per fetch round (java-tron `NET_MAX_TRX_PER_SECOND`-scale
/// batching; the sync path uses a bounded inventory window).
pub const MAX_FETCH_PER_ROUND: usize = 2000;

/// Decide which block numbers to fetch from a peer's advertised inventory.
///
/// `our_head` is the highest block we have; `peer_inventory` is the ascending list
/// of block numbers the peer offers. Returns the numbers we lack, in order, capped.
pub fn blocks_to_fetch(our_head: i64, peer_inventory: &[i64]) -> Vec<i64> {
    peer_inventory
        .iter()
        .copied()
        .filter(|&n| n > our_head)
        .take(MAX_FETCH_PER_ROUND)
        .collect()
}

/// Whether the peer is ahead of us (we should sync from it).
pub fn peer_is_ahead(our_head: i64, peer_head: i64) -> bool {
    peer_head > our_head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetches_only_blocks_above_head() {
        assert_eq!(blocks_to_fetch(5, &[3, 4, 5, 6, 7, 8]), vec![6, 7, 8]);
        assert_eq!(blocks_to_fetch(10, &[3, 4, 5]), Vec::<i64>::new());
        assert_eq!(blocks_to_fetch(-1, &[0, 1, 2]), vec![0, 1, 2]); // from genesis
    }

    #[test]
    fn caps_fetch_round() {
        let inv: Vec<i64> = (0..5000).collect();
        assert_eq!(blocks_to_fetch(-1, &inv).len(), MAX_FETCH_PER_ROUND);
    }

    #[test]
    fn peer_ahead_detection() {
        assert!(peer_is_ahead(5, 6));
        assert!(!peer_is_ahead(5, 5));
        assert!(!peer_is_ahead(5, 4));
    }
}
