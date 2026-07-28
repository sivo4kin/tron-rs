//! Block / transaction model: ids, hashes, and the transaction merkle root.
//!
//! Byte-exact with java-tron (verified against its source and live-block fixtures):
//! - **tx id** = `sha256(raw_data.toByteArray())` (`TransactionCapsule.getRawHash`)
//! - **merkle leaf** = `sha256(transaction.toByteArray())` — the *full* tx including
//!   signatures (`TransactionCapsule.getMerkleHash`)
//! - **txTrieRoot** = binary SHA256 merkle over leaves: empty list → `ZERO_HASH`,
//!   an odd trailing node is carried up **unchanged**, parent = `SHA256(left‖right)`
//!   (`org.tron.plugins.utils.MerkleRoot` / `MerkleTree`)
//! - **block hash** = `sha256(blockHeader.rawData.toByteArray())`
//! - **block id** = 8-byte big-endian block number ‖ `blockHash[8..32]`
//!   (`Sha256Hash.generateBlockId`)

use prost::Message;
use tron_crypto::sha256;
use tron_proto::protocol;
use tron_types::H256;

/// Transaction id: `sha256(raw_data bytes)`.
pub fn tx_id(tx: &protocol::Transaction) -> H256 {
    match &tx.raw_data {
        Some(raw) => H256(sha256(&raw.encode_to_vec())),
        None => H256::ZERO,
    }
}

/// Merkle leaf hash: `sha256(full transaction bytes)` (signatures included).
pub fn tx_merkle_leaf(tx: &protocol::Transaction) -> H256 {
    H256(sha256(&tx.encode_to_vec()))
}

/// java-tron's binary SHA256 merkle root.
///
/// Empty → `H256::ZERO`; an odd trailing node is carried up unchanged;
/// parent = `SHA256(left ‖ right)`.
pub fn merkle_root(hashes: &[H256]) -> H256 {
    if hashes.is_empty() {
        return H256::ZERO;
    }
    let mut level: Vec<H256> = hashes.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            match pair {
                [left, right] => {
                    let mut buf = [0u8; 64];
                    buf[..32].copy_from_slice(&left.0);
                    buf[32..].copy_from_slice(&right.0);
                    next.push(H256(sha256(&buf)));
                }
                [odd] => next.push(*odd), // carried up unchanged
                _ => unreachable!(),
            }
        }
        level = next;
    }
    level[0]
}

/// The block's `txTrieRoot`: merkle root over each transaction's full-bytes hash.
pub fn tx_trie_root(block: &protocol::Block) -> H256 {
    let leaves: Vec<H256> = block.transactions.iter().map(tx_merkle_leaf).collect();
    merkle_root(&leaves)
}

/// Block hash: `sha256(header.raw_data bytes)`.
pub fn block_hash(block: &protocol::Block) -> Option<H256> {
    let raw = block.block_header.as_ref()?.raw_data.as_ref()?;
    Some(H256(sha256(&raw.encode_to_vec())))
}

/// Block id: 8-byte big-endian number ‖ `hash[8..32]`.
pub fn block_id(num: i64, hash: &H256) -> H256 {
    let mut id = hash.0;
    id[..8].copy_from_slice(&num.to_be_bytes());
    H256(id)
}

/// Convenience: the block's id from its own header.
pub fn block_id_of(block: &protocol::Block) -> Option<H256> {
    let raw = block.block_header.as_ref()?.raw_data.as_ref()?;
    Some(block_id(raw.number, &block_hash(block)?))
}

/// Recover the block producer's address from the header's witness signature.
///
/// java-tron (`BlockCapsule.validateSignature`): the witness signs the raw-header
/// hash (`sha256(blockHeader.rawData)`); the 65-byte signature is `r ‖ s ‖ v`
/// with `v` either the raw recovery id (0..=3) or Ethereum-style `27 + recid`.
/// The recovered public key's Tron address must equal `raw_data.witness_address`.
pub fn recover_witness(block: &protocol::Block) -> Option<tron_types::Address> {
    let header = block.block_header.as_ref()?;
    let sig = &header.witness_signature;
    if sig.len() != 65 {
        return None;
    }
    let digest = block_hash(block)?.0;
    let mut rs = [0u8; 64];
    rs.copy_from_slice(&sig[..64]);
    let v = sig[64];
    let recovery_id = if v >= 27 { v - 27 } else { v };
    let recoverable = tron_crypto::RecoverableSignature { rs, recovery_id };
    let pubkey = tron_crypto::recover(&digest, &recoverable).ok()?;
    Some(tron_crypto::address_from_public_key(&pubkey))
}

