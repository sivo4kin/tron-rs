//! Node wiring: configuration + a service supervisor with graceful shutdown.
//!
//! P0 stands up the supervision skeleton — services start, run until a shutdown
//! signal, and stop cleanly. Later phases replace the placeholder services with the
//! real p2p, consensus, and rpc subsystems.

pub mod net;
pub mod production;
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
    /// Produce blocks when this node is the scheduled witness (needs `witness_key`).
    #[serde(default)]
    pub witness: bool,
    /// Hex-encoded 32-byte witness (SR) private key used to sign produced blocks.
    #[serde(default)]
    pub witness_key: Option<String>,
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
            witness: false,
            witness_key: None,
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
    /// Parse the configured witness private key (hex, 32 bytes), if any.
    pub fn witness_secret_key(&self) -> Option<tron_crypto::SecretKey> {
        let hex = self.witness_key.as_ref()?;
        let bytes = hex::decode(hex.trim_start_matches("0x")).ok()?;
        tron_crypto::SecretKey::from_slice(&bytes).ok()
    }

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

        // Shared peer table: seeded from config, then self-populated by the discovery
        // service. The RPC `listnodes` handler reads it and the discovery service
        // writes to it, so discovered peers surface over HTTP.
        let peers = std::sync::Arc::new(std::sync::Mutex::new(self.config.seeded_peers()));

        // Shared mempool: broadcasthex (rpc) and inbound tx gossip (channel service)
        // admit into the same pool.
        let mempool =
            std::sync::Arc::new(std::sync::Mutex::new(tron_consensus::mempool::Mempool::default()));

        let mut handles = Vec::new();

        // Real HTTP API service: serve the tron-openapi handlers on http_port until
        // shutdown (graceful — the axum server is aborted when the token fires).
        {
            let http_addr: std::net::SocketAddr =
                ([0, 0, 0, 0], self.config.http_port).into();
            let node_state = tron_rpc::server::NodeState::new(state.clone())
                .with_peers(peers.clone())
                .with_mempool(mempool.clone());
            let router = tron_rpc::server::router_with_state(node_state);
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

        // Discovery service: bind the UDP discovery port and run the Kademlia
        // PING/PONG/FIND_NODE loop, populating the shared peer table from the seeds.
        // A bind failure is non-fatal (logged) so the rest of the node still runs.
        {
            let disc_addr: std::net::SocketAddr = ([0, 0, 0, 0], self.config.p2p_port).into();
            let node_id = ephemeral_node_id(self.config.p2p_port);
            let seeds = discovery_seeds(&self.config.seed_nodes);
            let disc_peers = peers.clone();
            let token = shutdown.clone();
            handles.push(tokio::spawn(async move {
                match tron_p2p::discovery_service::Discovery::bind(disc_addr, node_id, disc_peers)
                    .await
                {
                    Ok(discovery) => {
                        tracing::info!(addr = %disc_addr, seeds = seeds.len(), "discovery listening");
                        discovery.run(seeds, token).await;
                    }
                    Err(e) => tracing::warn!(addr = %disc_addr, error = %e, "discovery bind failed"),
                }
                tracing::info!(service = "discovery", "service stopped");
            }));
        }

        // Channel service: bind the TCP p2p port, keep persistent peer connections,
        // gossip block/tx inventory, and apply inbound blocks through the intake gate.
        // Dials the currently-known peers; a bind failure is non-fatal.
        //
        // Its advertise handle is captured so the block-production service can gossip
        // locally produced blocks (T07).
        let channel_advertise;
        {
            let chan_addr: std::net::SocketAddr = ([0, 0, 0, 0], self.config.p2p_port).into();
            let handler = std::sync::Arc::new(crate::net::NodeChannelHandler::new(
                state.clone(),
                mempool.clone(),
                true,
            ));
            let (service, advertise) =
                tron_p2p::service::ChannelService::new(handler, Default::default());
            channel_advertise = advertise;
            let dial: Vec<std::net::SocketAddr> = peers
                .lock()
                .map(|pm| {
                    pm.addr_list()
                        .iter()
                        .filter_map(|a| format!("{}:{}", a.host, a.port).parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            let token = shutdown.clone();
            handles.push(tokio::spawn(async move {
                match tokio::net::TcpListener::bind(chan_addr).await {
                    Ok(listener) => {
                        tracing::info!(addr = %chan_addr, dial = dial.len(), "channel listening");
                        service.run(listener, dial, token).await;
                    }
                    Err(e) => tracing::warn!(addr = %chan_addr, error = %e, "channel bind failed"),
                }
                tracing::info!(service = "channel", "service stopped");
            }));
        }

        // Block-production service (T07): when configured as a witness, on each block
        // interval produce a block for our scheduled slot from the mempool, apply it
        // locally (advancing the head), and gossip it via the channel service.
        {
            let token = shutdown.clone();
            let prod_state = state.clone();
            let prod_mempool = mempool.clone();
            let advertise = channel_advertise.clone();
            let witness_key = self.config.witness_secret_key().filter(|_| self.config.witness);
            handles.push(tokio::spawn(async move {
                tracing::info!(service = "consensus", producing = witness_key.is_some(), "service started");
                match witness_key {
                    Some(key) => {
                        let mut tick = tokio::time::interval(std::time::Duration::from_millis(
                            tron_consensus::BLOCK_INTERVAL_MS,
                        ));
                        loop {
                            tokio::select! {
                                _ = token.cancelled() => break,
                                _ = tick.tick() => {
                                    let now = crate::production::now_ms();
                                    if let Some(block) =
                                        crate::production::try_produce(&prod_state, &prod_mempool, &key, now)
                                    {
                                        if let Some(n) = crate::production::apply_produced(
                                            &prod_state, &prod_mempool, &block,
                                        ) {
                                            tracing::info!(number = n, "produced block");
                                            advertise.advertise_block(n);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Not a witness: nothing to produce.
                    None => token.cancelled().await,
                }
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

/// Parse `"host:port"` seed strings into discovery [`Endpoint`](tron_p2p::discovery::Endpoint)s.
/// Only IPv4 literals are accepted (the discovery wire format is IPv4-only);
/// hostnames and malformed entries are skipped.
fn discovery_seeds(seed_nodes: &[String]) -> Vec<tron_p2p::discovery::Endpoint> {
    let mut out = Vec::new();
    for node in seed_nodes {
        let Some((host, port)) = node.rsplit_once(':') else { continue };
        let (Ok(ip), Ok(port)) = (host.parse::<std::net::Ipv4Addr>(), port.parse::<u16>()) else {
            continue;
        };
        out.push(tron_p2p::discovery::Endpoint::new(ip, port, port));
    }
    out
}

/// Derive an ephemeral 32-byte discovery node id from a seed (splitmix64 spread over
/// 32 bytes). Deviation: real Tron ids are the node's secp256k1 public key; a random
/// per-process id suffices for the Kademlia distance metric until node keys are wired.
fn ephemeral_node_id(port: u16) -> [u8; 32] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ ((port as u64) << 48) ^ 0x9e37_79b9_7f4a_7c15;
    let mut id = [0u8; 32];
    for chunk in id.chunks_mut(8) {
        // splitmix64
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_be_bytes());
    }
    id
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
    fn discovery_seeds_parses_ipv4_and_skips_the_rest() {
        let seeds = discovery_seeds(&[
            "1.2.3.4:18888".into(),
            "example.com:18888".into(), // hostname -> skipped
            "9.9.9.9".into(),           // no port -> skipped
            "5.6.7.8:70000".into(),     // bad port -> skipped
            "10.0.0.1:18889".into(),
        ]);
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].ip, std::net::Ipv4Addr::new(1, 2, 3, 4));
        assert_eq!(seeds[0].udp_port, 18888);
        assert_eq!(seeds[1].ip, std::net::Ipv4Addr::new(10, 0, 0, 1));
    }

    #[test]
    fn ephemeral_node_id_is_nonzero_and_varies() {
        let a = ephemeral_node_id(18888);
        assert_ne!(a, [0u8; 32]);
        // Distinct ports (folded into the seed) should not collide deterministically.
        let b = ephemeral_node_id(18889);
        assert_ne!(a, b);
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
