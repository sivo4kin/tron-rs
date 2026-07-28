//! Hashing, secp256k1 keys, and Tron address derivation.
//!
//! Tron uses the same primitives as Ethereum: keccak256 for address derivation and
//! secp256k1 (recoverable) signatures. An address is `0x41 || keccak256(pubkey_xy)[12..]`.

use sha2::{Digest, Sha256};
use sha3::Keccak256;
use thiserror::Error;
use tron_types::Address;

pub use secp256k1::{PublicKey, SecretKey};

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("secp256k1 error: {0}")]
    Secp(#[from] secp256k1::Error),
    #[error("invalid recovery id: {0}")]
    RecoveryId(i32),
}

/// SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// Double SHA-256 (used e.g. for Base58Check checksums, block/tx ids in parts of Tron).
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

/// keccak256 (Ethereum/Tron variant, not NIST SHA3-256).
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// Derive a Tron [`Address`] from a public key: `0x41 || keccak256(uncompressed[1..])[12..]`.
pub fn address_from_public_key(pk: &PublicKey) -> Address {
    // Uncompressed SEC1 is 65 bytes: 0x04 || X(32) || Y(32). Hash the 64-byte X||Y.
    let uncompressed = pk.serialize_uncompressed();
    let hash = keccak256(&uncompressed[1..]);
    let mut body = [0u8; 20];
    body.copy_from_slice(&hash[12..]);
    Address::from_body(body)
}

/// Derive the public key from a secret key.
pub fn public_key(sk: &SecretKey) -> PublicKey {
    let secp = secp256k1::Secp256k1::new();
    PublicKey::from_secret_key(&secp, sk)
}

/// A recoverable signature: 64-byte r||s plus a recovery id (v).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoverableSignature {
    pub rs: [u8; 64],
    pub recovery_id: u8,
}

/// Sign a 32-byte message digest, producing a recoverable signature (java-tron style).
pub fn sign_digest(sk: &SecretKey, digest: &[u8; 32]) -> Result<RecoverableSignature, CryptoError> {
    let secp = secp256k1::Secp256k1::new();
    let msg = secp256k1::Message::from_digest(*digest);
    let sig = secp.sign_ecdsa_recoverable(&msg, sk);
    let (rec_id, rs) = sig.serialize_compact();
    Ok(RecoverableSignature { rs, recovery_id: rec_id.to_i32() as u8 })
}

/// Recover the signing public key from a digest and recoverable signature.
pub fn recover(digest: &[u8; 32], sig: &RecoverableSignature) -> Result<PublicKey, CryptoError> {
    let secp = secp256k1::Secp256k1::new();
    let msg = secp256k1::Message::from_digest(*digest);
    let rec_id = secp256k1::ecdsa::RecoveryId::from_i32(sig.recovery_id as i32)
        .map_err(|_| CryptoError::RecoveryId(sig.recovery_id as i32))?;
    let rsig = secp256k1::ecdsa::RecoverableSignature::from_compact(&sig.rs, rec_id)?;
    Ok(secp.recover_ecdsa(&msg, &rsig)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keccak_empty_vector() {
        // keccak256("") — the canonical Ethereum/Tron empty-input hash.
        assert_eq!(
            hex::encode(keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn sha256_empty_vector() {
        assert_eq!(
            hex::encode(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    fn test_sk() -> SecretKey {
        SecretKey::from_slice(&[0x11u8; 32]).unwrap()
    }

    #[test]
    fn address_derivation_is_tron_shaped() {
        let addr = address_from_public_key(&public_key(&test_sk()));
        // Prefix byte and mainnet Base58 'T' are the Tron-shape invariants.
        assert_eq!(addr.as_bytes()[0], tron_types::ADDRESS_PREFIX);
        assert!(addr.to_base58check().starts_with('T'));
    }

    #[test]
    fn sign_recover_roundtrip() {
        let sk = test_sk();
        let pk = public_key(&sk);
        let digest = sha256(b"tron-rs p0");
        let sig = sign_digest(&sk, &digest).unwrap();
        let recovered = recover(&digest, &sig).unwrap();
        assert_eq!(recovered, pk);
    }
}
