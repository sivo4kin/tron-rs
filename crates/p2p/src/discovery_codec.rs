//! Discovery message codec: PING / PONG / FIND_NODE / NEIGHBORS.
//!
//! java-tron's discovery protocol (`org.tron.common.overlay.discover`) is the
//! reference. We implement the same four-message Kademlia set over a compact,
//! self-describing binary framing rather than java-tron's exact RLP/protobuf-over-
//! UDP encoding (documented deviation — the message *semantics* match; the byte
//! layout is our own and versioned by the leading type byte).
//!
//! Wire layout (all integers big-endian):
//! - byte 0: message type (`0x01` PING, `0x02` PONG, `0x03` FIND_NODE, `0x04` NEIGHBORS)
//! - a [`Node`] is `id(32) ‖ ipv4(4) ‖ udp_port(2) ‖ tcp_port(2)` = [`NODE_WIRE_LEN`]
//! - PING/PONG:   `[type] [from:Node]`
//! - FIND_NODE:   `[type] [from:Node] [target:32]`
//! - NEIGHBORS:   `[type] [from:Node] [count:2] [count × Node]`
//!
//! Every message carries the sender's own [`Node`] (`from`) so a receiver learns the
//! sender's id+endpoint directly — this is what lets a fresh node bootstrap.

use crate::discovery::{Endpoint, Node, NodeId, NODE_ID_LEN};
use std::net::Ipv4Addr;

const T_PING: u8 = 0x01;
const T_PONG: u8 = 0x02;
const T_FIND_NODE: u8 = 0x03;
const T_NEIGHBORS: u8 = 0x04;

/// Encoded size of one [`Node`]: id(32) + ipv4(4) + udp(2) + tcp(2).
pub const NODE_WIRE_LEN: usize = NODE_ID_LEN + 4 + 2 + 2;

/// Cap on nodes carried in a single NEIGHBORS message (DoS bound; matches the
/// k-bucket size served by the routing table).
pub const MAX_NEIGHBORS: usize = 16;

/// A decoded discovery message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryMessage {
    Ping { from: Node },
    Pong { from: Node },
    FindNode { from: Node, target: NodeId },
    Neighbors { from: Node, nodes: Vec<Node> },
}

/// Reasons a byte buffer is not a valid discovery message.
#[derive(Debug, PartialEq, Eq)]
pub enum CodecError {
    Empty,
    UnknownType(u8),
    /// The buffer ended before a declared field was complete.
    Truncated,
    /// A NEIGHBORS count exceeded [`MAX_NEIGHBORS`] (rejected before allocation).
    TooManyNodes(usize),
    /// Trailing bytes after a fully-parsed message.
    TrailingBytes,
}

fn put_node(buf: &mut Vec<u8>, node: &Node) {
    buf.extend_from_slice(&node.id);
    buf.extend_from_slice(&node.endpoint.ip.octets());
    buf.extend_from_slice(&node.endpoint.udp_port.to_be_bytes());
    buf.extend_from_slice(&node.endpoint.tcp_port.to_be_bytes());
}

/// Read one [`Node`] from the front of `bytes`, returning it and the remainder.
fn take_node(bytes: &[u8]) -> Result<(Node, &[u8]), CodecError> {
    if bytes.len() < NODE_WIRE_LEN {
        return Err(CodecError::Truncated);
    }
    let (node_bytes, rest) = bytes.split_at(NODE_WIRE_LEN);
    let mut id: NodeId = [0u8; NODE_ID_LEN];
    id.copy_from_slice(&node_bytes[..NODE_ID_LEN]);
    let ip = Ipv4Addr::new(
        node_bytes[NODE_ID_LEN],
        node_bytes[NODE_ID_LEN + 1],
        node_bytes[NODE_ID_LEN + 2],
        node_bytes[NODE_ID_LEN + 3],
    );
    let udp_port = u16::from_be_bytes([node_bytes[NODE_ID_LEN + 4], node_bytes[NODE_ID_LEN + 5]]);
    let tcp_port = u16::from_be_bytes([node_bytes[NODE_ID_LEN + 6], node_bytes[NODE_ID_LEN + 7]]);
    Ok((Node::new(id, Endpoint::new(ip, udp_port, tcp_port)), rest))
}

