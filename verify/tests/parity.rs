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
fn witness_signature_recovers_to_header_address() {
    // TronGrid's Nile gateway strips witness_signature from served blocks (the
    // mainnet gateway does not), so only signature-bearing fixtures are checked —
    // and at least one such fixture must exist so this test can't silently pass.
    let mut checked = 0;
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        let has_sig = block
            .block_header
            .as_ref()
            .is_some_and(|h| h.witness_signature.len() == 65);
        if !has_sig {
            continue;
        }
        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();
        let recovered = tron_chain::recover_witness(&block)
            .unwrap_or_else(|| panic!("signature present but unrecoverable in {name}"));
        assert_eq!(
            hex::encode(recovered.as_bytes()),
            hex::encode(&raw.witness_address),
            "recovered producer address mismatch on {name}"
        );
        checked += 1;
    }
    assert!(checked > 0, "no signature-bearing fixture — capture a mainnet block");
}

#[test]
fn tx_signatures_recover_to_owner_address() {
    // For every single-signature transaction across all fixtures, the recovered
    // signer must equal the contract's owner_address. Multisig / permission-
    // delegated txs (signer legitimately != owner) are counted separately and
    // must stay a small minority; every signature must at least recover.
    // Measured on live data: Nile traffic is 100% owner-signed, while roughly half
    // of mainnet txs are signed by permission-delegated keys (exchange
    // infrastructure using active-permission signing) — recovered != owner there
    // is correct Tron behavior, not a crypto bug. So: every signature must
    // recover; Nile fixtures must match exactly; mainnet needs a sanity floor
    // (a broken v/digest pipeline would match ~0%).
    let mut grand_total = 0u32;
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        let (mut matched, mut total) = (0u32, 0u32);
        for tx in &block.transactions {
            if tx.signature.len() != 1 {
                continue; // multisig out of scope here
            }
            total += 1;
            let signer = tron_chain::recover_tx_signer(tx)
                .unwrap_or_else(|| panic!("unrecoverable signature in {name}"));
            let owner = tron_chain::tx_owner_address(tx)
                .unwrap_or_else(|| panic!("no owner_address in {name}"));
            if signer.as_bytes().as_slice() == owner.as_slice() {
                matched += 1;
            }
        }
        grand_total += total;
        if total == 0 {
            continue;
        }
        let ratio = f64::from(matched) / f64::from(total);
        println!("{name}: signer==owner {matched}/{total} ({ratio:.4})");
        if name.starts_with("nile-") {
            assert_eq!(matched, total, "{name}: non-delegated testnet txs must all match");
        } else {
            assert!(ratio > 0.30, "{name}: ratio {ratio:.4} too low — pipeline likely wrong");
        }
    }
    assert!(grand_total > 100, "expected a substantial tx corpus, got {grand_total}");
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
