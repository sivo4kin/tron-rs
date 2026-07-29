//! Node-discovery Kademlia primitives: distance metric, node model, routing table.
//!
//! Peers are identified by a fixed-length node id; distance is the number of leading
//! equal bits (the higher the shared prefix, the closer — java-tron/opentron
//! `KademliaOptions`). This module owns the pure, synchronous pieces the async
//! discovery service ([`crate::discovery_service`]) drives: the closeness metric, the
//! [`Node`]/[`Endpoint`] model, and a k-bucket [`RoutingTable`].

use crate::PeerAddr;
use std::net::Ipv4Addr;

/// Node-id length in bytes (deviation: Tron uses a 64-byte secp256k1 pubkey id; we
/// use a 32-byte id — the metric is identical on any byte width and ids are treated
/// as uniformly random, so we compare them directly rather than hashing first).
pub const NODE_ID_LEN: usize = 32;

/// Node-id bit-width (the Kademlia id space).
pub const ID_BITS: usize = NODE_ID_LEN * 8;

/// Nodes per k-bucket (java-tron `KademliaOptions.BUCKET_SIZE`).
pub const K_BUCKET_SIZE: usize = 16;

/// A discovery node id.
pub type NodeId = [u8; NODE_ID_LEN];

/// Count the number of leading equal bits between two byte slices (shorter length
/// wins). This is the Kademlia "closeness" — more shared prefix bits = closer.
pub fn common_prefix_bits(a: &[u8], b: &[u8]) -> u32 {
    let mut bits = 0u32;
    for (x, y) in a.iter().zip(b.iter()) {
        if x == y {
            bits += 8;
        } else {
            bits += (x ^ y).leading_zeros();
            break;
        }
    }
    bits
}

/// XOR distance ordering: is `a` strictly closer to `target` than `b`?
/// (More shared prefix bits ⇒ closer.)
pub fn is_closer(target: &[u8], a: &[u8], b: &[u8]) -> bool {
    common_prefix_bits(target, a) > common_prefix_bits(target, b)
}

/// Sort candidate node ids by closeness to `target` (closest first).
pub fn sort_by_distance(target: &[u8], mut candidates: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    candidates.sort_by(|a, b| common_prefix_bits(target, b).cmp(&common_prefix_bits(target, a)));
    candidates
}

/// A UDP/TCP endpoint. Discovery runs over UDP; the same port carries the TCP
/// channel in Tron. (Deviation: only IPv4 endpoints are modeled on the wire.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub ip: Ipv4Addr,
    pub udp_port: u16,
    pub tcp_port: u16,
}

impl Endpoint {
    pub fn new(ip: Ipv4Addr, udp_port: u16, tcp_port: u16) -> Self {
        Self { ip, udp_port, tcp_port }
    }

    /// The channel (TCP) address other subsystems dial, as a [`PeerAddr`].
    pub fn peer_addr(&self) -> PeerAddr {
        PeerAddr::new(self.ip.to_string(), self.tcp_port)
    }
}

/// A discovery node: its id and where to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub endpoint: Endpoint,
}

impl Node {
    pub fn new(id: NodeId, endpoint: Endpoint) -> Self {
        Self { id, endpoint }
    }
}

/// The result of inserting a node into a [`RoutingTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// A new node occupied free space in its bucket.
    Added,
    /// A node already present had its endpoint refreshed (moved to most-recent).
    Refreshed,
    /// The bucket was full; the least-recently-seen node was evicted for the newcomer.
    EvictedOldest(NodeId),
    /// The node is our own id — never stored.
    Ignored,
}

/// A Kademlia routing table: `ID_BITS` k-buckets keyed by distance to `local`.
///
/// Bucket `i` holds nodes whose highest differing bit with `local` is at position
/// `i` (i.e. `ID_BITS - 1 - common_prefix_bits`). Within a bucket the vector is
/// ordered least-recently-seen first; a full bucket evicts its front (LRU — a
/// simplification of java-tron's ping-to-confirm eviction).
pub struct RoutingTable {
    local: NodeId,
    buckets: Vec<Vec<Node>>,
    k: usize,
}

