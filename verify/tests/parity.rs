//! Differential parity tests against committed live-chain fixtures (SPEC section 7).
//!
//! Each fixture is a raw `protocol.Block` captured from a java-tron Nile node.
//! We recompute what java-tron computed and assert byte equality:
//! - `txTrieRoot` recomputed from transaction bytes must equal the header field
//!   (this transitively proves prost re-encoding of every tx is byte-identical to
//!   java-tron's serialization — any drift would change the leaves and the root).
//! - the parent-hash linkage layout (block id = BE height + hash tail).

use tron_chain::{block_id_of, tx_id, tx_trie_root};

#[test]
fn fixtures_present() {
    let names = tron_verify::fixture_names().expect("fixture dir readable");
    assert!(
        !names.is_empty(),
        "no committed fixtures — run `cargo run -p tron-verify --bin capture <nums>`"
    );
}

#[test]
fn tx_trie_root_matches_java_tron_header() {
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        let raw = block
            .block_header
            .as_ref()
            .and_then(|h| h.raw_data.as_ref())
            .expect("header");
        let expected = hex::encode(&raw.tx_trie_root);
        let computed = tx_trie_root(&block).to_hex();
        assert_eq!(
            computed, expected,
            "txTrieRoot mismatch on {name} ({} txs)",
            block.transactions.len()
        );
    }
}

#[test]
fn block_id_embeds_height_and_hash() {
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        let id = block_id_of(&block).expect("block id");
        // First 8 bytes are the big-endian height.
        assert_eq!(&id.0[..8], &raw.number.to_be_bytes(), "height prefix on {name}");
        // Parent linkage: parent_hash's height prefix is number-1.
        assert_eq!(
            &raw.parent_hash[..8],
            &(raw.number - 1).to_be_bytes(),
            "parent height prefix on {name}"
        );
    }
}

#[test]
fn tx_ids_are_nonzero_and_unique() {
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        let ids: Vec<String> = block.transactions.iter().map(|t| tx_id(t).to_hex()).collect();
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(ids.len(), dedup.len(), "duplicate tx ids in {name}");
        for id in ids {
            assert_ne!(id, "0".repeat(64), "zero tx id in {name}");
        }
    }
}
