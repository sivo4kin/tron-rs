//! Node wiring: configuration + a service supervisor with graceful shutdown.
//!
//! P0 stands up the supervision skeleton — services start, run until a shutdown
//! signal, and stop cleanly. Later phases replace the placeholder services with the
//! real p2p, consensus, and rpc subsystems.

pub mod sync;

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
    /// Seed peers to bootstrap sync from, as `"host:port"` strings.
    #[serde(default)]
    pub seed_nodes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            network: "nile".to_string(),
            data_dir: "./data".to_string(),
            p2p_port: tron_p2p::DEFAULT_P2P_PORT,
            http_port: tron_rpc::DEFAULT_HTTP_PORT,
            grpc_port: tron_rpc::DEFAULT_GRPC_PORT,
            seed_nodes: Vec::new(),
        }
    }
}

impl Config {
    /// Build a [`PeerManager`](tron_p2p::peer::PeerManager) seeded from
    /// `seed_nodes` (head unknown = -1 until the peer advertises one).
    pub fn seeded_peers(&self) -> tron_p2p::peer::PeerManager {
        let mut pm = tron_p2p::peer::PeerManager::new();
        for (i, node) in self.seed_nodes.iter().enumerate() {
            if let Some((host, port)) = node.rsplit_once(':') {
                if let Ok(port) = port.parse::<u16>() {
                    pm.upsert(tron_p2p::PeerAddr::new(host, port), -1, i as u64);
                }
            }
        }
        pm
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

    /// Open the world state backing store per build features:
    /// with `rocksdb`, a persistent [`tron_storage::RocksStore`] under
    /// `data_dir/state`; otherwise an in-memory store (tests / early phases).
    #[cfg(feature = "rocksdb")]
    pub fn open_state(
        &self,
    ) -> anyhow::Result<tron_state::WorldState<tron_storage::RocksStore>> {
        let path = std::path::Path::new(&self.config.data_dir).join("state");
        let db = tron_storage::RocksStore::open(&path)
            .map_err(|e| anyhow::anyhow!("open state db at {}: {e}", path.display()))?;
        tracing::info!(path = %path.display(), "state db opened (rocksdb)");
        Ok(tron_state::WorldState::new(db))
    }

    #[cfg(not(feature = "rocksdb"))]
    pub fn open_state(
        &self,
    ) -> anyhow::Result<tron_state::WorldState<tron_storage::MemoryStore>> {
        tracing::info!("state db opened (in-memory; build with --features rocksdb to persist)");
        Ok(tron_state::WorldState::new(tron_storage::MemoryStore::new()))
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
        let state = std::sync::Arc::new(self.open_state()?);

        let mut handles = Vec::new();

        // Real HTTP API service: serve the tron-openapi handlers on http_port until
        // shutdown (graceful — the axum server is aborted when the token fires).
        {
            let http_addr: std::net::SocketAddr =
                ([0, 0, 0, 0], self.config.http_port).into();
            let router = tron_rpc::server::router(state.clone());
            let token = shutdown.clone();
            handles.push(tokio::spawn(async move {
                match tokio::net::TcpListener::bind(http_addr).await {
                    Ok(listener) => {
                        tracing::info!(addr = %http_addr, "http api listening");
                        let serve = axum::serve(listener, router)
                            .with_graceful_shutdown(async move { token.cancelled().await });
                        if let Err(e) = serve.await {
                            tracing::warn!(error = %e, "http api error");
                        }
                    }
                    Err(e) => tracing::warn!(addr = %http_addr, error = %e, "http bind failed"),
                }
                tracing::info!(service = "rpc", "service stopped");
            }));
        }

        // Sync service: periodically sync from the best seeded peer into the shared
        // state (a no-op when no peers are configured/ahead). Runs on the same
        // Arc<WorldState> the HTTP server serves — enabled by WorldState's &self API.
        {
            let sync_state = state.clone();
            let peers = self.config.seeded_peers();
            let token = shutdown.clone();
            handles.push(tokio::spawn(async move {
                tracing::info!(service = "sync", "service started");
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        _ = tick.tick() => {
                            match crate::sync::sync_from_best_peer(&sync_state, &peers, false).await {
                                Ok(n) if n > 0 => tracing::info!(applied = n, "synced blocks"),
                                Ok(_) => {}
                                Err(e) => tracing::debug!(?e, "sync round failed"),
                            }
                        }
                    }
                }
                tracing::info!(service = "sync", "service stopped");
            }));
        }

        // Placeholder service for the remaining subsystem (consensus/producer).
        {
            let token = shutdown.clone();
            handles.push(tokio::spawn(async move {
                tracing::info!(service = "consensus", "service started");
                token.cancelled().await;
                tracing::info!(service = "consensus", "service stopped");
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
        assert!(c.seed_nodes.is_empty());
    }

    #[test]
    fn seeded_peers_from_config() {
        let mut c = Config::default();
        c.seed_nodes = vec!["1.2.3.4:18888".into(), "5.6.7.8:18889".into(), "bad".into()];
        let pm = c.seeded_peers();
        assert_eq!(pm.len(), 2); // the malformed entry is skipped
        // no peer has an advertised head yet (-1), so none is a sync target
        assert!(pm.best_sync_target(-1).is_none());
    }
}
