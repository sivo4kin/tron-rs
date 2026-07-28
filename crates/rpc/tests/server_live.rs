//! Live HTTP server test: bind a socket, POST via a real client, assert JSON.

use std::sync::Arc;
use tron_proto::protocol;
use tron_rpc::server::router;
use tron_state::WorldState;
use tron_storage::MemoryStore;
use tron_types::Address;

#[tokio::test]
async fn serves_getaccount_over_http() {
    let addr = Address::from_body([0x11; 20]);
    let mut ws = WorldState::new(MemoryStore::new());
    ws.put_account(
        &addr,
        &protocol::Account {
            address: addr.as_bytes().to_vec(),
            balance: 7_777_777,
            ..Default::default()
        },
    )
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let app = router(Arc::new(ws));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("http://{bound}/wallet/getaccount"))
        .json(&serde_json::json!({ "address": addr.to_hex() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["balance"], 7_777_777);
    assert_eq!(resp["address"], addr.to_hex());

    let unknown = Address::from_body([0x22; 20]);
    let resp: serde_json::Value = client
        .post(format!("http://{bound}/wallet/getaccount"))
        .json(&serde_json::json!({ "address": unknown.to_hex() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp, serde_json::json!({}));
}
