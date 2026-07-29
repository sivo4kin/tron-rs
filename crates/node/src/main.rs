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

    // CLI args: an optional config path (first non-flag arg) and a `--witness` flag
    // that forces block production on (overriding the config value).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let witness_flag = args.iter().any(|a| a == "--witness");
    let config_path = args.iter().find(|a| !a.starts_with("--")).cloned();
    let mut config = Config::load(config_path.as_deref())?;
    if witness_flag {
        config.witness = true;
    }

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
