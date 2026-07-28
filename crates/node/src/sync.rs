//! Sync driver: validate and apply a batch of synced blocks in order.
//!
//! Bridges the P3 channel sync to P1 storage + P3 validation: each fetched block
//! is decoded, structurally validated ([`tron_consensus::validation`]), checked
//! for contiguous linkage to our head, and stored. Stops at the first invalid or
//! out-of-order block (a peer feeding a bad chain gets no further than the fault).

use prost::Message;
use tron_consensus::validation::{validate_block, BlockValidationError, ValidationOptions};
use tron_state::WorldState;
use tron_storage::KvStore;

#[derive(Debug)]
pub enum SyncError {
    Decode(String),
    Invalid(BlockValidationError),
    OutOfOrder { expected: i64, got: i64 },
    State(String),
}

/// Decode, validate, and store `blocks` (each raw `protocol.Block` bytes) in order.
/// Returns the number applied. `require_sig` toggles the witness-signature check
/// (off for signature-stripped gateway sources).
pub fn apply_synced_blocks<S: KvStore>(
    state: &mut WorldState<S>,
    blocks: &[Vec<u8>],
    require_sig: bool,
) -> Result<usize, SyncError> {
    let opts = ValidationOptions { require_witness_signature: require_sig };
    let mut applied = 0;
    for bytes in blocks {
        let block = tron_proto::protocol::Block::decode(bytes.as_slice())
            .map_err(|e| SyncError::Decode(e.to_string()))?;
        validate_block(&block, opts).map_err(SyncError::Invalid)?;

        // Contiguity: this block must be head+1.
        let head = state
            .get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER)
            .map_err(|e| SyncError::State(e.to_string()))?;
        let num = block
            .block_header
            .as_ref()
            .and_then(|h| h.raw_data.as_ref())
            .map(|r| r.number)
            .unwrap_or(-1);
        if num != head + 1 {
            return Err(SyncError::OutOfOrder { expected: head + 1, got: num });
        }

        state.put_block(&block).map_err(|e| SyncError::State(e.to_string()))?;
        applied += 1;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_consensus::producer::produce_block;
    use tron_crypto::SecretKey;
    use tron_storage::MemoryStore;

    fn genesis() -> tron_proto::protocol::Block {
        tron_proto::protocol::Block {
            block_header: Some(tron_proto::protocol::BlockHeader {
                raw_data: Some(tron_proto::protocol::block_header::Raw { number: 0, ..Default::default() }),
                witness_signature: vec![],
            }),
            transactions: vec![],
        }
    }

    fn chain(n: i64) -> (Vec<Vec<u8>>, tron_proto::protocol::Block) {
        let sk = SecretKey::from_slice(&[0x88u8; 32]).unwrap();
        let mut parent = genesis();
        let mut out = Vec::new();
        for i in 1..=n {
            let b = produce_block(&parent, &sk, i * 3000, vec![], 30);
            out.push(b.encode_to_vec());
            parent = b;
        }
        (out, genesis())
    }

    #[test]
    fn applies_a_valid_contiguous_chain() {
        let (blocks, g) = chain(4);
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_block(&g).unwrap();
        let applied = apply_synced_blocks(&mut ws, &blocks, true).unwrap();
        assert_eq!(applied, 4);
        assert_eq!(ws.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(), 4);
    }

    #[test]
    fn rejects_a_tampered_block() {
        let (mut blocks, g) = chain(3);
        // Corrupt the second block's bytes.
        let mut tampered = tron_proto::protocol::Block::decode(blocks[1].as_slice()).unwrap();
        tampered.block_header.as_mut().unwrap().witness_signature = vec![0u8; 10];
        blocks[1] = tampered.encode_to_vec();

        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_block(&g).unwrap();
        let err = apply_synced_blocks(&mut ws, &blocks, true).unwrap_err();
        assert!(matches!(err, SyncError::Invalid(_)));
        // Only the first (valid) block was applied before the fault.
        assert_eq!(ws.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(), 1);
    }

    #[test]
    fn rejects_out_of_order_block() {
        let (blocks, g) = chain(3);
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_block(&g).unwrap();
        // Skip block 1 -> block 2 is out of order.
        let err = apply_synced_blocks(&mut ws, &blocks[1..], true).unwrap_err();
        assert!(matches!(err, SyncError::OutOfOrder { expected: 1, got: 2 }));
    }
}
