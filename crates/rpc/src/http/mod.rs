//! HTTP JSON gateway handlers (P4), split into topical submodules (P05).
//!
//! Pure request→response handlers over [`WorldState`], matching the java-tron
//! FullNode HTTP API whose contract is captured by the `tron-openapi` OpenAPI spec
//! (task 1). Handlers are transport-agnostic (no server bound yet) so they unit-test
//! offline; an axum/hyper binding wires them to a socket later.
//!
//! Address rendering follows java-tron's `visible` flag: hex (`41…`) when false,
//! Base58Check (`T…`) when true.
//!
//! The handlers live in topical submodules; the shared helpers ([`error`],
//! [`parse_req_address`], [`render_address`], [`block_to_json`]) stay here and are used
//! by the submodules via `use super::…`. Each group's handlers are re-exported here so
//! callers keep using `http::<handler>` regardless of which file they live in.

use serde_json::{json, Value};
use tron_types::{Address, ADDRESS_LEN};

// Account-info (get_account, resources, reward, brokerage) — P05 split of the root file.
pub mod accounts_info;
// Account resource/stake endpoints (P01).
pub mod accounts;
// TRC10 asset endpoints (P02).
pub mod assets;
// Block query endpoints (P05 split).
pub mod blocks;
// Chain parameters / dynamic properties / pricing / address validation (P05 split).
pub mod chain;
// Contract, transaction-lookup, and broadcast endpoints (P05 split).
pub mod contracts;
// Governance — proposals & exchanges (P04, P05 split).
pub mod governance;
// DEX / market endpoints (P03).
pub mod market;
// Node identity / peers / witness list (P05 split).
pub mod node;

// Re-export each group's handlers at the `http::` level so `server.rs` and tests keep
// calling `http::<handler>` unchanged. (assets/accounts/market are called via their
// module path — `http::assets::…` — so they are not glob-re-exported here.)
pub use accounts_info::*;
pub use blocks::*;
pub use chain::*;
pub use contracts::*;
pub use governance::*;
pub use node::*;

/// Error body shape java-tron returns (HTTP 200 with an `Error` field, or 400).
fn error(msg: &str) -> Value {
    json!({ "Error": msg })
}

fn parse_req_address(addr_str: &str) -> Option<Address> {
    Address::from_hex(addr_str).ok().or_else(|| Address::from_base58check(addr_str).ok())
}

fn render_address(addr: &[u8], visible: bool) -> Option<String> {
    let arr: [u8; ADDRESS_LEN] = addr.try_into().ok()?;
    let a = Address::from_bytes(arr).ok()?;
    Some(if visible { a.to_base58check() } else { a.to_hex() })
}

/// Render a block as java-tron-shaped JSON (subset: header number/timestamp/
/// txTrieRoot/parentHash/witness, block id, and transaction count).
fn block_to_json(block: &tron_proto::protocol::Block) -> Value {
    let Some(raw) = block.block_header.as_ref().and_then(|h| h.raw_data.as_ref()) else {
        return json!({});
    };
    let header = json!({
        "number": raw.number,
        "timestamp": raw.timestamp,
        "txTrieRoot": hex::encode(&raw.tx_trie_root),
        "parentHash": hex::encode(&raw.parent_hash),
        "witness_address": hex::encode(&raw.witness_address),
        "version": raw.version,
    });
    let block_id = tron_chain::block_id_of(block).map(|h| h.to_hex()).unwrap_or_default();
    json!({
        "blockID": block_id,
        "block_header": { "raw_data": header },
        "transactions_count": block.transactions.len(),
    })
}
