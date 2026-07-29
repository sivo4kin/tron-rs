//! Account resource/stake HTTP handlers (P01).
//!
//! Split into a sibling `http::accounts` module (continuing the pattern established by
//! `http::assets`) so `http.rs` stops growing. Shared helpers come from the parent via
//! `use super::...`.
//!
//! These read the Stake-2.0 fields the freeze/unfreeze/delegate actuators populate on
//! `protocol.Account` (`frozen_v2`, `unfrozen_v2`). The already-present resource views
//! (`getaccountresource`, `getaccountnet`, `getavailableunfreezecount`,
//! `getdelegatedresourcev2`) stay in `http.rs`; this module adds the endpoints that
//! were missing.

use super::parse_req_address;
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

/// Read the request's owner address from either `owner_address` or `address`.
fn owner_address(req: &Value) -> Option<tron_types::Address> {
    req.get("owner_address")
        .or_else(|| req.get("address"))
        .and_then(Value::as_str)
        .and_then(parse_req_address)
}

/// `POST /wallet/getcanwithdrawunfreezeamount` — body `{ "owner_address": "...",
/// "timestamp": <ms> }`. The total `unfrozen_v2` amount already matured at `timestamp`
/// (java-tron `getCanWithdrawUnfreezeAmount`: sum where `unfreeze_expire_time <= now`).
pub fn get_can_withdraw_unfreeze_amount<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let ts = req.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    let amount = owner_address(req)
        .and_then(|a| state.get_account(&a).ok().flatten())
        .map(|acc| {
            acc.unfrozen_v2
                .iter()
                .filter(|u| u.unfreeze_expire_time <= ts)
                .map(|u| u.unfreeze_amount)
                .sum::<i64>()
        })
        .unwrap_or(0);
    json!({ "amount": amount })
}

/// `POST /wallet/getcandelegatedmaxsize` — body `{ "type": <0=bandwidth|1=energy>,
/// "owner_address": "..." }`. The max amount the account can delegate for the resource.
///
/// Deviation: java-tron subtracts the account's current resource *usage* from the
/// frozen balance; usage accounting isn't modeled (same posture as the delegate
/// actuator), so this returns the full `frozen_v2` balance of the requested resource.
pub fn get_can_delegated_max_size<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let resource = req.get("type").and_then(Value::as_i64).unwrap_or(0) as i32;
    let max_size = owner_address(req)
        .and_then(|a| state.get_account(&a).ok().flatten())
        .map(|acc| {
            acc.frozen_v2
                .iter()
                .filter(|f| f.r#type == resource)
                .map(|f| f.amount)
                .sum::<i64>()
        })
        .unwrap_or(0);
    json!({ "max_size": max_size })
}

/// `POST /wallet/getdelegatedresourceaccountindexv2` — body `{ "value": "<address>" }`.
///
/// Deviation: the delegate actuator does NOT maintain a `DelegatedResourceAccountIndex`
/// (documented there), so the from/to account lists are always empty; only the queried
/// account is echoed. Wiring real from/to lists requires that index as a follow-up.
pub fn get_delegated_resource_account_index_v2<S: KvStore>(
    _state: &WorldState<S>,
    req: &Value,
) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let account = req
        .get("value")
        .and_then(Value::as_str)
        .and_then(parse_req_address)
        .map(|a| if visible { a.to_base58check() } else { a.to_hex() })
        .unwrap_or_default();
    json!({
        "account": account,
        "fromAccounts": [],
        "toAccounts": [],
        "timestamp": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    fn seeded_stake(addr: &Address) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            addr,
            &protocol::Account {
                address: addr.as_bytes().to_vec(),
                frozen_v2: vec![
                    protocol::account::FreezeV2 { r#type: 0, amount: 5_000_000 }, // bandwidth
                    protocol::account::FreezeV2 { r#type: 1, amount: 3_000_000 }, // energy
                ],
                unfrozen_v2: vec![
                    // matured at ts 1_000
                    protocol::account::UnFreezeV2 {
                        r#type: 0,
                        unfreeze_amount: 1_000_000,
                        unfreeze_expire_time: 1_000,
                    },
                    // matures later at ts 5_000
                    protocol::account::UnFreezeV2 {
                        r#type: 0,
                        unfreeze_amount: 2_000_000,
                        unfreeze_expire_time: 5_000,
                    },
                ],
                ..Default::default()
            },
        )
        .unwrap();
        ws
    }

    #[test]
    fn can_withdraw_unfreeze_amount_counts_only_matured() {
        let addr = Address::from_body([0xd1; 20]);
        let ws = seeded_stake(&addr);
        // ts 2000 -> only the first entry (expire 1000) has matured.
        let resp = get_can_withdraw_unfreeze_amount(
            &ws,
            &json!({ "owner_address": addr.to_hex(), "timestamp": 2_000 }),
        );
        assert_eq!(resp["amount"], 1_000_000);
        // ts 10000 -> both matured.
        let all = get_can_withdraw_unfreeze_amount(
            &ws,
            &json!({ "owner_address": addr.to_hex(), "timestamp": 10_000 }),
        );
        assert_eq!(all["amount"], 3_000_000);
        // ts 0 -> nothing matured.
        let none = get_can_withdraw_unfreeze_amount(
            &ws,
            &json!({ "owner_address": addr.to_hex(), "timestamp": 0 }),
        );
        assert_eq!(none["amount"], 0);
        // unknown account -> 0.
        let other = Address::from_body([0xd2; 20]);
        assert_eq!(
            get_can_withdraw_unfreeze_amount(&ws, &json!({ "owner_address": other.to_hex(), "timestamp": 10_000 }))["amount"],
            0
        );
    }

    #[test]
    fn can_delegated_max_size_per_resource() {
        let addr = Address::from_body([0xd3; 20]);
        let ws = seeded_stake(&addr);
        // type 0 = bandwidth
        assert_eq!(
            get_can_delegated_max_size(&ws, &json!({ "type": 0, "owner_address": addr.to_hex() }))["max_size"],
            5_000_000
        );
        // type 1 = energy
        assert_eq!(
            get_can_delegated_max_size(&ws, &json!({ "type": 1, "owner_address": addr.to_hex() }))["max_size"],
            3_000_000
        );
        // unknown account -> 0
        let other = Address::from_body([0xd4; 20]);
        assert_eq!(
            get_can_delegated_max_size(&ws, &json!({ "type": 0, "owner_address": other.to_hex() }))["max_size"],
            0
        );
    }

    #[test]
    fn delegated_resource_account_index_echoes_address_empty_lists() {
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0xd5; 20]);
        let resp = get_delegated_resource_account_index_v2(&ws, &json!({ "value": addr.to_hex() }));
        assert_eq!(resp["account"], addr.to_hex());
        assert!(resp["fromAccounts"].as_array().unwrap().is_empty());
        assert!(resp["toAccounts"].as_array().unwrap().is_empty());
        // visible=true -> base58 account echo
        let vis = get_delegated_resource_account_index_v2(
            &ws,
            &json!({ "value": addr.to_base58check(), "visible": true }),
        );
        assert!(vis["account"].as_str().unwrap().starts_with('T'));
    }
}