impl RoutingTable {
    pub fn new(local: NodeId) -> Self {
        Self::with_k(local, K_BUCKET_SIZE)
    }

    /// Construct with an explicit bucket capacity (tests exercise eviction with a
    /// small `k`).
    pub fn with_k(local: NodeId, k: usize) -> Self {
        Self { local, buckets: (0..ID_BITS).map(|_| Vec::new()).collect(), k }
    }

    pub fn local_id(&self) -> &NodeId {
        &self.local
    }

    /// Bucket index for `id`, or `None` if `id` is our own id.
    fn bucket_index(&self, id: &NodeId) -> Option<usize> {
        let cpb = common_prefix_bits(&self.local, id) as usize;
        if cpb >= ID_BITS {
            None
        } else {
            Some(ID_BITS - 1 - cpb)
        }
    }

    /// Insert or refresh `node`. See [`InsertOutcome`].
    pub fn insert(&mut self, node: Node) -> InsertOutcome {
        let Some(idx) = self.bucket_index(&node.id) else {
            return InsertOutcome::Ignored;
        };
        let bucket = &mut self.buckets[idx];
        if let Some(pos) = bucket.iter().position(|n| n.id == node.id) {
            let mut existing = bucket.remove(pos);
            existing.endpoint = node.endpoint;
            bucket.push(existing);
            InsertOutcome::Refreshed
        } else if bucket.len() < self.k {
            bucket.push(node);
            InsertOutcome::Added
        } else {
            let evicted = bucket.remove(0);
            bucket.push(node);
            InsertOutcome::EvictedOldest(evicted.id)
        }
    }

    /// The `count` nodes closest to `target` (closest first).
    pub fn closest(&self, target: &NodeId, count: usize) -> Vec<Node> {
        let mut all: Vec<Node> = self.buckets.iter().flatten().cloned().collect();
        all.sort_by(|a, b| {
            common_prefix_bits(target, &b.id).cmp(&common_prefix_bits(target, &a.id))
        });
        all.truncate(count);
        all
    }

