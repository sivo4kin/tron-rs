//! Governance endpoints — proposals & Bancor exchanges (split from `http.rs`, P05).
//!
//! The proposal and exchange stores have no `KvStore` iteration (00-conventions §2).
//! Both are keyed by a monotonic i64 id (`id.to_be_bytes()`), and the node tracks the
//! highest id in a dynamic property (`LATEST_PROPOSAL_NUM` / `LATEST_EXCHANGE_NUM`).
//! So enumeration walks `1..=latest` and issues a direct `get` per id — the counter is
//! the implicit index; no `scan_prefix` is needed.

use super::{error, render_address};
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

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
    fn list_endpoints_return_empty_shapes() {
        let ws = WorldState::new(MemoryStore::new());
        assert!(list_exchanges(&ws)["exchanges"].as_array().unwrap().is_empty());
        assert!(list_proposals(&ws)["proposals"].as_array().unwrap().is_empty());
    }
}
