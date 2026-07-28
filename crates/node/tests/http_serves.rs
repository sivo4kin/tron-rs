//! The running node serves the HTTP API on its configured port.

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tron_node::{Config, Node};

#[tokio::test]
async fn node_serves_http_and_shuts_down() {
    // Bind an ephemeral-ish high port to avoid clashes.
    let port = 28090;
    let mut config = Config::default();
    config.http_port = port;
    config.data_dir = std::env::temp_dir()
        .join(format!("tron-rs-http-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();

    let shutdown = CancellationToken::new();
    let run_token = shutdown.clone();
    let handle = tokio::spawn(async move { Node::new(config).run(run_token).await });

    // Give the server a moment to bind, then hit validateaddress (no state needed).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/wallet/validateaddress"))
        .json(&serde_json::json!({ "address": "garbage" }))
        .send()
        .await;

    // The node answered over HTTP.
    if let Ok(r) = resp {
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["result"], false);
    } else {
        panic!("node did not serve HTTP on {port}");
    }

    // Clean shutdown.
    shutdown.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(joined.is_ok(), "node did not shut down");
    assert!(joined.unwrap().unwrap().is_ok());
}
