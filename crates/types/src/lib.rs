//! Core Tron primitive types: [`Address`], [`H256`], and [`Sun`].
//!
//! Encodings follow java-tron: a Tron address is 21 bytes = `0x41` prefix + 20-byte
//! body, rendered either as hex (`41` + 40 hex chars) or Base58Check (Bitcoin-style
//! double-SHA256 checksum), where mainnet Base58 addresses start with `T`.

use thiserror::Error;

/// Mainnet address prefix byte (`0x41`). Testnets share this prefix.
pub const ADDRESS_PREFIX: u8 = 0x41;
/// Length of a Tron address in bytes (prefix + 20-byte body).
pub const ADDRESS_LEN: usize = 21;

#[derive(Debug, Error, PartialEq)]
pub enum TypeError {
    #[error("invalid length: expected {expected}, got {got}")]
    InvalidLength { expected: usize, got: usize },
    #[error("invalid address prefix: expected 0x{ADDRESS_PREFIX:02x}, got 0x{0:02x}")]
    InvalidPrefix(u8),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("base58 decode error: {0}")]
    Base58(String),
}

/// A 21-byte Tron address (`0x41` prefix + 20-byte body).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address([u8; ADDRESS_LEN]);

impl Address {
    /// Construct from a raw 20-byte body, prepending the `0x41` prefix.
    pub fn from_body(body: [u8; 20]) -> Self {
        let mut bytes = [0u8; ADDRESS_LEN];
        bytes[0] = ADDRESS_PREFIX;
        bytes[1..].copy_from_slice(&body);
        Address(bytes)
    }

    /// Construct from 21 raw bytes, validating the prefix.
    pub fn from_bytes(bytes: [u8; ADDRESS_LEN]) -> Result<Self, TypeError> {
        if bytes[0] != ADDRESS_PREFIX {
            return Err(TypeError::InvalidPrefix(bytes[0]));
        }
        Ok(Address(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; ADDRESS_LEN] {
        &self.0
    }

    /// The 20-byte body (address without the prefix).
    pub fn body(&self) -> &[u8] {
        &self.0[1..]
    }

    /// Lowercase hex (`41` + 40 hex chars), matching java-tron's `visible=false` form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, TypeError> {
        let bytes = hex::decode(s.trim_start_matches("0x"))?;
        let arr: [u8; ADDRESS_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| TypeError::InvalidLength { expected: ADDRESS_LEN, got: bytes.len() })?;
        Address::from_bytes(arr)
    }

    /// Base58Check (mainnet form, starts with `T`), matching `visible=true`.
    pub fn to_base58check(&self) -> String {
        bs58::encode(self.0).with_check().into_string()
    }

    pub fn from_base58check(s: &str) -> Result<Self, TypeError> {
        let bytes = bs58::decode(s)
            .with_check(None)
            .into_vec()
            .map_err(|e| TypeError::Base58(e.to_string()))?;
        let arr: [u8; ADDRESS_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| TypeError::InvalidLength { expected: ADDRESS_LEN, got: bytes.len() })?;
        Address::from_bytes(arr)
    }
}

impl core::fmt::Debug for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Address({})", self.to_base58check())
    }
}

/// A 32-byte hash (block id, tx id, storage key, …).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct H256(pub [u8; 32]);

impl H256 {
    pub const ZERO: H256 = H256([0u8; 32]);

    pub fn from_slice(s: &[u8]) -> Result<Self, TypeError> {
        let arr: [u8; 32] = s
            .try_into()
            .map_err(|_| TypeError::InvalidLength { expected: 32, got: s.len() })?;
        Ok(H256(arr))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl core::fmt::Debug for H256 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "H256({})", self.to_hex())
    }
}

/// TRX's smallest unit: 1 TRX = 1_000_000 sun. Balances are signed (java-tron uses int64).
pub type Sun = i64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_hex_roundtrip() {
        let body = [0x11u8; 20];
        let a = Address::from_body(body);
        assert_eq!(a.as_bytes()[0], ADDRESS_PREFIX);
        let hex = a.to_hex();
        assert!(hex.starts_with("41"));
        assert_eq!(Address::from_hex(&hex).unwrap(), a);
    }

    #[test]
    fn address_base58check_roundtrip_and_mainnet_prefix() {
        let a = Address::from_body([0xabu8; 20]);
        let b58 = a.to_base58check();
        // Mainnet Base58Check addresses start with 'T'.
        assert!(b58.starts_with('T'), "got {b58}");
        assert_eq!(Address::from_base58check(&b58).unwrap(), a);
    }

    #[test]
    fn rejects_bad_prefix() {
        let mut bytes = [0u8; ADDRESS_LEN];
        bytes[0] = 0x42;
        assert_eq!(Address::from_bytes(bytes), Err(TypeError::InvalidPrefix(0x42)));
    }

    #[test]
    fn h256_zero_and_hex() {
        assert_eq!(H256::ZERO.to_hex(), "0".repeat(64));
        let h = H256([0xff; 32]);
        assert_eq!(H256::from_slice(&h.0).unwrap(), h);
    }
}
