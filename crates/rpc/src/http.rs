//! HTTP JSON gateway handlers (P4).
//!
//! Pure request→response handlers over [`WorldState`], matching the java-tron
//! FullNode HTTP API whose contract is captured by the `tron-openapi` OpenAPI spec
//! (task 1). Handlers are transport-agnostic (no server bound yet) so they unit-test
//! offline; an axum/hyper binding wires them to a socket later.
//!
//! Address rendering follows java-tron's `visible` flag: hex (`41…`) when false,
//! Base58Check (`T…`) when true.

use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

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

/// `POST /wallet/getaccount` — body `{ "address": "...", "visible": bool }`.
///
/// Returns the account as JSON (a faithful subset of java-tron's JsonFormat:
/// address, balance, create_time, account_name, type). An unknown account returns
/// `{}` (java-tron returns an empty object), matching the OpenAPI response schema.
pub fn get_account<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(addr_str) = req.get("address").and_then(Value::as_str) else {
        return error("invalid address");
    };
    let Some(addr) = parse_req_address(addr_str) else {
        return error("invalid address");
    };

    let account = match state.get_account(&addr) {
        Ok(Some(a)) => a,
        Ok(None) => return json!({}), // java-tron: empty object for unknown account
        Err(e) => return error(&format!("state error: {e}")),
    };

    let mut out = json!({
        "balance": account.balance,
        "create_time": account.create_time,
        "type": account.r#type,
    });
    if let Some(a) = render_address(&account.address, visible) {
        out["address"] = json!(a);
    }
    if !account.account_name.is_empty() {
        // java-tron renders account_name as hex under visible=false.
        out["account_name"] = json!(hex::encode(&account.account_name));
    }
    out
}

/// `GET /wallet/getnowblock`-style block-number probe is out of scope here; this
/// module grows endpoint-by-endpoint against the tron-openapi contract.

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn seeded() -> (WorldState<MemoryStore>, Address) {
        let mut ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0x11; 20]);
        ws.put_account(
            &addr,
            &protocol::Account {
                address: addr.as_bytes().to_vec(),
                balance: 1_234_567,
                create_time: 1_700_000_000_000,
                ..Default::default()
            },
        )
        .unwrap();
        (ws, addr)
    }

    #[test]
    fn get_account_hex_and_visible() {
        let (ws, addr) = seeded();
        // visible=false -> hex address
        let resp = get_account(&ws, &json!({ "address": addr.to_hex() }));
        assert_eq!(resp["balance"], 1_234_567);
        assert_eq!(resp["address"], addr.to_hex());
        assert!(resp["address"].as_str().unwrap().starts_with("41"));

        // visible=true -> Base58Check address, and it accepts base58 input too
        let resp = get_account(&ws, &json!({ "address": addr.to_base58check(), "visible": true }));
        assert_eq!(resp["address"], addr.to_base58check());
        assert!(resp["address"].as_str().unwrap().starts_with('T'));
    }

    #[test]
    fn unknown_account_is_empty_object() {
        let (ws, _) = seeded();
        let other = Address::from_body([0x22; 20]);
        let resp = get_account(&ws, &json!({ "address": other.to_hex() }));
        assert_eq!(resp, json!({}));
    }

    #[test]
    fn bad_address_is_error() {
        let (ws, _) = seeded();
        assert!(get_account(&ws, &json!({ "address": "not-an-address" }))["Error"].is_string());
        assert!(get_account(&ws, &json!({}))["Error"].is_string());
    }
}