impl DiscoveryMessage {
    /// Serialize to the wire format described in the module docs.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            DiscoveryMessage::Ping { from } => {
                buf.push(T_PING);
                put_node(&mut buf, from);
            }
            DiscoveryMessage::Pong { from } => {
                buf.push(T_PONG);
                put_node(&mut buf, from);
            }
            DiscoveryMessage::FindNode { from, target } => {
                buf.push(T_FIND_NODE);
                put_node(&mut buf, from);
                buf.extend_from_slice(target);
            }
            DiscoveryMessage::Neighbors { from, nodes } => {
                buf.push(T_NEIGHBORS);
                put_node(&mut buf, from);
                let count = nodes.len().min(MAX_NEIGHBORS);
                buf.extend_from_slice(&(count as u16).to_be_bytes());
                for node in nodes.iter().take(count) {
                    put_node(&mut buf, node);
                }
            }
        }
        buf
    }

    /// Parse a datagram. Rejects unknown types, truncated fields, oversized
    /// neighbor lists (before allocating), and trailing garbage.
    pub fn decode(bytes: &[u8]) -> Result<DiscoveryMessage, CodecError> {
        let (&kind, rest) = bytes.split_first().ok_or(CodecError::Empty)?;
        match kind {
            T_PING | T_PONG => {
                let (from, rest) = take_node(rest)?;
                if !rest.is_empty() {
                    return Err(CodecError::TrailingBytes);
                }
                Ok(if kind == T_PING {
                    DiscoveryMessage::Ping { from }
                } else {
                    DiscoveryMessage::Pong { from }
                })
            }
            T_FIND_NODE => {
                let (from, rest) = take_node(rest)?;
                if rest.len() < NODE_ID_LEN {
                    return Err(CodecError::Truncated);
                }
                let (target_bytes, rest) = rest.split_at(NODE_ID_LEN);
                if !rest.is_empty() {
                    return Err(CodecError::TrailingBytes);
                }
                let mut target: NodeId = [0u8; NODE_ID_LEN];
                target.copy_from_slice(target_bytes);
                Ok(DiscoveryMessage::FindNode { from, target })
            }
            T_NEIGHBORS => {
                let (from, rest) = take_node(rest)?;
                if rest.len() < 2 {
                    return Err(CodecError::Truncated);
                }
                let (count_bytes, mut rest) = rest.split_at(2);
                let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
                if count > MAX_NEIGHBORS {
                    return Err(CodecError::TooManyNodes(count));
                }
                let mut nodes = Vec::with_capacity(count);
                for _ in 0..count {
                    let (node, tail) = take_node(rest)?;
                    nodes.push(node);
                    rest = tail;
                }
                if !rest.is_empty() {
                    return Err(CodecError::TrailingBytes);
                }
                Ok(DiscoveryMessage::Neighbors { from, nodes })
            }
            other => Err(CodecError::UnknownType(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id_byte: u8, port: u16) -> Node {
        Node::new(
            [id_byte; NODE_ID_LEN],
            Endpoint::new(Ipv4Addr::new(127, 0, 0, 1), port, port + 1),
        )
    }

    fn roundtrip(msg: &DiscoveryMessage) {
        let bytes = msg.encode();
        assert_eq!(&DiscoveryMessage::decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn node_wire_len_is_forty() {
        assert_eq!(NODE_WIRE_LEN, 40);
    }

    #[test]
    fn ping_pong_roundtrip() {
        roundtrip(&DiscoveryMessage::Ping { from: node(0x11, 30301) });
        roundtrip(&DiscoveryMessage::Pong { from: node(0x22, 30302) });
    }

    #[test]
    fn find_node_roundtrip_preserves_ports_and_ip() {
        let msg = DiscoveryMessage::FindNode { from: node(0x33, 18888), target: [0x99; NODE_ID_LEN] };
        let bytes = msg.encode();
        let decoded = DiscoveryMessage::decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
        if let DiscoveryMessage::FindNode { from, target } = decoded {
            assert_eq!(from.endpoint.ip, Ipv4Addr::new(127, 0, 0, 1));
            assert_eq!(from.endpoint.udp_port, 18888);
            assert_eq!(from.endpoint.tcp_port, 18889);
            assert_eq!(target, [0x99; NODE_ID_LEN]);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn neighbors_roundtrip_multiple_nodes() {
        let nodes = vec![node(0x01, 1), node(0x02, 2), node(0x03, 3)];
        roundtrip(&DiscoveryMessage::Neighbors { from: node(0xaa, 100), nodes });
    }

    #[test]
    fn neighbors_empty_list_roundtrip() {
        roundtrip(&DiscoveryMessage::Neighbors { from: node(0xaa, 100), nodes: vec![] });
    }

    #[test]
    fn empty_and_unknown_type_rejected() {
        assert_eq!(DiscoveryMessage::decode(&[]), Err(CodecError::Empty));
        assert_eq!(DiscoveryMessage::decode(&[0x7f, 0, 0]), Err(CodecError::UnknownType(0x7f)));
    }

    #[test]
    fn truncated_node_rejected() {
        // PING with a short (non-40-byte) node body.
        let mut buf = vec![T_PING];
        buf.extend_from_slice(&[0u8; 10]);
        assert_eq!(DiscoveryMessage::decode(&buf), Err(CodecError::Truncated));
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mut bytes = DiscoveryMessage::Ping { from: node(0x11, 1) }.encode();
        bytes.push(0xff);
        assert_eq!(DiscoveryMessage::decode(&bytes), Err(CodecError::TrailingBytes));
    }

    #[test]
    fn oversized_neighbor_count_rejected_before_alloc() {
        // Hand-build a NEIGHBORS header claiming more than MAX_NEIGHBORS nodes.
        let mut buf = vec![T_NEIGHBORS];
        put_node(&mut buf, &node(0xaa, 1));
        buf.extend_from_slice(&((MAX_NEIGHBORS as u16) + 1).to_be_bytes());
        assert_eq!(
            DiscoveryMessage::decode(&buf),
            Err(CodecError::TooManyNodes(MAX_NEIGHBORS + 1))
        );
    }

    #[test]
    fn neighbors_count_exceeding_body_is_truncated() {
        // Claim 3 nodes but supply none.
        let mut buf = vec![T_NEIGHBORS];
        put_node(&mut buf, &node(0xaa, 1));
        buf.extend_from_slice(&3u16.to_be_bytes());
        assert_eq!(DiscoveryMessage::decode(&buf), Err(CodecError::Truncated));
    }
}
