//! DEX (market) HTTP handlers (P03) — the last of the P01-P04 API set.
//!
//! Reads the order-book index stores that the market actuators (A09/A10) maintain.
//! The on-disk wire format mirrors `crates/actuators/src/market/order_book.rs` (the
//! single source of truth); this module only reads it, via `state.db.get` on the
//! `tron_state::cf::MARKET_*` column families (rpc does not depend on the actuator
//! crate). Layout:
//! - `MARKET_ORDER`: `order_id (32B)` -> prost `MarketOrder`.
//! - `MARKET_PAIR`: `pair_key` -> best-first run of 16-byte `(sell_be, buy_be)` prices.
//! - `MARKET_PAIR_PRICE`: `pair_key ‖ price16` -> FIFO run of 32-byte `order_id`s.
//! - `pair_key(sell, buy) = sell ‖ 0x00 ‖ buy` (token ids are ascii / `"_"`).
//!
//! Secondary indexes (A11) back the account/pair-list endpoints:
//! - `MARKET_ACCOUNT_ORDER`: `owner_address (21B)` -> concatenated 32-byte `order_id`s.
//! - `MARKET_PAIRS`: singleton (key `b"pairs"`) -> length-prefixed `u16 BE len ‖ pair_key`
//!   runs. All five market endpoints are now fully served.

use super::render_address;
use serde_json::{json, Value};
use tron_state::WorldState;
use tron_storage::KvStore;

fn pair_key(sell: &[u8], buy: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(sell.len() + 1 + buy.len());
    k.extend_from_slice(sell);
    k.push(0x00);
    k.extend_from_slice(buy);
    k
}

fn price16(sell: i64, buy: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&sell.to_be_bytes());
    b[8..].copy_from_slice(&buy.to_be_bytes());
    b
}

fn read_order<S: KvStore>(state: &WorldState<S>, id: &[u8]) -> Option<tron_proto::protocol::MarketOrder> {
    use prost::Message;
    if id.len() != 32 {
        return None;
    }
    let bytes = state.db.get(tron_state::cf::MARKET_ORDER, id).ok().flatten()?;
    tron_proto::protocol::MarketOrder::decode(bytes.as_slice()).ok()
}

/// Best-first `(sell, buy)` prices for a pair (decodes the 16-byte runs).
fn read_prices<S: KvStore>(state: &WorldState<S>, pair: &[u8]) -> Vec<(i64, i64)> {
    let bytes = state.db.get(tron_state::cf::MARKET_PAIR, pair).ok().flatten().unwrap_or_default();
    bytes
        .chunks_exact(16)
        .map(|c| {
            let sell = i64::from_be_bytes(c[..8].try_into().unwrap());
            let buy = i64::from_be_bytes(c[8..].try_into().unwrap());
            (sell, buy)
        })
        .collect()
}

/// FIFO order ids at a given price (decodes the 32-byte runs).
fn read_order_ids<S: KvStore>(state: &WorldState<S>, pair: &[u8], sell: i64, buy: i64) -> Vec<[u8; 32]> {
    let mut key = pair.to_vec();
    key.extend_from_slice(&price16(sell, buy));
    let bytes = state.db.get(tron_state::cf::MARKET_PAIR_PRICE, &key).ok().flatten().unwrap_or_default();
    bytes
        .chunks_exact(32)
        .map(|c| {
            let mut id = [0u8; 32];
            id.copy_from_slice(c);
            id
        })
        .collect()
}

fn order_to_json(o: &tron_proto::protocol::MarketOrder, visible: bool) -> Value {
    json!({
        "order_id": hex::encode(&o.order_id),
        "owner_address": render_address(&o.owner_address, visible),
        "create_time": o.create_time,
        "sell_token_id": String::from_utf8_lossy(&o.sell_token_id),
        "sell_token_quantity": o.sell_token_quantity,
        "buy_token_id": String::from_utf8_lossy(&o.buy_token_id),
        "buy_token_quantity": o.buy_token_quantity,
        "sell_token_quantity_remain": o.sell_token_quantity_remain,
    })
}

