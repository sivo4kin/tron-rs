//! Async node-discovery service: a Kademlia PING/PONG/FIND_NODE/NEIGHBORS loop
//! over UDP that self-populates the routing table and the shared [`PeerManager`].
//!
//! Reference: java-tron `org.tron.common.overlay.discover` (`DiscoverServer` /
//! `NodeManager`). Flow:
//! 1. **Bootstrap** — PING every seed endpoint.
//! 2. **PING → PONG** — a receiver adds the sender and replies PONG.
//! 3. **PONG → FIND_NODE** — on a PONG we ask that node for peers near *our* id.
//! 4. **FIND_NODE → NEIGHBORS** — a receiver replies with its closest known nodes.
//! 5. **NEIGHBORS** — each learned node is added and (if new) PINGed, expanding the
//!    table. A periodic refresh re-issues FIND_NODE(self) to the closest peers.
//!
//! Discovered nodes are mirrored into a shared [`PeerManager`] so the channel/sync
//! services (and the `listnodes` RPC) see real peers instead of only static seeds.

use crate::discovery::{Endpoint, InsertOutcome, Node, NodeId, RoutingTable, K_BUCKET_SIZE};
use crate::discovery_codec::DiscoveryMessage;
use crate::peer::PeerManager;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

/// Shared, mutable peer set (mirrored discovery output; read by the RPC layer).
pub type SharedPeers = Arc<Mutex<PeerManager>>;
/// Shared routing table (readable snapshot of the Kademlia state).
pub type SharedTable = Arc<Mutex<RoutingTable>>;

/// Max discovery datagram we will read (NEIGHBORS with a full bucket ≈ 700 bytes).
const RECV_BUF: usize = 2048;

/// How often to re-issue FIND_NODE(self) to refresh the table.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// A running discovery endpoint bound to a UDP socket.
pub struct Discovery {
    local: Node,
    socket: Arc<UdpSocket>,
    table: SharedTable,
    peers: SharedPeers,
}

