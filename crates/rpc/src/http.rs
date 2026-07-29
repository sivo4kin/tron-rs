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

/// TRC10 asset endpoints — split into a sibling module (P02) so this file stops
/// growing. New endpoint groups should follow the same `http::<group>` pattern.
pub mod assets;
/// Account resource/stake endpoints (P01) — same sibling-module pattern.
pub mod accounts;

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


/// `POST /wallet/getblockbyid` — body `{ "value": "<block id hex>" }`.
pub fn get_block_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let Some(id_hex) = req.get("value").and_then(Value::as_str) else {
        return error("invalid block id");
    };
    let Ok(id) = hex::decode(id_hex.trim_start_matches("0x")) else {
        return error("invalid block id");
    };
    match state.get_block_by_id(&id) {
        Ok(Some(b)) => block_to_json(&b),
        _ => json!({}),
    }
}


/// `POST /wallet/gettransactioncountbyblocknum` — body `{ "num": n }`.
pub fn get_transaction_count_by_block_num<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let num = req.get("num").and_then(Value::as_i64).unwrap_or(-1);
    match state.get_block_by_num(num) {
        Ok(Some(b)) => json!({ "count": b.transactions.len() }),
        _ => json!({ "count": 0 }),
    }
}

/// `POST /wallet/getblockbylimitnext` — body `{ "startNum": s, "endNum": e }`.
/// Returns blocks in `[startNum, endNum)` (java-tron caps the span; here <= 100).
pub fn get_block_by_limit_next<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let start = req.get("startNum").and_then(Value::as_i64).unwrap_or(0);
    let end = req.get("endNum").and_then(Value::as_i64).unwrap_or(0);
    if end <= start || end - start > 100 {
        return error("request block num error");
    }
    let mut blocks = Vec::new();
    for n in start..end {
        if let Ok(Some(b)) = state.get_block_by_num(n) {
            blocks.push(block_to_json(&b));
        }
    }
    json!({ "block": blocks })
}


/// `POST /wallet/getaccountresource` — body `{ "address": "..." }`.
/// Returns the account's staked resources derived from its `frozen_v2` entries
/// (bandwidth vs energy). A subset of java-tron's resource view.
pub fn get_account_resource<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let Some(addr_str) = req.get("address").and_then(Value::as_str) else {
        return error("invalid address");
    };
    let Some(addr) = parse_req_address(addr_str) else {
        return error("invalid address");
    };
    let account = match state.get_account(&addr) {
        Ok(Some(a)) => a,
        _ => return json!({}),
    };
    // ResourceCode: 0 = BANDWIDTH, 1 = ENERGY (java-tron proto).
    let mut frozen_bandwidth = 0i64;
    let mut frozen_energy = 0i64;
    for f in &account.frozen_v2 {
        match f.r#type {
            1 => frozen_energy += f.amount,
            _ => frozen_bandwidth += f.amount,
        }
    }
    json!({
        "tronPowerLimit": (frozen_bandwidth + frozen_energy) / 1_000_000,
        "netLimit": frozen_bandwidth,
        "EnergyLimit": frozen_energy,
    })
}


/// `POST /wallet/getburntrx` — total TRX burned so far.
pub fn get_burn_trx<S: KvStore>(state: &WorldState<S>) -> Value {
    json!({ "amount": state.get_prop_i64("BURN_TRX_AMOUNT").unwrap_or(0) })
}

/// `POST /wallet/getnextmaintenancetime` — the next DPoS maintenance timestamp.
pub fn get_next_maintenance_time<S: KvStore>(state: &WorldState<S>) -> Value {
    json!({ "num": state.get_prop_i64("NEXT_MAINTENANCE_TIME").unwrap_or(0) })
}

/// `POST /wallet/totaltransaction` — total processed transaction count.
pub fn total_transaction<S: KvStore>(state: &WorldState<S>) -> Value {
    json!({ "num": state.get_prop_i64("TOTAL_TRANSACTION").unwrap_or(0) })
}


/// `POST /wallet/getReward` — body `{ "address": "..." }`. The account's claimable
/// reward allowance (accrued mortgage), in sun.
pub fn get_reward<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let addr = req.get("address").and_then(Value::as_str).and_then(parse_req_address);
    let allowance = addr
        .and_then(|a| state.get_account(&a).ok().flatten())
        .map(|acc| acc.allowance)
        .unwrap_or(0);
    json!({ "reward": allowance })
}

