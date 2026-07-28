//! Cross-node integration: node A produces a block chain, node B syncs it over a
//! real TCP socket and stores it — ending with identical heads and block ids.
//!
//! Ties together block production (P5), the TCP channel sync (P3), and block
//! storage (P1) end to end across two independent in-process world states.

use prost::Message;
use tron_chain::block_id_of;
use tron_consensus::producer::produce_block;
use tron_crypto::SecretKey;
use tron_p2p::channel::{serve_sync, sync_from};
use tron_state::WorldState;
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

#[tokio::test]
async fn node_b_syncs_node_a_chain_over_tcp() {
    // --- Node A: build and store a 3-block chain on top of genesis. ---
    let sk = SecretKey::from_slice(&[0x77u8; 32]).unwrap();
    let mut a = WorldState::new(MemoryStore::new());
    a.put_block(&genesis()).unwrap();

    let mut parent = genesis();
    for i in 1..=3i64 {
        let block = produce_block(&parent, &sk, i * 3000, vec![], 30);
        a.put_block(&block).unwrap();
        parent = block;
    }
    let a_head = a.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap();
    assert_eq!(a_head, 3);

    // Snapshot A's block bytes for the sync server.
    let a_blocks: std::collections::HashMap<i64, Vec<u8>> = (1..=3)
        .map(|n| (n, a.get_block_by_num(n).unwrap().unwrap().encode_to_vec()))
        .collect();

    // --- Serve A over TCP; Node B (starting at head 0) syncs. ---
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        serve_sync(&mut sock, &[1, 2, 3], move |n| a_blocks.get(&n).cloned()).await.unwrap()
    });

    let mut b = WorldState::new(MemoryStore::new());
    b.put_block(&genesis()).unwrap();

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let fetched = sync_from(&mut client, 0).await.unwrap();
    drop(client);

    // Node B decodes and stores each synced block.
    for (_n, bytes) in &fetched {
        let block = tron_proto::protocol::Block::decode(bytes.as_slice()).unwrap();
        b.put_block(&block).unwrap();
    }

    // --- Both nodes now agree on head and per-block ids. ---
    assert_eq!(server.await.unwrap(), 3);
    assert_eq!(
        b.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(),
        a_head
    );
    for n in 1..=3 {
        let ida = block_id_of(&a.get_block_by_num(n).unwrap().unwrap()).unwrap();
        let idb = block_id_of(&b.get_block_by_num(n).unwrap().unwrap()).unwrap();
        assert_eq!(ida, idb, "block {n} differs between nodes");
    }
}
