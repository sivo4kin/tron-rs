//! Peer-to-peer networking (P3): node discovery + the channel/sync protocol.
//!
//! Tron uses a custom wire protocol (not libp2p): Kademlia-style discovery over UDP
//! and a TCP channel for block/transaction sync — both on port 18888. opentron's
//! `services/discovery` and `services/channel` are the protocol reference; the
//! fork-choice/finality integration they lacked is first-class here.

/// Default Tron p2p listen port (UDP discovery + TCP channel).
pub const DEFAULT_P2P_PORT: u16 = 18888;

/// A discovered peer endpoint (placeholder; P3 fills the discovery table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    pub host: String,
    pub port: u16,
}

impl PeerAddr {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_and_peer() {
        assert_eq!(DEFAULT_P2P_PORT, 18888);
        let p = PeerAddr::new("127.0.0.1", DEFAULT_P2P_PORT);
        assert_eq!(p.port, 18888);
    }
}
