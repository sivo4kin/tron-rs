//! Contract, transaction lookup, and broadcast endpoints (split from `http.rs`, P05).

use super::{error, parse_req_address};
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

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
    use tron_types::Address;

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
}