/// `POST /wallet/getBrokerage` — a witness's brokerage percentage (default 20).
pub fn get_brokerage<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    // Read the witness's stored brokerage (UpdateBrokerage actuator), defaulting
    // to the network default when unset or the address is missing/unparseable.
    let b = req
        .get("address")
        .and_then(Value::as_str)
        .and_then(parse_req_address)
        .and_then(|a| state.get_brokerage(&a).ok())
        .unwrap_or(tron_consensus::reward::DEFAULT_BROKERAGE);
    json!({ "brokerage": b })
}


/// `POST /wallet/getenergyprices` — the energy price history string.
pub fn get_energy_prices<S: KvStore>(state: &WorldState<S>) -> Value {
    let price = { let p = state.get_prop_i64("ENERGY_FEE").unwrap_or(0); if p > 0 { p } else { 100 } };
    json!({ "prices": format!("0:{price}") })
}

/// `POST /wallet/getbandwidthprices` — the bandwidth price history string.
pub fn get_bandwidth_prices<S: KvStore>(state: &WorldState<S>) -> Value {
    let price = { let p = state.get_prop_i64("TRANSACTION_FEE").unwrap_or(0); if p > 0 { p } else { 1000 } };
    json!({ "prices": format!("0:{price}") })
}

/// `POST /wallet/getmemofee` — the memo fee history string.
pub fn get_memo_fee<S: KvStore>(state: &WorldState<S>) -> Value {
    let fee = state.get_prop_i64("MEMO_FEE").unwrap_or(0);
    json!({ "prices": format!("0:{fee}") })
}


// -- Proposals & exchanges (P04) --------------------------------------------
//
// The proposal and exchange stores have no `KvStore` iteration (00-conventions §2).
// Both are keyed by a monotonic i64 id (`id.to_be_bytes()`), and the node tracks the
// highest id in a dynamic property (`LATEST_PROPOSAL_NUM` / `LATEST_EXCHANGE_NUM`).
// So enumeration walks `1..=latest` and issues a direct `get` per id — the counter is
// the implicit index; no `scan_prefix` is needed.

/// Dynamic-property key: highest assigned proposal id (java-tron `ProposalActuator`).
const LATEST_PROPOSAL_NUM: &str = "LATEST_PROPOSAL_NUM";
/// Dynamic-property key: highest assigned exchange id (java-tron `ExchangeCreateActuator`).
const LATEST_EXCHANGE_NUM: &str = "LATEST_EXCHANGE_NUM";

fn read_proposal<S: KvStore>(state: &WorldState<S>, id: i64) -> Option<tron_proto::protocol::Proposal> {
    use prost::Message;
    let bytes = state.db.get(tron_state::cf::PROPOSAL, &id.to_be_bytes()).ok().flatten()?;
    tron_proto::protocol::Proposal::decode(bytes.as_slice()).ok()
}

fn read_exchange<S: KvStore>(state: &WorldState<S>, id: i64) -> Option<tron_proto::protocol::Exchange> {
    use prost::Message;
    let bytes = state.db.get(tron_state::cf::EXCHANGE, &id.to_be_bytes()).ok().flatten()?;
    tron_proto::protocol::Exchange::decode(bytes.as_slice()).ok()
}

fn proposal_to_json(p: &tron_proto::protocol::Proposal, visible: bool) -> Value {
    // parameters is a map; sort by key for stable output.
    let mut params: Vec<(i64, i64)> = p.parameters.iter().map(|(k, v)| (*k, *v)).collect();
    params.sort_unstable();
    let parameters: Vec<Value> =
        params.into_iter().map(|(k, v)| json!({ "key": k, "value": v })).collect();
    let approvals: Vec<Value> = p
        .approvals
        .iter()
        .filter_map(|a| render_address(a, visible))
        .map(Value::from)
        .collect();
    json!({
        "proposal_id": p.proposal_id,
        "proposer_address": render_address(&p.proposer_address, visible),
        "parameters": parameters,
        "expiration_time": p.expiration_time,
        "create_time": p.create_time,
        "approvals": approvals,
        "state": p.state,
    })
}

