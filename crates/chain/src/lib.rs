//! Block / transaction model and the transaction merkle root (P1).
//!
//! The `txTrieRoot` is a binary SHA256 merkle over transaction ids and must match
//! java-tron's `MerkleRoot` bit-for-bit (empty → zero; odd node carried up;
//! parent = SHA256(left‖right)). opentron's `merkle-tree` is a parity-correct
//! reference. Only the empty case is implemented in P0.

use tron_crypto::sha256;
use tron_types::H256;

/// Compute the transaction merkle root over ordered transaction ids.
///
/// P0: the empty case (→ `H256::ZERO`) is final and parity-correct; the general
/// case is implemented in P1 against the differential harness.
pub fn tx_merkle_root(tx_ids: &[H256]) -> H256 {
    match tx_ids {
        [] => H256::ZERO,
        [single] => *single,
        _ => {
            // P1: full binary-merkle with odd-node carry, parent = SHA256(l‖r).
            // Placeholder folds pairwise so the type/signature is exercised; NOT
            // yet java-tron parity — replaced in P1.
            let mut level: Vec<H256> = tx_ids.to_vec();
            while level.len() > 1 {
                let mut next = Vec::with_capacity(level.len().div_ceil(2));
                for pair in level.chunks(2) {
                    if pair.len() == 2 {
                        let mut buf = [0u8; 64];
                        buf[..32].copy_from_slice(&pair[0].0);
                        buf[32..].copy_from_slice(&pair[1].0);
                        next.push(H256(sha256(&buf)));
                    } else {
                        next.push(pair[0]);
                    }
                }
                level = next;
            }
            level[0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(tx_merkle_root(&[]), H256::ZERO);
    }

    #[test]
    fn single_root_is_identity() {
        let h = H256([7u8; 32]);
        assert_eq!(tx_merkle_root(&[h]), h);
    }
}