    /// Every node currently held.
    pub fn all_nodes(&self) -> Vec<Node> {
        self.buckets.iter().flatten().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True if `id` is already in the table.
    pub fn contains(&self, id: &NodeId) -> bool {
        match self.bucket_index(id) {
            Some(idx) => self.buckets[idx].iter().any(|n| &n.id == id),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nid(b: u8) -> NodeId {
        [b; NODE_ID_LEN]
    }

    fn ep(port: u16) -> Endpoint {
        Endpoint::new(Ipv4Addr::LOCALHOST, port, port)
    }

    fn node(b: u8, port: u16) -> Node {
        Node::new(nid(b), ep(port))
    }

    #[test]
    fn prefix_bits_counts_leading_equal_bits() {
        assert_eq!(common_prefix_bits(&[0xff], &[0xff]), 8);
        assert_eq!(common_prefix_bits(&[0x00], &[0x00]), 8);
        // 0b1111_1111 vs 0b1111_0000 -> 4 leading equal bits
        assert_eq!(common_prefix_bits(&[0xf0], &[0xff]), 4);
        // differ in the first bit
        assert_eq!(common_prefix_bits(&[0x00], &[0x80]), 0);
        // multi-byte: full first byte equal, then diverge
        assert_eq!(common_prefix_bits(&[0xaa, 0xf0], &[0xaa, 0xff]), 12);
    }

    #[test]
    fn closeness_comparison() {
        let target = [0xffu8, 0xff];
        let near = [0xff, 0xf0]; // 12 bits
        let far = [0xf0, 0x00]; // 4 bits
        assert!(is_closer(&target, &near, &far));
        assert!(!is_closer(&target, &far, &near));
    }

    #[test]
    fn sort_orders_closest_first() {
        let target = vec![0xff, 0xff];
        let candidates = vec![
            vec![0x00, 0x00], // 0 bits
            vec![0xff, 0xf0], // 12 bits
            vec![0xf0, 0x00], // 4 bits
            vec![0xff, 0xff], // 16 bits (self)
        ];
        let sorted = sort_by_distance(&target, candidates);
        assert_eq!(sorted[0], vec![0xff, 0xff]);
        assert_eq!(sorted[1], vec![0xff, 0xf0]);
        assert_eq!(sorted[2], vec![0xf0, 0x00]);
        assert_eq!(sorted[3], vec![0x00, 0x00]);
    }

    #[test]
    fn endpoint_maps_to_peer_addr() {
        let e = Endpoint::new(Ipv4Addr::new(10, 0, 0, 7), 18888, 18888);
        assert_eq!(e.peer_addr(), PeerAddr::new("10.0.0.7", 18888));
    }

    #[test]
    fn insert_adds_and_refreshes_and_ignores_self() {
        let local = nid(0x00);
        let mut rt = RoutingTable::new(local);
        assert!(rt.is_empty());

        // A brand-new node is added.
        assert_eq!(rt.insert(node(0x80, 1)), InsertOutcome::Added);
        assert_eq!(rt.len(), 1);
        assert!(rt.contains(&nid(0x80)));

        // Same id again refreshes (endpoint updated), does not grow the table.
        assert_eq!(rt.insert(node(0x80, 2)), InsertOutcome::Refreshed);
        assert_eq!(rt.len(), 1);

        // Our own id is never stored.
        assert_eq!(rt.insert(node(0x00, 9)), InsertOutcome::Ignored);
        assert_eq!(rt.len(), 1);
    }

    #[test]
    fn full_bucket_evicts_least_recently_seen() {
        // All three ids have their first bit set -> common_prefix_bits(local, id) == 0
        // -> they land in the same (highest-index) bucket.
        let local = nid(0x00);
        let mut rt = RoutingTable::with_k(local, 2);
        let a = node(0x80, 1);
        let b = node(0xc0, 2);
        let c = node(0xa0, 3);

        assert_eq!(rt.insert(a.clone()), InsertOutcome::Added);
        assert_eq!(rt.insert(b.clone()), InsertOutcome::Added);
        // Bucket now full (k=2); inserting c evicts the oldest (a).
        assert_eq!(rt.insert(c.clone()), InsertOutcome::EvictedOldest(a.id));
        assert_eq!(rt.len(), 2);
        assert!(!rt.contains(&a.id));
        assert!(rt.contains(&b.id));
        assert!(rt.contains(&c.id));
    }

    #[test]
    fn refresh_moves_node_to_most_recent_and_saves_it_from_eviction() {
        let local = nid(0x00);
        let mut rt = RoutingTable::with_k(local, 2);
        let a = node(0x80, 1);
        let b = node(0xc0, 2);
        rt.insert(a.clone());
        rt.insert(b.clone());
        // Refresh a -> a becomes most-recent, b becomes oldest.
        assert_eq!(rt.insert(a.clone()), InsertOutcome::Refreshed);
        // Now inserting c evicts b (the oldest), not a.
        let c = node(0xa0, 3);
        assert_eq!(rt.insert(c.clone()), InsertOutcome::EvictedOldest(b.id));
        assert!(rt.contains(&a.id));
        assert!(!rt.contains(&b.id));
    }

    #[test]
    fn closest_returns_nodes_ordered_by_distance() {
        let local = nid(0x00);
        let mut rt = RoutingTable::new(local);
        // Distances to target 0x00..:
        //   0x01 -> shares all but the last bit of the first byte (cpb 7)  [closest]
        //   0x0f -> cpb 4
        //   0x80 -> cpb 0                                                  [farthest]
        rt.insert(node(0x01, 1));
        rt.insert(node(0x0f, 2));
        rt.insert(node(0x80, 3));

        let target = nid(0x00);
        let closest = rt.closest(&target, 2);
        assert_eq!(closest.len(), 2);
        assert_eq!(closest[0].id, nid(0x01));
        assert_eq!(closest[1].id, nid(0x0f));

        // Asking for more than we have returns all, still ordered.
        let all = rt.closest(&target, 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].id, nid(0x80));
    }
}
