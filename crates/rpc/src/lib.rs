//! Node APIs (P4): HTTP JSON gateway → gRPC → eth-JSON-RPC.
//!
//! The HTTP surface is contracted by the `tron-openapi` spec (task 1) and
//! conformance-tested against a java-tron reference node. gRPC reuses the generated
//! `tron-proto` service stubs. Default ports mirror java-tron.

pub mod http;

/// Default HTTP fullnode port.
pub const DEFAULT_HTTP_PORT: u16 = 8090;
/// Default gRPC fullnode port.
pub const DEFAULT_GRPC_PORT: u16 = 50051;
/// Default eth-compatible JSON-RPC port.
pub const DEFAULT_JSONRPC_PORT: u16 = 8545;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ports_match_java_tron() {
        assert_eq!(DEFAULT_HTTP_PORT, 8090);
        assert_eq!(DEFAULT_GRPC_PORT, 50051);
        assert_eq!(DEFAULT_JSONRPC_PORT, 8545);
    }
}
