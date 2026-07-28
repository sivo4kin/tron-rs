//! Precompiled contracts (addresses 0x01..), with java-tron energy costs.
//!
//! Costs and outputs mirror java-tron `PrecompiledContracts`:
//! - 0x01 ecrecover — 3000 energy, recovers the signer address (32-byte left-padded).
//! - 0x02 sha256    — `60 + ceil(len/32)*12`, returns sha256(data).
//! - 0x03 ripemd160 — `600 + ceil(len/32)*120` (output modeled as sha256(sha256(data)[..20])
//!   per java-tron's implementation, left-padded to 32 bytes).
//! - 0x04 identity  — `15 + ceil(len/32)*3`, returns data unchanged.

use tron_crypto::{keccak256, recover, sha256, RecoverableSignature};

/// ecrecover fixed energy (java-tron `ENERGY = 3000` for ECRecover).
pub const ECRECOVER_ENERGY: u64 = 3000;

fn words(len: usize) -> u64 {
    ((len + 31) / 32) as u64
}

/// Energy for a precompile at `address` given input `data` (java-tron formulas).
pub fn energy_for(address: u8, data: &[u8]) -> Option<u64> {
    Some(match address {
        0x01 => ECRECOVER_ENERGY,
        0x02 => 60 + words(data.len()) * 12,
        0x03 => 600 + words(data.len()) * 120,
        0x04 => 15 + words(data.len()) * 3,
        _ => return None,
    })
}

/// Execute the precompile at `address`. Returns `None` if no precompile lives there.
pub fn execute(address: u8, data: &[u8]) -> Option<Vec<u8>> {
    match address {
        0x01 => Some(ecrecover(data)),
        0x02 => Some(sha256(data).to_vec()),
        0x03 => Some(ripemd160_like(data)),
        0x04 => Some(data.to_vec()),
        _ => None,
    }
}

/// ecrecover input: hash(32) || v(32) || r(32) || s(32). Returns the 32-byte
/// left-padded recovered address (keccak256(pubkey)[12..]), or 32 zero bytes on
/// failure — matching EVM/java-tron behavior.
fn ecrecover(data: &[u8]) -> Vec<u8> {
    let mut input = [0u8; 128];
    let n = data.len().min(128);
    input[..n].copy_from_slice(&data[..n]);

    let hash: [u8; 32] = input[0..32].try_into().unwrap();
    // v is the last byte of the second word; must be 27 or 28.
    let v = input[63];
    if !(v == 27 || v == 28) {
        return vec![0u8; 32];
    }
    let mut rs = [0u8; 64];
    rs.copy_from_slice(&input[64..128]);
    let sig = RecoverableSignature { rs, recovery_id: v - 27 };
    match recover(&hash, &sig) {
        Ok(pk) => {
            let addr = keccak256(&pk.serialize_uncompressed()[1..]);
            let mut out = vec![0u8; 32];
            out[12..].copy_from_slice(&addr[12..]);
            out
        }
        Err(_) => vec![0u8; 32],
    }
}

/// java-tron's ripemd160 precompile actually double-hashes with sha256 over the
/// first 20 bytes; we reproduce that exact output shape, left-padded to 32 bytes.
fn ripemd160_like(data: &[u8]) -> Vec<u8> {
    let orig = sha256(data);
    let mut target = [0u8; 20];
    target.copy_from_slice(&orig[..20]);
    let hashed = sha256(&target);
    hashed.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_crypto::{public_key, sign_digest, SecretKey};

    #[test]
    fn energy_formulas_match_java_tron() {
        assert_eq!(energy_for(0x04, &[]).unwrap(), 15); // identity empty
        assert_eq!(energy_for(0x04, &[0u8; 32]).unwrap(), 15 + 3); // one word
        assert_eq!(energy_for(0x04, &[0u8; 33]).unwrap(), 15 + 6); // two words
        assert_eq!(energy_for(0x02, &[0u8; 64]).unwrap(), 60 + 24); // sha256 two words
        assert_eq!(energy_for(0x03, &[0u8; 32]).unwrap(), 600 + 120);
        assert_eq!(energy_for(0x01, &[]).unwrap(), 3000);
        assert_eq!(energy_for(0x05, &[]), None);
    }

    #[test]
    fn identity_returns_input() {
        assert_eq!(execute(0x04, b"hello").unwrap(), b"hello");
    }

    #[test]
    fn sha256_precompile_matches_crypto() {
        assert_eq!(execute(0x02, b"abc").unwrap(), sha256(b"abc").to_vec());
    }

    #[test]
    fn ecrecover_roundtrip_recovers_signer() {
        let sk = SecretKey::from_slice(&[0x22u8; 32]).unwrap();
        let pk = public_key(&sk);
        let expected_addr = keccak256(&pk.serialize_uncompressed()[1..]);

        let hash = sha256(b"precompile ecrecover");
        let sig = sign_digest(&sk, &hash).unwrap();

        // Build the 128-byte input: hash || v(27/28) || r||s
        let mut input = vec![0u8; 128];
        input[..32].copy_from_slice(&hash);
        input[63] = 27 + sig.recovery_id;
        input[64..128].copy_from_slice(&sig.rs);

        let out = execute(0x01, &input).unwrap();
        assert_eq!(out.len(), 32);
        // recovered address is in the low 20 bytes, left-padded
        assert_eq!(&out[12..], &expected_addr[12..]);
        assert_eq!(&out[..12], &[0u8; 12]);
    }

    #[test]
    fn ecrecover_bad_v_returns_zero() {
        let out = execute(0x01, &[0u8; 128]).unwrap(); // v=0 invalid
        assert_eq!(out, vec![0u8; 32]);
    }
}
