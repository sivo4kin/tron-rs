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

/// `POST /wallet/getnowblock` — the head block.
pub fn get_now_block<S: KvStore>(state: &WorldState<S>) -> Value {
    match state.get_now_block() {
        Ok(Some(b)) => block_to_json(&b),
        Ok(None) => json!({}),
        Err(e) => error(&format!("state error: {e}")),
    }
}

/// `POST /wallet/getblockbynum` — body `{ "num": <i64> }`.
pub fn get_block_by_num<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let Some(num) = req.get("num").and_then(Value::as_i64) else {
        return error("invalid num");
    };
    match state.get_block_by_num(num) {
        Ok(Some(b)) => block_to_json(&b),
        Ok(None) => json!({}),
        Err(e) => error(&format!("state error: {e}")),
    }
}

/// `POST /wallet/validateaddress` — body `{ "address": "..." }`.
/// Returns `{ "result": bool, "message": "..." }` (java-tron address validation).
pub fn validate_address(req: &Value) -> Value {
    let addr_str = req.get("address").and_then(Value::as_str).unwrap_or("");
    let ok = tron_types::Address::from_hex(addr_str).is_ok()
        || tron_types::Address::from_base58check(addr_str).is_ok();
    json!({
        "result": ok,
        "message": if ok { "Base58check or Hex string format error" } else { "Invalid address" }
    })
}

/// `POST /wallet/getblockbylatestnum` — body `{ "num": n }`.
/// Returns the latest `n` blocks (capped), newest-last, as a `{ "block": [...] }`.
pub fn get_block_by_latest_num<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let n = req.get("num").and_then(Value::as_i64).unwrap_or(0).clamp(0, 100);
    let head = state.get_prop_i64(tron_state::blocks::LATEST_BLOCK_NUMBER).unwrap_or(0);
    let start = (head - n + 1).max(0);
    let mut blocks = Vec::new();
    for num in start..=head {
        if let Ok(Some(b)) = state.get_block_by_num(num) {
            blocks.push(block_to_json(&b));
        }
    }
    json!({ "block": blocks })
}


/// Known chain-parameter keys surfaced by `getchainparameters` (subset; grows as
/// the dynamic-property set is modeled).
const CHAIN_PARAM_KEYS: &[&str] = &[
    "getEnergyFee",
    "getCreateAccountFee",
    "getCreateNewAccountFeeInSystemContract",
    "getWitnessPayPerBlock",
    "getMaintenanceTimeInterval",
];

fn prop_for_param(key: &str) -> &str {
    match key {
        "getEnergyFee" => "ENERGY_FEE",
        "getCreateAccountFee" => "CREATE_ACCOUNT_FEE",
        "getCreateNewAccountFeeInSystemContract" => "CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT",
        "getWitnessPayPerBlock" => "WITNESS_PAY_PER_BLOCK",
        "getMaintenanceTimeInterval" => "MAINTENANCE_TIME_INTERVAL",
        _ => key,
    }
}

/// `POST /wallet/getchainparameters` — the committee-adjustable chain parameters
/// as `{ "chainParameter": [ { "key": ..., "value": ... }, ... ] }`.
pub fn get_chain_parameters<S: KvStore>(state: &WorldState<S>) -> Value {
    let params: Vec<Value> = CHAIN_PARAM_KEYS
        .iter()
        .map(|k| {
            let v = state.get_prop_i64(prop_for_param(k)).unwrap_or(0);
            json!({ "key": k, "value": v })
        })
        .collect();
    json!({ "chainParameter": params })
}

/// `POST /wallet/getnodeinfo` — static node identity/config (subset).
pub fn get_node_info(network: &str, p2p_port: u16) -> Value {
    json!({
        "configNodeInfo": {
            "codeVersion": env!("CARGO_PKG_VERSION"),
            "p2pVersion": network,
            "listenPort": p2p_port,
        },
        "solidityBlock": "",
        "activeConnectCount": 0,
    })
}