fn exchange_to_json(e: &tron_proto::protocol::Exchange, visible: bool) -> Value {
    // Token ids are stored as their ASCII asset-id bytes; render as strings.
    json!({
        "exchange_id": e.exchange_id,
        "creator_address": render_address(&e.creator_address, visible),
        "create_time": e.create_time,
        "first_token_id": String::from_utf8_lossy(&e.first_token_id),
        "first_token_balance": e.first_token_balance,
        "second_token_id": String::from_utf8_lossy(&e.second_token_id),
        "second_token_balance": e.second_token_balance,
    })
}

/// Ids `[offset+1 ..= offset+limit]` clamped to the live range `1..=latest`.
fn page_range(offset: i64, limit: i64, latest: i64) -> std::ops::RangeInclusive<i64> {
    let start = offset.max(0) + 1;
    let end = (offset.max(0) + limit.max(0)).min(latest);
    start..=end
}

/// `POST /wallet/getproposalbyid` — body `{ "id": <i64> }`. Empty object if absent.
pub fn get_proposal_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = req.get("id").and_then(Value::as_i64) else {
        return error("invalid id");
    };
    match read_proposal(state, id) {
        Some(p) => json!({ "proposal": proposal_to_json(&p, visible) }),
        None => json!({}),
    }
}

/// `POST /wallet/listproposals` — all governance proposals, ascending id.
pub fn list_proposals<S: KvStore>(state: &WorldState<S>) -> Value {
    let latest = state.get_prop_i64(LATEST_PROPOSAL_NUM).unwrap_or(0);
    let proposals: Vec<Value> = (1..=latest)
        .filter_map(|id| read_proposal(state, id).map(|p| proposal_to_json(&p, false)))
        .collect();
    json!({ "proposals": proposals })
}

/// `POST /wallet/getpaginatedproposallist` — body `{ "offset": o, "limit": l }`.
pub fn get_paginated_proposal_list<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let offset = req.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let limit = req.get("limit").and_then(Value::as_i64).unwrap_or(0).min(100);
    let latest = state.get_prop_i64(LATEST_PROPOSAL_NUM).unwrap_or(0);
    let proposals: Vec<Value> = page_range(offset, limit, latest)
        .filter_map(|id| read_proposal(state, id).map(|p| proposal_to_json(&p, false)))
        .collect();
    json!({ "proposals": proposals })
}

/// `POST /wallet/getexchangebyid` — body `{ "id": <i64> }`. Empty object if absent.
pub fn get_exchange_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = req.get("id").and_then(Value::as_i64) else {
        return error("invalid id");
    };
    match read_exchange(state, id) {
        Some(e) => exchange_to_json(&e, visible),
        None => json!({}),
    }
}

/// `POST /wallet/listexchanges` — all Bancor exchanges, ascending id.
pub fn list_exchanges<S: KvStore>(state: &WorldState<S>) -> Value {
    let latest = state.get_prop_i64(LATEST_EXCHANGE_NUM).unwrap_or(0);
    let exchanges: Vec<Value> = (1..=latest)
        .filter_map(|id| read_exchange(state, id).map(|e| exchange_to_json(&e, false)))
        .collect();
    json!({ "exchanges": exchanges })
}

/// `POST /wallet/getpaginatedexchangelist` — body `{ "offset": o, "limit": l }`.
pub fn get_paginated_exchange_list<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let offset = req.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let limit = req.get("limit").and_then(Value::as_i64).unwrap_or(0).min(100);
    let latest = state.get_prop_i64(LATEST_EXCHANGE_NUM).unwrap_or(0);
    let exchanges: Vec<Value> = page_range(offset, limit, latest)
        .filter_map(|id| read_exchange(state, id).map(|e| exchange_to_json(&e, false)))
        .collect();
    json!({ "exchanges": exchanges })
}

/// `POST /wallet/getavailableunfreezecount` — pending Stake 2.0 unfreeze slots
/// used by the account (java-tron caps concurrent unfreezes at 32).
pub fn get_available_unfreeze_count<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    const MAX_UNFREEZE: usize = 32;
    let used = req.get("owner_address").or_else(|| req.get("address"))
        .and_then(Value::as_str).and_then(parse_req_address)
        .and_then(|a| state.get_account(&a).ok().flatten())
        .map(|acc| acc.unfrozen_v2.len())
        .unwrap_or(0);
    json!({ "count": MAX_UNFREEZE.saturating_sub(used) })
}

