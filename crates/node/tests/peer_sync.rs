//! End-to-end peer-managed sync: pick best peer -> connect -> sync -> apply.

use prost::Message;
use tron_consensus::producer::produce_block;
use tron_crypto::SecretKey;
use tron_node::sync::sync_from_best_peer;
use tron_p2p::channel::serve_sync;
use tron_p2p::peer::PeerManager;
use tron_p2p::PeerAddr;
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
async fn node_syncs_from_the_best_peer_and_applies() {
    // A peer serves a 3-block chain.
    let sk = SecretKey::from_slice(&[0x99u8; 32]).unwrap();
    let mut parent = genesis();
    let mut blocks = std::collections::HashMap::new();
    for i in 1..=3i64 {
        let b = produce_block(&parent, &sk, i * 3000, vec![], 30);
        blocks.insert(i, b.encode_to_vec());
        parent = b;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = serve_sync(&mut sock, &[1, 2, 3], move |n| blocks.get(&n).cloned()).await;
    });

    // Our node at genesis, with a peer manager pointing at the server.
    let mut ws = WorldState::new(MemoryStore::new());
    ws.put_block(&genesis()).unwrap();
    let mut peers = PeerManager::new();
    peers.upsert(PeerAddr::new(addr.ip().to_string(), addr.port()), 3, 1);

    let applied = sync_from_best_peer(&mut ws, &peers, false).await.unwrap();
    assert_eq!(applied, 3);
    assert_eq!(ws.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(), 3);

    // No peer ahead -> nothing to do.
    let mut caught_up = PeerManager::new();
    caught_up.upsert(PeerAddr::new("127.0.0.1", 1), 3, 1);
    assert_eq!(sync_from_best_peer(&mut ws, &caught_up, false).await.unwrap(), 0);
}


#[tokio::test]
async fn shared_arc_world_state_can_sync_while_shared() {
    // Prove Arc<WorldState> (as the HTTP server holds) can be synced into via &self.
    use std::sync::Arc;
    let sk = SecretKey::from_slice(&[0xa1u8; 32]).unwrap();
    let mut parent = genesis();
    let mut blocks = std::collections::HashMap::new();
    for i in 1..=2i64 {
        let b = produce_block(&parent, &sk, i * 3000, vec![], 30);
        blocks.insert(i, b.encode_to_vec());
        parent = b;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let _ = serve_sync(&mut sock, &[1, 2], move |n| blocks.get(&n).cloned()).await;
    });

    let ws = Arc::new(WorldState::new(MemoryStore::new()));
    ws.put_block(&genesis()).unwrap(); // &self mutation through the Arc
    let ws_reader = ws.clone(); // a second handle, as the HTTP server would hold

    let mut peers = PeerManager::new();
    peers.upsert(PeerAddr::new(addr.ip().to_string(), addr.port()), 2, 1);

    // Sync into the shared state via &self.
    let applied = sync_from_best_peer(&ws, &peers, false).await.unwrap();
    assert_eq!(applied, 2);
    // The other Arc handle observes the synced blocks.
    assert_eq!(ws_reader.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap(), 2);
}
