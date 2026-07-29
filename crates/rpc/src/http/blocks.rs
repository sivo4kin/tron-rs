//! Block query endpoints (split from `http.rs`, P05).

use super::{block_to_json, error};
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

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
}