/// `POST /wallet/getdelegatedresourcev2` — Stake 2.0 delegation records (empty
/// until the delegation store is enumerated).
pub fn get_delegated_resource_v2(_req: &Value) -> Value {
    json!({ "delegatedResource": [] })
}


/// `POST /wallet/getaccountnet` — bandwidth (net) usage/limit for an account,
/// derived from its bandwidth `frozen_v2` stake.
pub fn get_account_net<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let net_staked = req.get("address").and_then(Value::as_str).and_then(parse_req_address)
        .and_then(|a| state.get_account(&a).ok().flatten())
        .map(|acc| acc.frozen_v2.iter().filter(|f| f.r#type == 0).map(|f| f.amount).sum::<i64>())
        .unwrap_or(0);
    json!({ "freeNetLimit": 600, "NetLimit": net_staked, "NetUsed": 0 })
}

/// `POST /wallet/listwitnesses` — the witness (SR) list (empty until the witness
/// store is enumerated).
pub fn list_witnesses() -> Value {
    json!({ "witnesses": [] })
}


#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn seeded() -> (WorldState<MemoryStore>, Address) {
        let ws = WorldState::new(MemoryStore::new());
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
        let ws = WorldState::new(MemoryStore::new());
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
        let ws = WorldState::new(MemoryStore::new());
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
        let ws = WorldState::new(MemoryStore::new());
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
        let ws = WorldState::new(MemoryStore::new());
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
        let ws = WorldState::new(MemoryStore::new());
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

    #[test]
    fn get_block_by_id_endpoint() {
        let ws = WorldState::new(MemoryStore::new());
        let blk = protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw { number: 12, ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        ws.put_block(&blk).unwrap();
        let id = tron_chain::block_id_of(&blk).unwrap();
        let resp = get_block_by_id(&ws, &json!({ "value": id.to_hex() }));
        assert_eq!(resp["block_header"]["raw_data"]["number"], 12);
        assert_eq!(get_block_by_id(&ws, &json!({ "value": "00" })), json!({}));
    }

    fn store_blocks(ws: &mut WorldState<MemoryStore>, nums: &[i64], txs: usize) {
        for &n in nums {
            ws.put_block(&protocol::Block {
                block_header: Some(protocol::BlockHeader {
                    raw_data: Some(protocol::block_header::Raw { number: n, ..Default::default() }),
                    ..Default::default()
                }),
                transactions: vec![protocol::Transaction::default(); txs],
            }).unwrap();
        }
    }

    #[test]
    fn tx_count_by_block_num() {
        let mut ws = WorldState::new(MemoryStore::new());
        store_blocks(&mut ws, &[5], 3);
        assert_eq!(get_transaction_count_by_block_num(&ws, &json!({ "num": 5 }))["count"], 3);
        assert_eq!(get_transaction_count_by_block_num(&ws, &json!({ "num": 9 }))["count"], 0);
    }

    #[test]
    fn block_by_limit_next_range() {
        let mut ws = WorldState::new(MemoryStore::new());
        store_blocks(&mut ws, &[1, 2, 3, 4], 0);
        let resp = get_block_by_limit_next(&ws, &json!({ "startNum": 1, "endNum": 4 }));
        assert_eq!(resp["block"].as_array().unwrap().len(), 3); // 1,2,3
        assert!(get_block_by_limit_next(&ws, &json!({ "startNum": 4, "endNum": 1 }))["Error"].is_string());
    }

    #[test]
    fn account_resource_from_frozen_v2() {
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0x33; 20]);
        ws.put_account(&addr, &protocol::Account {
            address: addr.as_bytes().to_vec(),
            frozen_v2: vec![
                protocol::account::FreezeV2 { r#type: 0, amount: 5_000_000 }, // bandwidth
                protocol::account::FreezeV2 { r#type: 1, amount: 3_000_000 }, // energy
            ],
            ..Default::default()
        }).unwrap();
        let resp = get_account_resource(&ws, &json!({ "address": addr.to_hex() }));
        assert_eq!(resp["netLimit"], 5_000_000);
        assert_eq!(resp["EnergyLimit"], 3_000_000);
        assert_eq!(resp["tronPowerLimit"], 8); // 8 TRX staked
        // unknown account -> empty
        let other = Address::from_body([0x44; 20]);
        assert_eq!(get_account_resource(&ws, &json!({ "address": other.to_hex() })), json!({}));
    }

    #[test]
    fn dynamic_property_endpoints() {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64("BURN_TRX_AMOUNT", 42).unwrap();
        ws.put_prop_i64("NEXT_MAINTENANCE_TIME", 1000).unwrap();
        ws.put_prop_i64("TOTAL_TRANSACTION", 99).unwrap();
        assert_eq!(get_burn_trx(&ws)["amount"], 42);
        assert_eq!(get_next_maintenance_time(&ws)["num"], 1000);
        assert_eq!(total_transaction(&ws)["num"], 99);
        // defaults to 0 when unset
        let empty = WorldState::new(MemoryStore::new());
        assert_eq!(get_burn_trx(&empty)["amount"], 0);
    }

    #[test]
    fn reward_and_brokerage_endpoints() {
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0x55; 20]);
        ws.put_account(&addr, &protocol::Account {
            address: addr.as_bytes().to_vec(), allowance: 777, ..Default::default()
        }).unwrap();
        assert_eq!(get_reward(&ws, &json!({ "address": addr.to_hex() }))["reward"], 777);
        // unknown account -> 0 reward
        let other = Address::from_body([0x66; 20]);
        assert_eq!(get_reward(&ws, &json!({ "address": other.to_hex() }))["reward"], 0);
        // default brokerage 20
        assert_eq!(get_brokerage(&ws, &json!({ "address": addr.to_hex() }))["brokerage"], 20);
        // a stored brokerage is served back
        ws.put_brokerage(&addr, 35).unwrap();
        assert_eq!(get_brokerage(&ws, &json!({ "address": addr.to_hex() }))["brokerage"], 35);
    }

    #[test]
    fn pricing_endpoints() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(get_energy_prices(&ws)["prices"], "0:100"); // default
        ws.put_prop_i64("ENERGY_FEE", 140).unwrap();
        assert_eq!(get_energy_prices(&ws)["prices"], "0:140");
        assert_eq!(get_bandwidth_prices(&ws)["prices"], "0:1000");
        assert_eq!(get_memo_fee(&ws)["prices"], "0:0");
    }

    #[test]
    fn list_endpoints_return_empty_shapes() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(list_exchanges(&ws)["exchanges"].as_array().unwrap().is_empty());
        assert!(list_proposals(&ws)["proposals"].as_array().unwrap().is_empty());
        // asset endpoints moved to `http::assets` (tested there).
    }

    // -- Proposals & exchanges (P04) --------------------------------------

    /// Store a proposal at `id` and bump the LATEST_PROPOSAL_NUM counter.
    fn seed_proposal(ws: &WorldState<MemoryStore>, id: i64, proposer: &Address) {
        use prost::Message;
        let mut params = std::collections::HashMap::new();
        params.insert(1i64, 100i64);
        let p = protocol::Proposal {
            proposal_id: id,
            proposer_address: proposer.as_bytes().to_vec(),
            parameters: params,
            expiration_time: 1_700_000_100_000,
            create_time: 1_700_000_000_000,
            approvals: vec![proposer.as_bytes().to_vec()],
            state: 0,
        };
        ws.db.put(tron_state::cf::PROPOSAL, &id.to_be_bytes(), &p.encode_to_vec()).unwrap();
        ws.put_prop_i64(LATEST_PROPOSAL_NUM, id).unwrap();
    }

    fn seed_exchange(ws: &WorldState<MemoryStore>, id: i64, creator: &Address) {
        use prost::Message;
        let e = protocol::Exchange {
            exchange_id: id,
            creator_address: creator.as_bytes().to_vec(),
            create_time: 1_700_000_000_000,
            first_token_id: b"1000001".to_vec(),
            first_token_balance: 100_000,
            second_token_id: b"_".to_vec(),
            second_token_balance: 50_000,
        };
        ws.db.put(tron_state::cf::EXCHANGE, &id.to_be_bytes(), &e.encode_to_vec()).unwrap();
        ws.put_prop_i64(LATEST_EXCHANGE_NUM, id).unwrap();
    }

    #[test]
    fn proposal_by_id_and_listing() {
        let ws = WorldState::new(MemoryStore::new());
        let proposer = Address::from_body([0xa1; 20]);
        seed_proposal(&ws, 1, &proposer);
        seed_proposal(&ws, 2, &proposer);

        // by-id returns the wrapped proposal with rendered fields.
        let resp = get_proposal_by_id(&ws, &json!({ "id": 1 }));
        assert_eq!(resp["proposal"]["proposal_id"], 1);
        assert_eq!(resp["proposal"]["proposer_address"], proposer.to_hex());
        assert_eq!(resp["proposal"]["parameters"][0]["key"], 1);
        assert_eq!(resp["proposal"]["parameters"][0]["value"], 100);
        assert_eq!(resp["proposal"]["approvals"][0], proposer.to_hex());

        // unknown id -> empty object; bad request -> error.
        assert_eq!(get_proposal_by_id(&ws, &json!({ "id": 99 })), json!({}));
        assert!(get_proposal_by_id(&ws, &json!({}))["Error"].is_string());

        // list returns both, ascending.
        let list = list_proposals(&ws);
        let arr = list["proposals"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["proposal_id"], 1);
        assert_eq!(arr[1]["proposal_id"], 2);

        // visible=true renders the proposer as Base58Check.
        let vis = get_proposal_by_id(&ws, &json!({ "id": 2, "visible": true }));
        assert!(vis["proposal"]["proposer_address"].as_str().unwrap().starts_with('T'));
    }

    #[test]
    fn paginated_proposal_list() {
        let ws = WorldState::new(MemoryStore::new());
        let proposer = Address::from_body([0xa2; 20]);
        for id in 1..=5 {
            seed_proposal(&ws, id, &proposer);
        }
        // offset 1, limit 2 -> ids 2,3
        let page = get_paginated_proposal_list(&ws, &json!({ "offset": 1, "limit": 2 }));
        let arr = page["proposals"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["proposal_id"], 2);
        assert_eq!(arr[1]["proposal_id"], 3);
        // offset past the end -> empty
        assert!(get_paginated_proposal_list(&ws, &json!({ "offset": 10, "limit": 5 }))["proposals"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn exchange_by_id_and_listing() {
        let ws = WorldState::new(MemoryStore::new());
        let creator = Address::from_body([0xb1; 20]);
        seed_exchange(&ws, 1, &creator);
        seed_exchange(&ws, 2, &creator);

        let resp = get_exchange_by_id(&ws, &json!({ "id": 1 }));
        assert_eq!(resp["exchange_id"], 1);
        assert_eq!(resp["creator_address"], creator.to_hex());
        assert_eq!(resp["first_token_id"], "1000001");
        assert_eq!(resp["first_token_balance"], 100_000);
        assert_eq!(resp["second_token_id"], "_");
        assert_eq!(resp["second_token_balance"], 50_000);

        assert_eq!(get_exchange_by_id(&ws, &json!({ "id": 99 })), json!({}));
        assert!(get_exchange_by_id(&ws, &json!({}))["Error"].is_string());

        let list = list_exchanges(&ws);
        assert_eq!(list["exchanges"].as_array().unwrap().len(), 2);

        // paginated: offset 0 limit 1 -> just exchange 1
        let page = get_paginated_exchange_list(&ws, &json!({ "offset": 0, "limit": 1 }));
        let arr = page["exchanges"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["exchange_id"], 1);
    }

    #[test]
    fn available_unfreeze_count() {
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0x77; 20]);
        ws.put_account(&addr, &protocol::Account {
            address: addr.as_bytes().to_vec(),
            unfrozen_v2: vec![protocol::account::UnFreezeV2::default(); 5],
            ..Default::default()
        }).unwrap();
        assert_eq!(get_available_unfreeze_count(&ws, &json!({ "owner_address": addr.to_hex() }))["count"], 27); // 32-5
        // unknown -> full 32 available
        let other = Address::from_body([0x88; 20]);
        assert_eq!(get_available_unfreeze_count(&ws, &json!({ "owner_address": other.to_hex() }))["count"], 32);
        assert!(get_delegated_resource_v2(&json!({}))["delegatedResource"].as_array().unwrap().is_empty());
    }

    #[test]
    fn account_net_and_list_witnesses() {
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0x99; 20]);
        ws.put_account(&addr, &protocol::Account {
            address: addr.as_bytes().to_vec(),
            frozen_v2: vec![protocol::account::FreezeV2 { r#type: 0, amount: 2_000_000 }],
            ..Default::default()
        }).unwrap();
        assert_eq!(get_account_net(&ws, &json!({ "address": addr.to_hex() }))["NetLimit"], 2_000_000);
        assert!(list_witnesses()["witnesses"].as_array().unwrap().is_empty());
    }
}
