//! tron-rs full node binary (P0 scaffold).
//!
//! Loads config, wires Ctrl-C to a shutdown token, and runs the service supervisor
//! until interrupted, then shuts down cleanly.

use tokio_util::sync::CancellationToken;
use tron_node::{Config, Node};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Optional config path as the first CLI argument.
    let config_path = std::env::args().nth(1);
    let config = Config::load(config_path.as_deref())?;

    let shutdown = CancellationToken::new();

    // Cancel the shutdown token on Ctrl-C.
    let sig_token = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received");
            sig_token.cancel();
        }
    });

    Node::new(config).run(shutdown).await
}
