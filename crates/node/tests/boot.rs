//! P0 integration gate: the node boots and shuts down cleanly on a shutdown signal.

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tron_node::{Config, Node};

#[tokio::test]
async fn boots_and_shuts_down_cleanly() {
    let shutdown = CancellationToken::new();
    // Unique temp data_dir so the rocksdb-feature build does not write into the repo.
    let mut config = Config::default();
    config.data_dir = std::env::temp_dir()
        .join(format!("tron-rs-boot-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let node = Node::new(config);

    let run_token = shutdown.clone();
    let handle = tokio::spawn(async move { node.run(run_token).await });

    // Let services start, then signal shutdown.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown.cancel();

    // The node must drain and return promptly.
    let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(joined.is_ok(), "node did not shut down within 5s");
    assert!(joined.unwrap().unwrap().is_ok(), "node.run returned an error");
}