/// `POST /wallet/listnodes` — discovered peers (empty until the discovery table
/// is populated by the live channel).
pub fn list_nodes() -> Value {
    json!({ "nodes": [] })
}


/// `POST /wallet/getcontract` — body `{ "value": "<address>" }`.
/// Returns the deployed contract's bytecode + address (empty object if none).
pub fn get_contract<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let Some(addr_str) = req.get("value").and_then(Value::as_str) else {
        return error("invalid address");
    };
    let Some(addr) = parse_req_address(addr_str) else {
        return error("invalid address");
    };
    match state.get_code(&addr) {
        Ok(code) if !code.is_empty() => json!({
            "contract_address": addr.to_hex(),
            "bytecode": hex::encode(&code),
        }),
        _ => json!({}),
    }
}

/// `POST /wallet/gettransactionbyid` — body `{ "value": "<txid hex>" }`.
pub fn get_transaction_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let Some(id_hex) = req.get("value").and_then(Value::as_str) else {
        return error("invalid txid");
    };
    let Ok(id) = hex::decode(id_hex.trim_start_matches("0x")) else {
        return error("invalid txid");
    };
    match state.get_transaction(&id) {
        Ok(Some(tx)) => {
            let raw = tx.raw_data.as_ref();
            json!({
                "txID": id_hex,
                "raw_data": {
                    "ref_block_num": raw.map(|r| r.ref_block_num).unwrap_or(0),
                    "timestamp": raw.map(|r| r.timestamp).unwrap_or(0),
                    "contract_count": raw.map(|r| r.contract.len()).unwrap_or(0),
                },
                "signature_count": tx.signature.len(),
            })
        }
        _ => json!({}),
    }
}


