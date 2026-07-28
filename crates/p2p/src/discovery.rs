//! Node discovery distance metric (Kademlia, java-tron/opentron `KademliaOptions`).
//!
//! Peers are identified by a 64-byte node id; distance is the number of leading
//! equal bits (the higher the shared prefix, the closer). Buckets are indexed by
//! `256 - common_prefix_bits(sha3(a), sha3(b))`-style binning; here we expose the
//! primitives (common-prefix bits and a comparator) that a routing table uses.

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
    candidates.sort_by(|a, b| {
        common_prefix_bits(target, b).cmp(&common_prefix_bits(target, a))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
