//! Sequential full-chain replay over a contiguous range of real Nile blocks.
//!
//! The heart of the SPEC section 7 parity gate at chain scale: feed a contiguous
//! block range through validation + storage IN ORDER and assert the chain is
//! self-consistent on real data:
//!   - each block passes structural validation (txTrieRoot, parent-link),
//!   - **cross-block linkage**: block N's parent_hash equals block N-1's block id
//!     (the actual hash-chain, computed by us from the previous real block),
//!   - the head advances by exactly 1 each block.
//!
//! This is stronger than per-block checks: it proves our block-id / hashing is
//! consistent with how java-tron actually chained these blocks together.

use tron_chain::block_id_of;
use tron_consensus::validation::{validate_block, ValidationOptions};
use tron_state::WorldState;
use tron_storage::MemoryStore;

/// Load the contiguous nile-<n> fixtures as an ascending Vec<(number, Block)>.
fn contiguous_nile_blocks() -> Vec<(i64, tron_proto::protocol::Block)> {
    let mut blocks: Vec<(i64, tron_proto::protocol::Block)> = tron_verify::fixture_names()
        .unwrap()
        .into_iter()
        .filter(|n| n.starts_with("nile-"))
        .filter_map(|name| {
            let num: i64 = name.strip_prefix("nile-")?.parse().ok()?;
            Some((num, tron_verify::load_block(&name).unwrap()))
        })
        .collect();
    blocks.sort_by_key(|(n, _)| *n);
    // Keep only the longest contiguous run.
    let mut best: Vec<(i64, tron_proto::protocol::Block)> = Vec::new();
    let mut run: Vec<(i64, tron_proto::protocol::Block)> = Vec::new();
    for (n, b) in blocks {
        if run.last().map(|(p, _)| *p + 1 == n).unwrap_or(true) {
            run.push((n, b));
        } else {
            if run.len() > best.len() { best = std::mem::take(&mut run); }
            run = vec![(n, b)];
        }
    }
    if run.len() > best.len() { best = run; }
    best
}

#[test]
fn sequential_blocks_validate_and_chain_link() {
    let blocks = contiguous_nile_blocks();
    assert!(blocks.len() >= 5, "need a contiguous run of real blocks, got {}", blocks.len());

    let mut ws = WorldState::new(MemoryStore::new());
    let opts = ValidationOptions { require_witness_signature: false }; // Nile strips sigs
    let mut prev_id: Option<Vec<u8>> = None;
    let mut prev_num: Option<i64> = None;

    for (num, block) in &blocks {
        // Structural validation on each real block.
        validate_block(block, opts)
            .unwrap_or_else(|e| panic!("block {num} failed validation: {e}"));

        let raw = block.block_header.as_ref().unwrap().raw_data.as_ref().unwrap();

        // Cross-block linkage: this block's parent_hash == the id we computed for
        // the previous real block.
        if let Some(pid) = &prev_id {
            assert_eq!(
                &raw.parent_hash, pid,
                "block {num} parent_hash != our computed id of block {}",
                prev_num.unwrap()
            );
        }

        // Head advances by exactly one.
        ws.put_block(block).unwrap();
        assert_eq!(
            ws.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(),
            *num
        );
        if let Some(pn) = prev_num {
            assert_eq!(*num, pn + 1, "non-contiguous block sequence");
        }

        prev_id = Some(block_id_of(block).unwrap().0.to_vec());
        prev_num = Some(*num);
    }

    // The stored head is the last block, retrievable via get_now_block.
    let head = ws.get_now_block().unwrap().unwrap();
    assert_eq!(
        head.block_header.unwrap().raw_data.unwrap().number,
        blocks.last().unwrap().0
    );
    println!("replayed {} contiguous real Nile blocks with valid chain linkage", blocks.len());
}