/// Read the `sell_token_id` / `buy_token_id` string bytes from the request.
fn pair_from_req(req: &Value) -> (Vec<u8>, Vec<u8>) {
    let sell = req.get("sell_token_id").and_then(Value::as_str).unwrap_or("").as_bytes().to_vec();
    let buy = req.get("buy_token_id").and_then(Value::as_str).unwrap_or("").as_bytes().to_vec();
    (sell, buy)
}

/// `POST /wallet/getmarketorderbyid` — body `{ "value": "<order id hex>" }`.
pub fn get_market_order_by_id<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = req
        .get("value")
        .and_then(Value::as_str)
        .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
    else {
        return json!({});
    };
    match read_order(state, &id) {
        Some(o) => order_to_json(&o, visible),
        None => json!({}),
    }
}

/// `POST /wallet/getmarketpricebypair` — body `{ "sell_token_id": ..., "buy_token_id": ... }`.
/// The pair's price book (best-first), java-tron `MarketPriceList`.
pub fn get_market_price_by_pair<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let (sell, buy) = pair_from_req(req);
    let prices: Vec<Value> = read_prices(state, &pair_key(&sell, &buy))
        .into_iter()
        .map(|(s, b)| json!({ "sell_token_quantity": s, "buy_token_quantity": b }))
        .collect();
    json!({
        "sell_token_id": String::from_utf8_lossy(&sell),
        "buy_token_id": String::from_utf8_lossy(&buy),
        "prices": prices,
    })
}

/// `POST /wallet/getmarketorderlistbypair` — body `{ "sell_token_id": ..., "buy_token_id": ... }`.
/// All resting orders for the pair, in book order (best price first, FIFO within a price).
pub fn get_market_order_list_by_pair<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let (sell, buy) = pair_from_req(req);
    let pair = pair_key(&sell, &buy);
    let mut orders = Vec::new();
    for (s, b) in read_prices(state, &pair) {
        for id in read_order_ids(state, &pair, s, b) {
            if let Some(o) = read_order(state, &id) {
                orders.push(order_to_json(&o, visible));
            }
        }
    }
    json!({ "orders": orders })
}

/// `POST /wallet/getmarketorderbyaccount` — body `{ "value": "<address>" }`.
/// The owner's live orders, read from the `MARKET_ACCOUNT_ORDER` index (A11):
/// `owner_address (21B)` -> concatenated 32-byte `order_id`s.
pub fn get_market_order_by_account<S: KvStore>(state: &WorldState<S>, req: &Value) -> Value {
    let visible = req.get("visible").and_then(Value::as_bool).unwrap_or(false);
    let addr = req
        .get("value")
        .or_else(|| req.get("address"))
        .and_then(Value::as_str)
        .and_then(super::parse_req_address);
    let Some(addr) = addr else {
        return json!({ "orders": [] });
    };
    let bytes = state
        .db
        .get(tron_state::cf::MARKET_ACCOUNT_ORDER, addr.as_bytes())
        .ok()
        .flatten()
        .unwrap_or_default();
    let orders: Vec<Value> = bytes
        .chunks_exact(32)
        .filter_map(|id| read_order(state, id).map(|o| order_to_json(&o, visible)))
        .collect();
    json!({ "orders": orders })
}

