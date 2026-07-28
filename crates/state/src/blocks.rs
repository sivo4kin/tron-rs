//! Block storage: persist blocks by number and track the head.
//!
//! Mirrors java-tron's `BlockStore` + `BlockIndexStore`: blocks are stored by
//! their 8-byte big-endian number in the `block` column family (prost-encoded
//! `protocol.Block`), and the latest block number is a dynamic property. Enough
//! to serve `getnowblock` / `getblockbynum` and to support linear replay.

use crate::{props, StateError, WorldState};
use prost::Message;
use tron_proto::protocol;
use tron_storage::KvStore;

const CF_BLOCK: &str = "block";

/// Dynamic-property key holding the current head block number.
pub const LATEST_BLOCK_NUMBER: &str = "LATEST_BLOCK_HEADER_NUMBER";

impl<S: KvStore> WorldState<S> {
    /// Persist a block by its header number and advance the head if it is newer.
    pub fn put_block(&mut self, block: &protocol::Block) -> Result<(), StateError> {
        let number = block
            .block_header
            .as_ref()
            .and_then(|h| h.raw_data.as_ref())
            .map(|r| r.number)
            .unwrap_or(0);
        self.db.put(CF_BLOCK, &number.to_be_bytes(), &block.encode_to_vec())?;
        if number >= self.get_prop_i64(LATEST_BLOCK_NUMBER)? {
            self.put_prop_i64(LATEST_BLOCK_NUMBER, number)?;
            if let Some(ts) = block.block_header.as_ref().and_then(|h| h.raw_data.as_ref()).map(|r| r.timestamp) {
                self.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, ts)?;
            }
        }
        Ok(())
    }

    pub fn get_block_by_num(&self, number: i64) -> Result<Option<protocol::Block>, StateError> {
        match self.db.get(CF_BLOCK, &number.to_be_bytes())? {
            Some(bytes) => Ok(Some(protocol::Block::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// The head block (highest stored number).
    pub fn get_now_block(&self) -> Result<Option<protocol::Block>, StateError> {
        let head = self.get_prop_i64(LATEST_BLOCK_NUMBER)?;
        self.get_block_by_num(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;

    fn block(number: i64, ts: i64) -> protocol::Block {
        protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw { number, timestamp: ts, ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn store_and_fetch_by_number() {
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_block(&block(5, 500)).unwrap();
        let got = ws.get_block_by_num(5).unwrap().unwrap();
        assert_eq!(got.block_header.unwrap().raw_data.unwrap().number, 5);
        assert!(ws.get_block_by_num(6).unwrap().is_none());
    }

    #[test]
    fn head_tracks_highest_and_updates_timestamp() {
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_block(&block(1, 100)).unwrap();
        ws.put_block(&block(3, 300)).unwrap();
        ws.put_block(&block(2, 200)).unwrap(); // older, must not move head
        let now = ws.get_now_block().unwrap().unwrap();
        assert_eq!(now.block_header.unwrap().raw_data.unwrap().number, 3);
        assert_eq!(ws.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP).unwrap(), 300);
        assert_eq!(ws.get_prop_i64(LATEST_BLOCK_NUMBER).unwrap(), 3);
    }
}
