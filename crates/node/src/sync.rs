//! Sync driver: validate and apply a batch of synced blocks in order.
//!
//! Bridges the P3 channel sync to P1 storage + P3 validation: each fetched block
//! is decoded, structurally validated ([`tron_consensus::validation`]), checked
//! for contiguous linkage to our head, and stored. Stops at the first invalid or
//! out-of-order block (a peer feeding a bad chain gets no further than the fault).

use prost::Message;
use tron_consensus::validation::{
    validate_block, validate_block_intake, BlockValidationError, ValidationOptions,
};
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
///
/// When the current active-witness set is known, prefer [`apply_synced_blocks_gated`],
/// which additionally rejects blocks not signed by an active witness *before*
/// storing them (audit CS-JTRON-006/-007). This entry point keeps the ungated
/// behavior for signature-stripped gateway sources where the producer can't be
/// checked anyway.
pub fn apply_synced_blocks<S: KvStore>(
    state: &WorldState<S>,
    blocks: &[Vec<u8>],
    require_sig: bool,
) -> Result<usize, SyncError> {
    apply_synced_blocks_gated(state, blocks, require_sig, None)
}

/// As [`apply_synced_blocks`], but when `active_witnesses` is `Some`, each block
/// must pass the full intake gate — self-consistent **and** signed by a member of
/// the active-witness set — before it is stored. This is the peer-facing path:
/// an unprivileged peer cannot make us store/apply/broadcast a block no scheduled
/// producer signed. With `None`, behaves exactly like the ungated path.
pub fn apply_synced_blocks_gated<S: KvStore>(
    state: &WorldState<S>,
    blocks: &[Vec<u8>],
    require_sig: bool,
    active_witnesses: Option<&[Vec<u8>]>,
) -> Result<usize, SyncError> {
    let opts = ValidationOptions { require_witness_signature: require_sig };
    let mut applied = 0;
    for bytes in blocks {
        let block = tron_proto::protocol::Block::decode(bytes.as_slice())
            .map_err(|e| SyncError::Decode(e.to_string()))?;
        match active_witnesses {
            Some(ws) => validate_block_intake(&block, ws).map_err(SyncError::Invalid)?,
            None => validate_block(&block, opts).map_err(SyncError::Invalid)?,
        }

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

/// One round of peer-managed sync: pick the best peer ahead of our head, connect
/// over TCP, fetch the blocks it offers, and validate+apply them. Returns the
/// number of blocks applied (0 if no peer is ahead). Ties [`tron_p2p::peer`],
/// the channel sync, and [`apply_synced_blocks`] together.
pub async fn sync_from_best_peer<S: KvStore>(
    state: &WorldState<S>,
    peers: &tron_p2p::peer::PeerManager,
    require_sig: bool,
) -> Result<usize, SyncError> {
    let our_head = state
        .get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER)
        .map_err(|e| SyncError::State(e.to_string()))?;
    let Some(target) = peers.best_sync_target(our_head) else {
        return Ok(0);
    };
    let addr = format!("{}:{}", target.addr.host, target.addr.port);

    let mut stream = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| SyncError::State(format!("connect {addr}: {e}")))?;
    let fetched = tron_p2p::channel::sync_from(&mut stream, our_head)
        .await
        .map_err(|e| SyncError::State(format!("sync {addr}: {e:?}")))?;

    let block_bytes: Vec<Vec<u8>> = fetched.into_iter().map(|(_, b)| b).collect();
    apply_synced_blocks(state, &block_bytes, require_sig)
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
    fn gated_admits_active_witness_chain_and_rejects_foreign_producer() {
        use tron_crypto::{address_from_public_key, public_key};
        let sk = SecretKey::from_slice(&[0x88u8; 32]).unwrap(); // same key chain() signs with
        let producer = address_from_public_key(&public_key(&sk)).as_bytes().to_vec();
        let (blocks, g) = chain(3);

        // Producer is an active witness -> the whole chain applies through the gate.
        let ws = WorldState::new(MemoryStore::new());
        ws.put_block(&g).unwrap();
        let active = vec![vec![0x41u8; 21], producer.clone()];
        let applied = apply_synced_blocks_gated(&ws, &blocks, true, Some(&active)).unwrap();
        assert_eq!(applied, 3);

        // Producer is NOT in the active set -> the first block is rejected before
        // it is ever stored; nothing is applied.
        let ws2 = WorldState::new(MemoryStore::new());
        ws2.put_block(&g).unwrap();
        let foreign = vec![vec![0x41u8; 21]];
        let err = apply_synced_blocks_gated(&ws2, &blocks, true, Some(&foreign)).unwrap_err();
        assert!(matches!(
            err,
            SyncError::Invalid(BlockValidationError::SignerNotActiveWitness { .. })
        ));
        assert_eq!(ws2.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(), 0);
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