/// `POST /wallet/getmarketpairlist` — all active trading pairs, read from the
/// `MARKET_PAIRS` singleton index (A11): key `b"pairs"` -> length-prefixed
/// `u16 BE len ‖ pair_key` runs, each `pair_key = sell ‖ 0x00 ‖ buy`.
pub fn get_market_pair_list<S: KvStore>(state: &WorldState<S>) -> Value {
    let bytes = state
        .db
        .get(tron_state::cf::MARKET_PAIRS, b"pairs")
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        i += 2;
        if i + len > bytes.len() {
            break;
        }
        let pk = &bytes[i..i + len];
        i += len;
        if let Some(pos) = pk.iter().position(|&b| b == 0x00) {
            pairs.push(json!({
                "sell_token_id": String::from_utf8_lossy(&pk[..pos]),
                "buy_token_id": String::from_utf8_lossy(&pk[pos + 1..]),
            }));
        }
    }
    json!({ "orderPair": pairs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    fn oid(b: u8) -> [u8; 32] {
        [b; 32]
    }

    /// Seed a resting order for pair (sell,buy) at price (sq,bq), wiring all 3 stores.
    fn seed_order(
        ws: &WorldState<MemoryStore>,
        id: [u8; 32],
        owner: &Address,
        sell: &[u8],
        buy: &[u8],
        price: (i64, i64),
    ) {
        use prost::Message;
        let order = protocol::MarketOrder {
            order_id: id.to_vec(),
            owner_address: owner.as_bytes().to_vec(),
            create_time: 1_700_000_000_000,
            sell_token_id: sell.to_vec(),
            sell_token_quantity: price.0,
            buy_token_id: buy.to_vec(),
            buy_token_quantity: price.1,
            sell_token_quantity_remain: price.0,
            ..Default::default()
        };
        ws.db.put(tron_state::cf::MARKET_ORDER, &id, &order.encode_to_vec()).unwrap();

        let pair = pair_key(sell, buy);
        // append id to the FIFO at this price
        let mut ids = read_order_ids(ws, &pair, price.0, price.1);
        ids.push(id);
        let mut idbytes = Vec::new();
        for i in &ids {
            idbytes.extend_from_slice(i);
        }
        let mut ppk = pair.clone();
        ppk.extend_from_slice(&price16(price.0, price.1));
        ws.db.put(tron_state::cf::MARKET_PAIR_PRICE, &ppk, &idbytes).unwrap();
        // register the price in the pair's list if new
        let mut prices = read_prices(ws, &pair);
        if !prices.contains(&price) {
            prices.push(price);
            let mut pbytes = Vec::new();
            for p in &prices {
                pbytes.extend_from_slice(&price16(p.0, p.1));
            }
            ws.db.put(tron_state::cf::MARKET_PAIR, &pair, &pbytes).unwrap();
        }
    }

    #[test]
    fn order_by_id_returns_seeded_order() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xe1; 20]);
        seed_order(&ws, oid(0x07), &owner, b"1000001", b"_", (100, 5));

        let resp = get_market_order_by_id(&ws, &json!({ "value": hex::encode(oid(0x07)) }));
        assert_eq!(resp["order_id"], hex::encode(oid(0x07)));
        assert_eq!(resp["owner_address"], owner.to_hex());
        assert_eq!(resp["sell_token_id"], "1000001");
        assert_eq!(resp["sell_token_quantity"], 100);
        assert_eq!(resp["buy_token_id"], "_");
        assert_eq!(resp["buy_token_quantity"], 5);

        // unknown / malformed -> empty object
        assert_eq!(get_market_order_by_id(&ws, &json!({ "value": hex::encode(oid(0x09)) })), json!({}));
        assert_eq!(get_market_order_by_id(&ws, &json!({ "value": "00" })), json!({}));
        assert_eq!(get_market_order_by_id(&ws, &json!({})), json!({}));
    }

    #[test]
    fn price_by_pair_returns_the_book() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xe2; 20]);
        seed_order(&ws, oid(0x01), &owner, b"1000001", b"_", (100, 5));
        seed_order(&ws, oid(0x02), &owner, b"1000001", b"_", (200, 9));

        let resp = get_market_price_by_pair(&ws, &json!({ "sell_token_id": "1000001", "buy_token_id": "_" }));
        assert_eq!(resp["sell_token_id"], "1000001");
        assert_eq!(resp["buy_token_id"], "_");
        let prices = resp["prices"].as_array().unwrap();
        assert_eq!(prices.len(), 2);
        assert_eq!(prices[0]["sell_token_quantity"], 100);
        assert_eq!(prices[0]["buy_token_quantity"], 5);
        assert_eq!(prices[1]["sell_token_quantity"], 200);

        // unknown pair -> empty price list
        let empty = get_market_price_by_pair(&ws, &json!({ "sell_token_id": "9", "buy_token_id": "_" }));
        assert!(empty["prices"].as_array().unwrap().is_empty());
    }

    #[test]
    fn order_list_by_pair_returns_orders_in_book_order() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xe3; 20]);
        // two orders at the same price -> FIFO; one at another price.
        seed_order(&ws, oid(0x01), &owner, b"1000001", b"_", (100, 5));
        seed_order(&ws, oid(0x02), &owner, b"1000001", b"_", (100, 5));
        seed_order(&ws, oid(0x03), &owner, b"1000001", b"_", (200, 9));

        let resp = get_market_order_list_by_pair(&ws, &json!({ "sell_token_id": "1000001", "buy_token_id": "_" }));
        let orders = resp["orders"].as_array().unwrap();
        assert_eq!(orders.len(), 3);
        // price (100,5) FIFO first: 0x01 then 0x02, then price (200,9): 0x03
        assert_eq!(orders[0]["order_id"], hex::encode(oid(0x01)));
        assert_eq!(orders[1]["order_id"], hex::encode(oid(0x02)));
        assert_eq!(orders[2]["order_id"], hex::encode(oid(0x03)));

        // unknown pair -> empty
        assert!(get_market_order_list_by_pair(&ws, &json!({ "sell_token_id": "9", "buy_token_id": "_" }))
            ["orders"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    fn seed_account_index(ws: &WorldState<MemoryStore>, owner: &Address, ids: &[[u8; 32]]) {
        let mut b = Vec::new();
        for id in ids {
            b.extend_from_slice(id);
        }
        ws.db.put(tron_state::cf::MARKET_ACCOUNT_ORDER, owner.as_bytes(), &b).unwrap();
    }

    fn seed_pairs(ws: &WorldState<MemoryStore>, pairs: &[(&[u8], &[u8])]) {
        let mut b = Vec::new();
        for (sell, buy) in pairs {
            let pk = pair_key(sell, buy);
            b.extend_from_slice(&(pk.len() as u16).to_be_bytes());
            b.extend_from_slice(&pk);
        }
        ws.db.put(tron_state::cf::MARKET_PAIRS, b"pairs", &b).unwrap();
    }

    #[test]
    fn order_by_account_returns_owner_orders() {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_body([0xe4; 20]);
        seed_order(&ws, oid(0x11), &owner, b"1000001", b"_", (100, 5));
        seed_order(&ws, oid(0x12), &owner, b"1000001", b"_", (200, 9));
        seed_account_index(&ws, &owner, &[oid(0x11), oid(0x12)]);

        let resp = get_market_order_by_account(&ws, &json!({ "value": owner.to_hex() }));
        let orders = resp["orders"].as_array().unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0]["order_id"], hex::encode(oid(0x11)));
        assert_eq!(orders[1]["order_id"], hex::encode(oid(0x12)));

        // unknown account -> empty list
        let other = Address::from_body([0x00; 20]);
        assert!(get_market_order_by_account(&ws, &json!({ "value": other.to_hex() }))["orders"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pair_list_returns_active_pairs() {
        let ws = WorldState::new(MemoryStore::new());
        seed_pairs(&ws, &[(b"1000001", b"_"), (b"1000002", b"1000001")]);

        let resp = get_market_pair_list(&ws);
        let pairs = resp["orderPair"].as_array().unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0]["sell_token_id"], "1000001");
        assert_eq!(pairs[0]["buy_token_id"], "_");
        assert_eq!(pairs[1]["sell_token_id"], "1000002");
        assert_eq!(pairs[1]["buy_token_id"], "1000001");

        // empty store -> empty list
        let empty = WorldState::new(MemoryStore::new());
        assert!(get_market_pair_list(&empty)["orderPair"].as_array().unwrap().is_empty());
    }
}