/// Recover the signer address of a transaction's first signature.
///
/// java-tron (`TransactionCapsule.validateSignature`): the signature is over the
/// transaction id (`sha256(raw_data)`); 65 bytes `r ‖ s ‖ v`.
pub fn recover_tx_signer(tx: &protocol::Transaction) -> Option<tron_types::Address> {
    let sig = tx.signature.first()?;
    if sig.len() != 65 {
        return None;
    }
    let digest = tx_id(tx).0;
    let mut rs = [0u8; 64];
    rs.copy_from_slice(&sig[..64]);
    let v = sig[64];
    let recovery_id = if v >= 27 { v - 27 } else { v };
    let pubkey = tron_crypto::recover(&digest, &tron_crypto::RecoverableSignature { rs, recovery_id }).ok()?;
    Some(tron_crypto::address_from_public_key(&pubkey))
}

/// Extract the contract's `owner_address` generically.
///
/// Every Tron contract message (TransferContract, TriggerSmartContract, …) puts
/// `owner_address` at **field 1**, so it can be read from the packed `Any` value
/// without knowing the concrete type: parse the first length-delimited field.
pub fn tx_owner_address(tx: &protocol::Transaction) -> Option<Vec<u8>> {
    let raw = tx.raw_data.as_ref()?;
    let contract = raw.contract.first()?;
    let any = contract.parameter.as_ref()?;
    let b = &any.value;
    // field 1, wire type 2 -> tag byte 0x0a, then varint length
    if b.len() < 2 || b[0] != 0x0a {
        return None;
    }
    let mut len = 0usize;
    let mut shift = 0;
    let mut i = 1;
    loop {
        let x = *b.get(i)?;
        i += 1;
        len |= ((x & 0x7f) as usize) << shift;
        if x & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    b.get(i..i + len).map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> H256 {
        H256([b; 32])
    }

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(merkle_root(&[]), H256::ZERO);
    }

    #[test]
    fn single_leaf_is_identity() {
        assert_eq!(merkle_root(&[h(7)]), h(7));
    }

    #[test]
    fn two_leaves_hash_concatenation() {
        let (a, b) = (h(1), h(2));
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&a.0);
        buf[32..].copy_from_slice(&b.0);
        assert_eq!(merkle_root(&[a, b]), H256(sha256(&buf)));
    }

    #[test]
    fn three_leaves_odd_carried_up() {
        // Level 0: [a,b,c] -> [H(a||b), c] -> H(H(a||b)||c)
        let (a, b, c) = (h(1), h(2), h(3));
        let ab = merkle_root(&[a, b]);
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&ab.0);
        buf[32..].copy_from_slice(&c.0);
        assert_eq!(merkle_root(&[a, b, c]), H256(sha256(&buf)));
    }

    #[test]
    fn five_leaves_shape() {
        // [a b c d e] -> [ab, cd, e] -> [ab_cd, e] -> H(ab_cd || e)
        let ls = [h(1), h(2), h(3), h(4), h(5)];
        let ab = merkle_root(&ls[0..2]);
        let cd = merkle_root(&ls[2..4]);
        let ab_cd = merkle_root(&[ab, cd]);
        assert_eq!(merkle_root(&ls), merkle_root(&[ab_cd, ls[4]]));
    }

    #[test]
    fn block_id_mixes_number() {
        let hash = h(0xee);
        let id = block_id(0x0102_0304, &hash);
        assert_eq!(&id.0[..8], &[0, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(&id.0[8..], &hash.0[8..]);
    }

    #[test]
    fn tx_id_of_missing_raw_is_zero() {
        let tx = tron_proto::protocol::Transaction::default();
        assert_eq!(tx_id(&tx), H256::ZERO);
    }
}
