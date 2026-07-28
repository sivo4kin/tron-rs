//! broadcasthex admits a valid transaction into the shared mempool.

use std::sync::{Arc, Mutex};
use tron_consensus::mempool::Mempool;
use tron_proto::protocol;
use tron_rpc::server::{router_with_state, NodeState};
use tron_state::WorldState;
use tron_storage::MemoryStore;

#[tokio::test]
async fn broadcasthex_pools_valid_transaction() {
    use prost::Message;
    let world = Arc::new(WorldState::new(MemoryStore::new()));
    let mempool = Arc::new(Mutex::new(Mempool::default()));
    let state = NodeState { world, mempool: mempool.clone() };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let app = router_with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let tx = protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw { ref_block_num: 3, ..Default::default() }),
        signature: vec![vec![0u8; 65]],
        ..Default::default()
    };
    let hex_tx = hex::encode(tx.encode_to_vec());

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("http://{bound}/wallet/broadcasthex"))
        .json(&serde_json::json!({ "transaction": hex_tx }))
        .send().await.unwrap()
        .json().await.unwrap();

    assert_eq!(resp["result"], true);
    // The tx is now in the shared mempool.
    assert_eq!(mempool.lock().unwrap().len(), 1);
}