/// `POST /wallet/broadcasthex` — body `{ "transaction": "<hex protobuf>" }`.
/// Decodes and structurally validates a signed transaction; returns the java-tron
/// broadcast result shape `{ "result": bool, "txid": ..., "message": ... }`.
/// (Mempool admission + network relay are wired when the channel is live.)
pub fn broadcast_hex(req: &Value) -> Value {
    use prost::Message;
    let Some(hex_str) = req.get("transaction").and_then(Value::as_str) else {
        return json!({ "result": false, "message": "missing transaction" });
    };
    let Ok(bytes) = hex::decode(hex_str.trim_start_matches("0x")) else {
        return json!({ "result": false, "message": "invalid hex" });
    };
    let tx = match tron_proto::protocol::Transaction::decode(bytes.as_slice()) {
        Ok(t) => t,
        Err(e) => return json!({ "result": false, "message": format!("decode error: {e}") }),
    };
    if tx.raw_data.is_none() {
        return json!({ "result": false, "message": "SIGERROR: no raw_data" });
    }
    if tx.signature.is_empty() {
        return json!({ "result": false, "message": "SIGERROR: no signature" });
    }
    let txid = tron_chain::tx_id(&tx);
    json!({ "result": true, "txid": txid.to_hex(), "message": "" })
}


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

    #[test]
    fn get_now_block_and_by_num() {
        let mut ws = WorldState::new(MemoryStore::new());
        let blk = protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw {
                    number: 42,
                    timestamp: 1_700_000_000_000,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            transactions: vec![protocol::Transaction::default()],
            ..Default::default()
        };
        ws.put_block(&blk).unwrap();

        let now = get_now_block(&ws);
        assert_eq!(now["block_header"]["raw_data"]["number"], 42);
        assert_eq!(now["transactions_count"], 1);

        let by_num = get_block_by_num(&ws, &json!({ "num": 42 }));
        assert_eq!(by_num["block_header"]["raw_data"]["number"], 42);

        assert_eq!(get_block_by_num(&ws, &json!({ "num": 99 })), json!({}));
        assert!(get_block_by_num(&ws, &json!({}))["Error"].is_string());
    }

    #[test]
    fn validate_address_accepts_valid_rejects_junk() {
        let a = Address::from_body([0x11; 20]);
        assert_eq!(validate_address(&json!({ "address": a.to_hex() }))["result"], true);
        assert_eq!(validate_address(&json!({ "address": a.to_base58check() }))["result"], true);
        assert_eq!(validate_address(&json!({ "address": "garbage" }))["result"], false);
        assert_eq!(validate_address(&json!({}))["result"], false);
    }

    #[test]
    fn get_block_by_latest_num_returns_recent_blocks() {
        let mut ws = WorldState::new(MemoryStore::new());
        for n in 1..=5i64 {
            ws.put_block(&protocol::Block {
                block_header: Some(protocol::BlockHeader {
                    raw_data: Some(protocol::block_header::Raw { number: n, ..Default::default() }),
                    ..Default::default()
                }),
                ..Default::default()
            }).unwrap();
        }
        let resp = get_block_by_latest_num(&ws, &json!({ "num": 3 }));
        let blocks = resp["block"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["block_header"]["raw_data"]["number"], 3);
        assert_eq!(blocks[2]["block_header"]["raw_data"]["number"], 5);
    }

    #[test]
    fn chain_parameters_reads_dynamic_props() {
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64("ENERGY_FEE", 140).unwrap();
        let resp = get_chain_parameters(&ws);
        let params = resp["chainParameter"].as_array().unwrap();
        let energy = params.iter().find(|p| p["key"] == "getEnergyFee").unwrap();
        assert_eq!(energy["value"], 140);
        // an unset param reads 0
        let ca = params.iter().find(|p| p["key"] == "getCreateAccountFee").unwrap();
        assert_eq!(ca["value"], 0);
    }

    #[test]
    fn node_info_and_list_nodes_shape() {
        let info = get_node_info("nile", 18888);
        assert_eq!(info["configNodeInfo"]["listenPort"], 18888);
        assert!(info["configNodeInfo"]["codeVersion"].is_string());
        assert_eq!(list_nodes()["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn get_contract_returns_bytecode() {
        let mut ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0xcc; 20]);
        ws.put_code(&addr, &[0x60, 0x00]).unwrap();
        let resp = get_contract(&ws, &json!({ "value": addr.to_hex() }));
        assert_eq!(resp["bytecode"], "6000");
        // unknown contract -> empty
        let other = Address::from_body([0xdd; 20]);
        assert_eq!(get_contract(&ws, &json!({ "value": other.to_hex() })), json!({}));
    }

    #[test]
    fn get_transaction_by_id_returns_stored_tx() {
        let mut ws = WorldState::new(MemoryStore::new());
        let tx = protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw { ref_block_num: 5, ..Default::default() }),
            signature: vec![vec![0u8; 65]],
            ..Default::default()
        };
        let id = tron_chain::tx_id(&tx);
        ws.put_transaction(&id.0, &tx).unwrap();
        let resp = get_transaction_by_id(&ws, &json!({ "value": id.to_hex() }));
        assert_eq!(resp["raw_data"]["ref_block_num"], 5);
        assert_eq!(resp["signature_count"], 1);
        assert_eq!(get_transaction_by_id(&ws, &json!({ "value": "00" })), json!({}));
    }

    #[test]
    fn broadcast_hex_validates_and_returns_txid() {
        use prost::Message;
        let tx = protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw { ref_block_num: 1, ..Default::default() }),
            signature: vec![vec![0u8; 65]],
            ..Default::default()
        };
        let hex_tx = hex::encode(tx.encode_to_vec());
        let resp = broadcast_hex(&json!({ "transaction": hex_tx }));
        assert_eq!(resp["result"], true);
        assert_eq!(resp["txid"], tron_chain::tx_id(&tx).to_hex());

        // unsigned tx rejected
        let unsigned = protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw::default()),
            ..Default::default()
        };
        let resp = broadcast_hex(&json!({ "transaction": hex::encode(unsigned.encode_to_vec()) }));
        assert_eq!(resp["result"], false);

        // junk hex rejected
        assert_eq!(broadcast_hex(&json!({ "transaction": "zzzz" }))["result"], false);
    }
}
