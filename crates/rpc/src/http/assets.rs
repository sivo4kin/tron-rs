//! TRC10 asset (asset-issue) HTTP handlers (P02).
//!
//! Split out of the monolithic `http.rs` to keep it from growing; new handler groups
//! live in sibling `http::*` submodules and reuse the shared helpers (`error`,
//! `render_address`) from the parent module.
//!
//! **Enumeration** (the 00-conventions §2 "no `KvStore` iteration" constraint): TRC10
//! tokens are stored keyed by their numeric id (rendered ascii) and the node tracks the
//! high-water id in the dynamic property `TOKEN_ID_NUM`, whose base is
//! [`tron_state::TOKEN_ID_BASE`] (`1_000_000`). Allocated ids are therefore
//! `TOKEN_ID_BASE+1 ..= get_token_id_num()`; list/paginated walk that range with a
//! direct `get` per id (the counter is the implicit index — no `scan_prefix`, no name
//! index).
//!
//! **by-name limitation:** I02 built no `name -> id` index, so `getassetissuebyname` /
//! `getassetissuelistbyname` linearly scan the id range and match on the stored name.
//! Correct for the modeled data; if the token set grows large this should be replaced
//! with a maintained name index (follow-up on the asset store, not this endpoint task).

use super::{error, render_address};
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

fn asset_to_json(a: &tron_proto::protocol::AssetIssueContract, visible: bool) -> Value {
    json!({
        "id": a.id,
        "owner_address": render_address(&a.owner_address, visible),
        "name": String::from_utf8_lossy(&a.name),
        "abbr": String::from_utf8_lossy(&a.abbr),
        "total_supply": a.total_supply,
        "trx_num": a.trx_num,
        "num": a.num,
        "precision": a.precision,
        "start_time": a.start_time,
        "end_time": a.end_time,
        "description": String::from_utf8_lossy(&a.description),
        "url": String::from_utf8_lossy(&a.url),
    })
}

/// Every stored asset, ascending by token id (`TOKEN_ID_BASE+1 ..= tokenIdNum`).
fn all_assets<S: KvStore>(state: &WorldState<S>) -> Vec<tron_proto::protocol::AssetIssueContract> {
    let latest = state.get_token_id_num().unwrap_or(tron_state::TOKEN_ID_BASE);
    ((tron_state::TOKEN_ID_BASE + 1)..=latest)
        .filter_map(|id| state.get_asset_issue(id).ok().flatten())
        .collect()
}

/// `POST /wallet/getassetissuebyid` — body `{ "value": "<token id>" }`.
pub fn get_asset_issue_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = req
        .get("value")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return error("invalid asset id");
    };
    match state.get_asset_issue(id) {
        Ok(Some(a)) => asset_to_json(&a, visible),
        Ok(None) => json!({}),
        Err(e) => error(&format!("state error: {e}")),
    }
}

/// `POST /wallet/getassetissuebyname` — body `{ "value": "<name>" }`. The first asset
/// whose stored name matches (empty object if none). See the by-name limitation above.
pub fn get_asset_issue_by_name<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(name) = req.get("value").and_then(Value::as_str) else {
        return error("invalid asset name");
    };
    match all_assets(state).into_iter().find(|a| a.name == name.as_bytes()) {
        Some(a) => asset_to_json(&a, visible),
        None => json!({}),
    }
}

/// `POST /wallet/getassetissuelistbyname` — body `{ "value": "<name>" }`. All assets
/// sharing that name (java-tron allows duplicate names post-`allowMultipleName`).
pub fn get_asset_issue_list_by_name<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let name = req.get("value").and_then(Value::as_str).unwrap_or("");
    let assets: Vec<Value> = all_assets(state)
        .iter()
        .filter(|a| a.name == name.as_bytes())
        .map(|a| asset_to_json(a, visible))
        .collect();
    json!({ "assetIssue": assets })
}

/// `POST /wallet/getassetissuelist` — every TRC10 asset issuance.
pub fn get_asset_issue_list<S: KvStore>(state: &WorldState<S>) -> Value {
    let assets: Vec<Value> = all_assets(state).iter().map(|a| asset_to_json(a, false)).collect();
    json!({ "assetIssue": assets })
}

