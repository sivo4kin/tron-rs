//! Block production (P5): assemble and sign a block as a Super Representative.
//!
//! Mirrors java-tron `Manager.generateBlock`: build the header raw (number =
//! parent+1, parentHash = parent id, timestamp = slot time, witness_address,
//! version), attach the ordered transactions, set txTrieRoot, then sign
//! `sha256(headerRaw)` with the witness key and store the 65-byte `r||s||v`
//! signature (v = 27 + recovery id, java-tron/Eth convention).
//!
//! The produced block is, by construction, accepted by [`crate::validation`] and
//! its signature recovers to the witness — the production/validation round-trip.

use prost::Message;
use tron_chain::tx_trie_root;
use tron_crypto::{address_from_public_key, public_key, sha256, sign_digest, SecretKey};
use tron_proto::protocol;

/// Assemble and sign a block on top of `parent`.
pub fn produce_block(
    parent: &protocol::Block,
    witness_key: &SecretKey,
    timestamp: i64,
    transactions: Vec<protocol::Transaction>,
    version: i32,
) -> protocol::Block {
    let parent_raw = parent
        .block_header
        .as_ref()
        .and_then(|h| h.raw_data.as_ref());
    let parent_number = parent_raw.map(|r| r.number).unwrap_or(0);
    let parent_id = tron_chain::block_id_of(parent).map(|h| h.0.to_vec()).unwrap_or(vec![0u8; 32]);

    let witness_address =
        address_from_public_key(&public_key(witness_key)).as_bytes().to_vec();

    // Draft block to compute txTrieRoot over the transactions.
    let mut block = protocol::Block {
        transactions,
        block_header: None,
    };
    let tx_root = tx_trie_root(&block).0.to_vec();

    let raw = protocol::block_header::Raw {
        timestamp,
        tx_trie_root: tx_root,
        parent_hash: parent_id,
        number: parent_number + 1,
        witness_address,
        version,
        ..Default::default()
    };

    // Sign sha256(headerRaw).
    let digest = sha256(&raw.encode_to_vec());
    let sig = sign_digest(witness_key, &digest).expect("sign header");
    let mut witness_signature = Vec::with_capacity(65);
    witness_signature.extend_from_slice(&sig.rs);
    witness_signature.push(27 + sig.recovery_id);

    block.block_header = Some(protocol::BlockHeader {
        raw_data: Some(raw),
        witness_signature,
    });
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::{validate_block, ValidationOptions};

    fn genesis() -> protocol::Block {
        protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw {
                    number: 0,
                    timestamp: 0,
                    ..Default::default()
                }),
                witness_signature: vec![],
            }),
            transactions: vec![],
        }
    }

    #[test]
    fn produced_block_validates_and_links() {
        let sk = SecretKey::from_slice(&[0x33u8; 32]).unwrap();
        let parent = genesis();
        let block = produce_block(&parent, &sk, 3000, vec![], 30);

        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        assert_eq!(raw.number, 1);
        assert_eq!(raw.timestamp, 3000);
        // parent linkage: parent id == our parent_hash
        assert_eq!(raw.parent_hash, tron_chain::block_id_of(&parent).unwrap().0.to_vec());

        // Full structural validation must pass (incl. signature recovery).
        validate_block(&block, ValidationOptions { require_witness_signature: true }).unwrap();
    }

    #[test]
    fn produced_signature_recovers_to_witness() {
        let sk = SecretKey::from_slice(&[0x44u8; 32]).unwrap();
        let expected = address_from_public_key(&public_key(&sk));
        let block = produce_block(&genesis(), &sk, 6000, vec![], 30);
        let recovered = tron_chain::recover_witness(&block).unwrap();
        assert_eq!(recovered, expected);
        // header's witness_address matches too
        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        assert_eq!(raw.witness_address, expected.as_bytes().to_vec());
    }

    #[test]
    fn produced_block_txtrieroot_covers_transactions() {
        let sk = SecretKey::from_slice(&[0x55u8; 32]).unwrap();
        let txs = vec![
            protocol::Transaction {
                raw_data: Some(protocol::transaction::Raw { ref_block_num: 1, ..Default::default() }),
                ..Default::default()
            },
            protocol::Transaction {
                raw_data: Some(protocol::transaction::Raw { ref_block_num: 2, ..Default::default() }),
                ..Default::default()
            },
        ];
        let block = produce_block(&genesis(), &sk, 3000, txs, 30);
        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        // recomputed root over the block equals the header field
        assert_eq!(tron_chain::tx_trie_root(&block).0.to_vec(), raw.tx_trie_root);
        // and it validates
        validate_block(&block, ValidationOptions { require_witness_signature: true }).unwrap();
    }
}
