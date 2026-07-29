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
//! **Limitations (documented):** the order-book stores have no per-account order index
//! and no all-pairs index, and `KvStore` has no iteration (00-conventions §2). So
//! `getmarketorderbyaccount` and `getmarketpairlist` return empty shapes — enumerating
//! them needs an index maintained by A09/A10 (or a `scan_prefix` on `KvStore`). The
//! by-id / by-pair endpoints are fully served.

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
///
/// Deviation: the order book keeps no per-account order index (A09/A10), and `KvStore`
/// cannot iterate, so this returns an empty order list. Serving it needs an
/// owner->order-ids index maintained by the market actuators.
pub fn get_market_order_by_account<S: KvStore>(_state: &WorldState<S>, _req: &Value) -> Value {
    json!({ "orders": [] })
}

/// `POST /wallet/getmarketpairlist` — all active trading pairs.
///
/// Deviation: there is no all-pairs index and `KvStore` cannot iterate the
/// `MARKET_PAIR` keys, so this returns an empty pair list. Serving it needs an
/// all-pairs index maintained by the market actuators (or `scan_prefix` on `KvStore`).
pub fn get_market_pair_list<S: KvStore>(_state: &WorldState<S>) -> Value {
    json!({ "orderPair": [] })
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

    #[test]
    fn by_account_and_pair_list_are_empty_shapes() {
        // documented limitations: no per-account order index, no all-pairs index.
        let ws = WorldState::new(MemoryStore::new());
        let addr = Address::from_body([0xe4; 20]);
        assert!(get_market_order_by_account(&ws, &json!({ "value": addr.to_hex() }))["orders"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(get_market_pair_list(&ws)["orderPair"].as_array().unwrap().is_empty());
    }
}
