//! Chain-parameter, dynamic-property, pricing, and address-validation endpoints
//! (split from `http.rs`, P05).

use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    #[test]
    fn validate_address_accepts_valid_rejects_junk() {
        let a = Address::from_body([0x11; 20]);
        assert_eq!(validate_address(&json!({ "address": a.to_hex() }))["result"], true);
        assert_eq!(validate_address(&json!({ "address": a.to_base58check() }))["result"], true);
        assert_eq!(validate_address(&json!({ "address": "garbage" }))["result"], false);
        assert_eq!(validate_address(&json!({}))["result"], false);
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
    fn pricing_endpoints() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(get_energy_prices(&ws)["prices"], "0:100"); // default
        ws.put_prop_i64("ENERGY_FEE", 140).unwrap();
        assert_eq!(get_energy_prices(&ws)["prices"], "0:140");
        assert_eq!(get_bandwidth_prices(&ws)["prices"], "0:1000");
        assert_eq!(get_memo_fee(&ws)["prices"], "0:0");
    }
}
