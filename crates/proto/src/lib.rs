//! Generated Tron protobuf + gRPC types.
//!
//! All Tron messages are in the `protocol` package; the gRPC services (`Wallet`,
//! `WalletSolidity`, …) are generated alongside. Regenerated from `protos/` on build.

pub mod protocol {
    tonic::include_proto!("protocol");
}

pub use protocol::*;

#[cfg(test)]
mod tests {
    use super::protocol;
    use prost::Message;

    #[test]
    fn block_message_roundtrip() {
        // Build a minimal BlockHeader.raw, encode, decode, and compare — proves the
        // generated types + prost codec are wired correctly.
        let raw = protocol::block_header::Raw {
            number: 42,
            timestamp: 1_700_000_000_000,
            ..Default::default()
        };
        let bytes = raw.encode_to_vec();
        let decoded = protocol::block_header::Raw::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.number, 42);
        assert_eq!(decoded.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn transfer_contract_roundtrip() {
        let c = protocol::TransferContract {
            owner_address: vec![0x41; 21],
            to_address: vec![0x41; 21],
            amount: 1_000_000,
        };
        let decoded =
            protocol::TransferContract::decode(c.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.amount, 1_000_000);
        assert_eq!(decoded.owner_address.len(), 21);
    }
}
