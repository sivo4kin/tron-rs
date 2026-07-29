//! Account, resource, reward, and brokerage query endpoints (split from `http.rs`, P05).
//!
//! (The Stake-2.0 delegation/withdraw helpers from P01 live in the sibling
//! [`super::accounts`] module; this holds the original account-info handlers.)

use super::{error, parse_req_address, render_address};
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

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
        assert!(crate::http::list_witnesses()["witnesses"].as_array().unwrap().is_empty());
    }
}
