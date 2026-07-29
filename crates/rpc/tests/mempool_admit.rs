//! broadcasthex runs the consensus admission pipeline and pools a valid tx.

use std::sync::{Arc, Mutex};
use tron_consensus::mempool::Mempool;
use tron_proto::protocol;
use tron_rpc::server::{router_with_state, NodeState};
use tron_state::WorldState;
use tron_storage::MemoryStore;

/// A properly signed transfer tx (no expiry) — passes the admission pipeline.
fn signed_transfer() -> protocol::Transaction {
    use prost::Message;
    use tron_crypto::{address_from_public_key, public_key, sign_digest, SecretKey};
    use tron_types::Address;

    let sk = SecretKey::from_slice(&[0x11u8; 32]).unwrap();
    let owner = address_from_public_key(&public_key(&sk));
    let c = protocol::TransferContract {
        owner_address: owner.as_bytes().to_vec(),
        to_address: Address::from_body([0x02; 20]).as_bytes().to_vec(),
        amount: 1_000,
    };
    let contract = protocol::transaction::Contract {
        r#type: protocol::transaction::contract::ContractType::TransferContract as i32,
        parameter: Some(prost_types::Any {
            type_url: "type.googleapis.com/protocol.TransferContract".into(),
            value: c.encode_to_vec(),
        }),
        ..Default::default()
    };
    let mut tx = protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw { contract: vec![contract], ..Default::default() }),
        ..Default::default()
    };
    let digest = tron_chain::tx_id(&tx).0;
    let sig = sign_digest(&sk, &digest).unwrap();
    let mut sig_bytes = sig.rs.to_vec();
    sig_bytes.push(sig.recovery_id);
    tx.signature = vec![sig_bytes];
    tx
}

#[tokio::test]
async fn broadcasthex_admits_valid_transaction() {
    use prost::Message;
    let world = Arc::new(WorldState::new(MemoryStore::new()));
    let mempool = Arc::new(Mutex::new(Mempool::default()));
    let state = NodeState::new(world).with_mempool(mempool.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let app = router_with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let hex_tx = hex::encode(signed_transfer().encode_to_vec());
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("http://{bound}/wallet/broadcasthex"))
        .json(&serde_json::json!({ "transaction": hex_tx }))
        .send().await.unwrap()
        .json().await.unwrap();

    assert_eq!(resp["result"], true);
    // The signed tx passed the pipeline and is now in the shared mempool.
    assert_eq!(mempool.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn broadcasthex_does_not_pool_unsigned_transaction() {
    use prost::Message;
    let world = Arc::new(WorldState::new(MemoryStore::new()));
    let mempool = Arc::new(Mutex::new(Mempool::default()));
    let state = NodeState::new(world).with_mempool(mempool.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let app = router_with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    // Contractless, junk-signature tx: structurally decodes but fails admission.
    let tx = protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw { ref_block_num: 3, ..Default::default() }),
        signature: vec![vec![0u8; 65]],
        ..Default::default()
    };
    let client = reqwest::Client::new();
    let _: serde_json::Value = client
        .post(format!("http://{bound}/wallet/broadcasthex"))
        .json(&serde_json::json!({ "transaction": hex::encode(tx.encode_to_vec()) }))
        .send().await.unwrap()
        .json().await.unwrap();

    // Rejected by the admission pipeline (no contract) -> not pooled.
    assert_eq!(mempool.lock().unwrap().len(), 0);
}