impl Discovery {
    /// Bind a UDP socket and derive our advertised [`Endpoint`] from the bound
    /// address (so `bind` to `:0` yields a concrete ephemeral port, handy in tests).
    pub async fn bind(bind: SocketAddr, id: NodeId, peers: SharedPeers) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind).await?;
        let local_addr = socket.local_addr()?;
        let ip = match local_addr.ip() {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(_) => Ipv4Addr::LOCALHOST,
        };
        let endpoint = Endpoint::new(ip, local_addr.port(), local_addr.port());
        Ok(Self {
            local: Node::new(id, endpoint),
            socket: Arc::new(socket),
            table: Arc::new(Mutex::new(RoutingTable::new(id))),
            peers,
        })
    }

    pub fn local(&self) -> &Node {
        &self.local
    }

    pub fn table(&self) -> SharedTable {
        self.table.clone()
    }

    pub fn peers(&self) -> SharedPeers {
        self.peers.clone()
    }

    /// Run the discovery loop until `token` is cancelled. Bootstraps from `seeds`,
    /// answers inbound queries, and periodically refreshes the table.
    pub async fn run(self, seeds: Vec<Endpoint>, token: CancellationToken) {
        for seed in &seeds {
            self.send(&DiscoveryMessage::Ping { from: self.local.clone() }, seed).await;
        }

        let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = refresh.tick() => self.refresh().await,
                res = self.socket.recv_from(&mut buf) => {
                    if let Ok((n, _src)) = res {
                        if let Ok(msg) = DiscoveryMessage::decode(&buf[..n]) {
                            self.handle(msg).await;
                        }
                    }
                }
            }
        }
    }

    /// Dispatch one decoded inbound message.
    async fn handle(&self, msg: DiscoveryMessage) {
        match msg {
            DiscoveryMessage::Ping { from } => {
                self.add(&from);
                self.send(&DiscoveryMessage::Pong { from: self.local.clone() }, &from.endpoint)
                    .await;
            }
            DiscoveryMessage::Pong { from } => {
                self.add(&from);
                let find = DiscoveryMessage::FindNode { from: self.local.clone(), target: self.local.id };
                self.send(&find, &from.endpoint).await;
            }
            DiscoveryMessage::FindNode { from, target } => {
                self.add(&from);
                let nodes = self.table.lock().unwrap().closest(&target, K_BUCKET_SIZE);
                self.send(&DiscoveryMessage::Neighbors { from: self.local.clone(), nodes }, &from.endpoint)
                    .await;
            }
            DiscoveryMessage::Neighbors { from, nodes } => {
                self.add(&from);
                for node in nodes {
                    if node.id != self.local.id && self.add(&node) {
                        // Newly learned: probe it so it, in turn, adds us.
                        self.send(&DiscoveryMessage::Ping { from: self.local.clone() }, &node.endpoint)
                            .await;
                    }
                }
            }
        }
    }

    /// Re-issue FIND_NODE(self) to the closest known nodes.
    async fn refresh(&self) {
        let targets: Vec<Endpoint> = {
            let table = self.table.lock().unwrap();
            table.closest(&self.local.id, 3).into_iter().map(|n| n.endpoint).collect()
        };
        for ep in targets {
            let find = DiscoveryMessage::FindNode { from: self.local.clone(), target: self.local.id };
            self.send(&find, &ep).await;
        }
    }

    /// Insert `node` into the routing table and mirror it into the peer set.
    /// Returns `true` if it was a brand-new table entry.
    fn add(&self, node: &Node) -> bool {
        if node.id == self.local.id {
            return false;
        }
        let outcome = self.table.lock().unwrap().insert(node.clone());
        self.peers.lock().unwrap().upsert(node.endpoint.peer_addr(), -1, now_millis());
        matches!(outcome, InsertOutcome::Added)
    }

    async fn send(&self, msg: &DiscoveryMessage, to: &Endpoint) {
        let bytes = msg.encode();
        let addr = SocketAddr::from((to.ip, to.udp_port));
        let _ = self.socket.send_to(&bytes, addr).await;
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// Poll `cond` every 20ms up to `timeout_ms`; returns whether it became true.
    async fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let steps = timeout_ms / 20;
        for _ in 0..steps {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn two_services_bootstrap_and_learn_each_other() {
        let peers_a: SharedPeers = Arc::new(Mutex::new(PeerManager::new()));
        let peers_b: SharedPeers = Arc::new(Mutex::new(PeerManager::new()));

        let a = Discovery::bind(local(), [0x11; 32], peers_a.clone()).await.unwrap();
        let b = Discovery::bind(local(), [0x22; 32], peers_b.clone()).await.unwrap();

        let b_ep = b.local().endpoint;
        let table_a = a.table();
        let table_b = b.table();

        let token = CancellationToken::new();
        // A knows B as a seed; B starts cold and learns A from the inbound PING.
        let ha = tokio::spawn(a.run(vec![b_ep], token.clone()));
        let hb = tokio::spawn(b.run(vec![], token.clone()));

        let learned = wait_until(2000, || {
            table_a.lock().unwrap().len() >= 1
                && table_b.lock().unwrap().len() >= 1
                && !peers_a.lock().unwrap().is_empty()
                && !peers_b.lock().unwrap().is_empty()
        })
        .await;

        assert!(learned, "the two discovery services did not learn each other in time");

        // A learned B's id; B learned A's id.
        assert!(table_a.lock().unwrap().contains(&[0x22; 32]));
        assert!(table_b.lock().unwrap().contains(&[0x11; 32]));

        token.cancel();
        let _ = ha.await;
        let _ = hb.await;
    }

    #[tokio::test]
    async fn ping_elicits_pong_and_populates_receiver() {
        // A single service B; we act as a raw client that PINGs it and expects a PONG.
        let peers_b: SharedPeers = Arc::new(Mutex::new(PeerManager::new()));
        let b = Discovery::bind(local(), [0x22; 32], peers_b.clone()).await.unwrap();
        let b_ep = b.local().endpoint;
        let table_b = b.table();

        let token = CancellationToken::new();
        let hb = tokio::spawn(b.run(vec![], token.clone()));

        let client = UdpSocket::bind(local()).await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let client_node = Node::new(
            [0x33; 32],
            Endpoint::new(Ipv4Addr::LOCALHOST, client_addr.port(), client_addr.port()),
        );
        client
            .send_to(
                &DiscoveryMessage::Ping { from: client_node }.encode(),
                SocketAddr::from((b_ep.ip, b_ep.udp_port)),
            )
            .await
            .unwrap();

        // We should receive a PONG back from B.
        let mut buf = vec![0u8; RECV_BUF];
        let recv = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf)).await;
        let (n, _) = recv.expect("no PONG within timeout").unwrap();
        match DiscoveryMessage::decode(&buf[..n]).unwrap() {
            DiscoveryMessage::Pong { from } => assert_eq!(from.id, [0x22; 32]),
            other => panic!("expected PONG, got {other:?}"),
        }

        // B recorded us in its table and peer set.
        assert!(wait_until(1000, || table_b.lock().unwrap().contains(&[0x33; 32])).await);
        assert!(!peers_b.lock().unwrap().is_empty());

        token.cancel();
        let _ = hb.await;
    }
}