/// `POST /wallet/getpaginatedassetissuelist` — body `{ "offset": o, "limit": l }`.
pub fn get_paginated_asset_issue_list<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let offset = req.get("offset").and_then(Value::as_i64).unwrap_or(0).max(0);
    let limit = req.get("limit").and_then(Value::as_i64).unwrap_or(0).clamp(0, 100);
    let base = tron_state::TOKEN_ID_BASE;
    let latest = state.get_token_id_num().unwrap_or(base);
    // ids are base+1..=latest; page over them.
    let start = base + 1 + offset;
    let end = (base + offset + limit).min(latest);
    let mut assets = Vec::new();
    let mut id = start;
    while id <= end {
        if let Ok(Some(a)) = state.get_asset_issue(id) {
            assets.push(asset_to_json(&a, false));
        }
        id += 1;
    }
    json!({ "assetIssue": assets })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    fn seed_asset(ws: &WorldState<MemoryStore>, id: i64, name: &[u8], owner: &Address) {
        let a = protocol::AssetIssueContract {
            id: id.to_string(),
            owner_address: owner.as_bytes().to_vec(),
            name: name.to_vec(),
            abbr: b"TT".to_vec(),
            total_supply: 1_000_000,
            trx_num: 1,
            num: 100,
            precision: 6,
            start_time: 1,
            end_time: 2,
            description: b"a test token".to_vec(),
            url: b"http://example.test".to_vec(),
            ..Default::default()
        };
        ws.put_asset_issue(id, &a).unwrap();
        ws.save_token_id_num(id).unwrap();
    }

    #[test]
    fn by_id_returns_seeded_asset() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xc1; 20]);
        seed_asset(&ws, 1_000_001, b"TestToken", &owner);

        let resp = get_asset_issue_by_id(&ws, &json!({ "value": "1000001" }));
        assert_eq!(resp["id"], "1000001");
        assert_eq!(resp["name"], "TestToken");
        assert_eq!(resp["owner_address"], owner.to_hex());
        assert_eq!(resp["total_supply"], 1_000_000);
        assert_eq!(resp["num"], 100);

        // visible=true -> base58 owner
        let vis = get_asset_issue_by_id(&ws, &json!({ "value": "1000001", "visible": true }));
        assert!(vis["owner_address"].as_str().unwrap().starts_with('T'));
    }

    #[test]
    fn by_id_unknown_is_empty_and_bad_is_error() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(get_asset_issue_by_id(&ws, &json!({ "value": "1000009" })), json!({}));
        assert!(get_asset_issue_by_id(&ws, &json!({ "value": "not-a-number" }))["Error"].is_string());
        assert!(get_asset_issue_by_id(&ws, &json!({}))["Error"].is_string());
    }

    #[test]
    fn list_returns_all_seeded_assets() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xc2; 20]);
        seed_asset(&ws, 1_000_001, b"Alpha", &owner);
        seed_asset(&ws, 1_000_002, b"Beta", &owner);
        seed_asset(&ws, 1_000_003, b"Alpha", &owner); // duplicate name

        let list = get_asset_issue_list(&ws);
        let arr = list["assetIssue"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], "1000001");
        assert_eq!(arr[2]["id"], "1000003");

        // empty store -> empty list
        let empty = WorldState::new(MemoryStore::new());
        assert!(get_asset_issue_list(&empty)["assetIssue"].as_array().unwrap().is_empty());
    }

    #[test]
    fn by_name_and_list_by_name() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xc3; 20]);
        seed_asset(&ws, 1_000_001, b"Alpha", &owner);
        seed_asset(&ws, 1_000_002, b"Beta", &owner);
        seed_asset(&ws, 1_000_003, b"Alpha", &owner);

        // by-name returns the first match.
        let one = get_asset_issue_by_name(&ws, &json!({ "value": "Beta" }));
        assert_eq!(one["id"], "1000002");
        assert_eq!(get_asset_issue_by_name(&ws, &json!({ "value": "Nope" })), json!({}));

        // list-by-name returns all matches for the duplicated name.
        let many = get_asset_issue_list_by_name(&ws, &json!({ "value": "Alpha" }));
        let arr = many["assetIssue"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "1000001");
        assert_eq!(arr[1]["id"], "1000003");
    }

    #[test]
    fn paginated_asset_list() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xc4; 20]);
        for i in 1..=5 {
            seed_asset(&ws, 1_000_000 + i, format!("T{i}").as_bytes(), &owner);
        }
        // offset 1, limit 2 -> ids 1000002, 1000003
        let page = get_paginated_asset_issue_list(&ws, &json!({ "offset": 1, "limit": 2 }));
        let arr = page["assetIssue"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "1000002");
        assert_eq!(arr[1]["id"], "1000003");
        // offset past the end -> empty
        assert!(get_paginated_asset_issue_list(&ws, &json!({ "offset": 10, "limit": 5 }))
            ["assetIssue"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
