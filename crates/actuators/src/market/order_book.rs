//! DEX order-book index storage — explicit index records (KvStore has no
//! iteration, per 00-conventions §2), mirroring java-tron's
//! `MarketPairToPriceStore` + `MarketPairPriceToOrderStore` + `MarketOrderStore`.
//!
//! Layout (all in the `market_*` column families):
//! - `MARKET_ORDER`: `order_id (32B)` → prost `MarketOrder`.
//! - `MARKET_PAIR` (`pair_to_price`): `pair_key` → the pair's price list, sorted
//!   **best (lowest) first**, encoded as a run of 16-byte `(sell_be, buy_be)`
//!   entries. Replaces java's separate priceNum + ordered `MarketPrice` scan.
//! - `MARKET_PAIR_PRICE`: `pair_key ‖ price16` → FIFO list of `order_id`s at that
//!   price (32 bytes each), maker priority = insertion order (price-time).
//!
//! `pair_key(sell, buy) = sell ‖ 0x00 ‖ buy` (token ids are ascii / `"_"`, never
//! contain a NUL, so the delimiter is unambiguous).

use crate::market::price::cmp_maker_price;
use crate::ActuatorError;
use tron_proto::protocol::MarketOrder;
use tron_state::{cf, WorldState};
use tron_storage::KvStore;

use prost::Message;

pub type OrderId = [u8; 32];

fn se(e: tron_storage::StorageError) -> ActuatorError {
    ActuatorError::State(e.to_string())
}

pub fn pair_key(sell: &[u8], buy: &[u8]) -> Vec<u8> {
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

fn pair_price_key(pair: &[u8], sell: i64, buy: i64) -> Vec<u8> {
    let mut k = Vec::with_capacity(pair.len() + 16);
    k.extend_from_slice(pair);
    k.extend_from_slice(&price16(sell, buy));
    k
}

// -- MARKET_ORDER ----------------------------------------------------------

pub fn get_order<S: KvStore>(
    state: &WorldState<S>,
    id: &OrderId,
) -> Result<Option<MarketOrder>, ActuatorError> {
    match state.db.get(cf::MARKET_ORDER, id).map_err(se)? {
        Some(b) => Ok(Some(
            MarketOrder::decode(b.as_slice()).map_err(|e| ActuatorError::State(e.to_string()))?,
        )),
        None => Ok(None),
    }
}

pub fn put_order<S: KvStore>(state: &mut WorldState<S>, order: &MarketOrder) -> Result<(), ActuatorError> {
    let mut id = [0u8; 32];
    id.copy_from_slice(&order.order_id);
    state.db.put(cf::MARKET_ORDER, &id, &order.encode_to_vec()).map_err(se)
}

// -- MARKET_PAIR (sorted price list) --------------------------------------

/// Sorted (best-first) prices for a pair.
pub fn get_prices<S: KvStore>(
    state: &WorldState<S>,
    pair: &[u8],
) -> Result<Vec<(i64, i64)>, ActuatorError> {
    let bytes = state.db.get(cf::MARKET_PAIR, pair).map_err(se)?.unwrap_or_default();
    let mut out = Vec::with_capacity(bytes.len() / 16);
    for chunk in bytes.chunks_exact(16) {
        let sell = i64::from_be_bytes(chunk[..8].try_into().unwrap());
        let buy = i64::from_be_bytes(chunk[8..].try_into().unwrap());
        out.push((sell, buy));
    }
    Ok(out)
}

fn put_prices<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    prices: &[(i64, i64)],
) -> Result<(), ActuatorError> {
    if prices.is_empty() {
        return state.db.delete(cf::MARKET_PAIR, pair).map_err(se);
    }
    let mut bytes = Vec::with_capacity(prices.len() * 16);
    for &(s, b) in prices {
        bytes.extend_from_slice(&price16(s, b));
    }
    state.db.put(cf::MARKET_PAIR, pair, &bytes).map_err(se)
}

/// Insert `price` into the pair's sorted list (best-first), if absent.
pub fn insert_price<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
) -> Result<(), ActuatorError> {
    let mut prices = get_prices(state, pair)?;
    if prices.iter().any(|&p| cmp_maker_price(p, price).is_eq()) {
        return Ok(());
    }
    let pos = prices
        .binary_search_by(|p| cmp_maker_price(*p, price))
        .unwrap_or_else(|e| e);
    prices.insert(pos, price);
    put_prices(state, pair, &prices)
}

pub fn remove_price<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
) -> Result<(), ActuatorError> {
    let mut prices = get_prices(state, pair)?;
    prices.retain(|&p| !cmp_maker_price(p, price).is_eq());
    put_prices(state, pair, &prices)
}

/// Best (lowest) maker price for the pair, if any.
pub fn best_price<S: KvStore>(
    state: &WorldState<S>,
    pair: &[u8],
) -> Result<Option<(i64, i64)>, ActuatorError> {
    Ok(get_prices(state, pair)?.into_iter().next())
}

// -- MARKET_PAIR_PRICE (FIFO order ids at a price) ------------------------

pub fn get_order_ids<S: KvStore>(
    state: &WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
) -> Result<Vec<OrderId>, ActuatorError> {
    let key = pair_price_key(pair, price.0, price.1);
    let bytes = state.db.get(cf::MARKET_PAIR_PRICE, &key).map_err(se)?.unwrap_or_default();
    let mut out = Vec::with_capacity(bytes.len() / 32);
    for chunk in bytes.chunks_exact(32) {
        let mut id = [0u8; 32];
        id.copy_from_slice(chunk);
        out.push(id);
    }
    Ok(out)
}

pub fn put_order_ids<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
    ids: &[OrderId],
) -> Result<(), ActuatorError> {
    let key = pair_price_key(pair, price.0, price.1);
    if ids.is_empty() {
        return state.db.delete(cf::MARKET_PAIR_PRICE, &key).map_err(se);
    }
    let mut bytes = Vec::with_capacity(ids.len() * 32);
    for id in ids {
        bytes.extend_from_slice(id);
    }
    state.db.put(cf::MARKET_PAIR_PRICE, &key, &bytes).map_err(se)
}

/// Append an order id to the FIFO at `price`, registering the price if new.
pub fn add_order_to_book<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
    id: OrderId,
) -> Result<(), ActuatorError> {
    let mut ids = get_order_ids(state, pair, price)?;
    ids.push(id);
    put_order_ids(state, pair, price, &ids)?;
    insert_price(state, pair, price)
}
