//! Node wiring: configuration + a service supervisor with graceful shutdown.
//!
//! P0 stands up the supervision skeleton — services start, run until a shutdown
//! signal, and stop cleanly. Later phases replace the placeholder services with the
//! real p2p, consensus, and rpc subsystems.

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// Node configuration (loaded from TOML or defaults).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Network name, e.g. "nile" or "shasta".
    pub network: String,
    /// Data directory for the state/chain databases.
    pub data_dir: String,
    pub p2p_port: u16,
    pub http_port: u16,
    pub grpc_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            network: "nile".to_string(),
            data_dir: "./data".to_string(),
            p2p_port: tron_p2p::DEFAULT_P2P_PORT,
            http_port: tron_rpc::DEFAULT_HTTP_PORT,
            grpc_port: tron_rpc::DEFAULT_GRPC_PORT,
        }
    }
}

impl Config {
    /// Load from a TOML file, falling back to defaults if the path is `None`.
    pub fn load(path: Option<&str>) -> anyhow::Result<Self> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)?;
                Ok(toml::from_str(&text)?)
            }
            None => Ok(Config::default()),
        }
    }
}

/// The node: owns config and supervises services.
pub struct Node {
    config: Config,
}

impl Node {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Start all services and run until `shutdown` is cancelled, then stop cleanly.
    ///
    /// Returns once every service has drained. In P0 the services are placeholders
    /// that simply wait for the shutdown signal.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        tracing::info!(
            network = %self.config.network,
            data_dir = %self.config.data_dir,
            "tron-rs node starting"
        );

        // Placeholder service set — replaced by real subsystems in later phases.
        let services = ["p2p", "consensus", "rpc"];
        let mut handles = Vec::with_capacity(services.len());
        for name in services {
            let token = shutdown.clone();
            let name = name.to_string();
            handles.push(tokio::spawn(async move {
                tracing::info!(service = %name, "service started");
                token.cancelled().await;
                tracing::info!(service = %name, "service stopped");
            }));
        }

        // Wait for the shutdown signal, then drain all services.
        shutdown.cancelled().await;
        tracing::info!("shutdown signal received; draining services");
        for h in handles {
            let _ = h.await;
        }
        tracing::info!("tron-rs node stopped cleanly");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let c = Config::default();
        assert_eq!(c.network, "nile");
        assert_eq!(c.p2p_port, 18888);
        assert_eq!(c.http_port, 8090);
    }
}
