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
//! Secondary indexes (A11), for the account/pair-list HTTP endpoints:
//! - `MARKET_ACCOUNT_ORDER`: `owner_address (21B)` → concatenated 32-byte
//!   `order_id`s of that owner's live orders (append on create; prune on
//!   cancel/full-fill).
//! - `MARKET_PAIRS`: singleton (key [`PAIRS_KEY`]) → the set of active `pair_key`s,
//!   each length-prefixed `u16 BE len ‖ pair_key` (pair keys are variable length).
//!   A pair is inserted when its first order books and removed when its book empties.
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
    put_prices(state, pair, &prices)?;
    // When the pair's book empties, drop it from the all-pairs index (A11).
    if prices.is_empty() {
        remove_pair(state, pair)?;
    }
    Ok(())
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

/// Append an order id to the FIFO at `price`, registering the price and pair if new.
pub fn add_order_to_book<S: KvStore>(
    state: &mut WorldState<S>,
    pair: &[u8],
    price: (i64, i64),
    id: OrderId,
) -> Result<(), ActuatorError> {
    let mut ids = get_order_ids(state, pair, price)?;
    ids.push(id);
    put_order_ids(state, pair, price, &ids)?;
    insert_price(state, pair, price)?;
    add_pair(state, pair) // register the pair in the all-pairs index (A11)
}

// -- MARKET_ACCOUNT_ORDER (owner -> live order ids) -----------------------

/// The owner's live order ids (append-order).
pub fn get_account_orders<S: KvStore>(
    state: &WorldState<S>,
    owner: &[u8],
) -> Result<Vec<OrderId>, ActuatorError> {
    let bytes = state.db.get(cf::MARKET_ACCOUNT_ORDER, owner).map_err(se)?.unwrap_or_default();
    let mut out = Vec::with_capacity(bytes.len() / 32);
    for chunk in bytes.chunks_exact(32) {
        let mut id = [0u8; 32];
        id.copy_from_slice(chunk);
        out.push(id);
    }
    Ok(out)
}

fn put_account_orders<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &[u8],
    ids: &[OrderId],
) -> Result<(), ActuatorError> {
    if ids.is_empty() {
        return state.db.delete(cf::MARKET_ACCOUNT_ORDER, owner).map_err(se);
    }
    let mut bytes = Vec::with_capacity(ids.len() * 32);
    for id in ids {
        bytes.extend_from_slice(id);
    }
    state.db.put(cf::MARKET_ACCOUNT_ORDER, owner, &bytes).map_err(se)
}

/// Append `id` to `owner`'s order index (idempotent).
pub fn add_account_order<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &[u8],
    id: OrderId,
) -> Result<(), ActuatorError> {
    let mut ids = get_account_orders(state, owner)?;
    if !ids.contains(&id) {
        ids.push(id);
        put_account_orders(state, owner, &ids)?;
    }
    Ok(())
}

/// Remove `id` from `owner`'s order index (on cancel / full fill).
pub fn remove_account_order<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &[u8],
    id: OrderId,
) -> Result<(), ActuatorError> {
    let mut ids = get_account_orders(state, owner)?;
    ids.retain(|x| *x != id);
    put_account_orders(state, owner, &ids)
}

// -- MARKET_PAIRS (all active pairs, singleton) ---------------------------

/// Fixed key for the all-pairs singleton record in `cf::MARKET_PAIRS`.
pub const PAIRS_KEY: &[u8] = b"pairs";

/// The set of active `pair_key`s (length-prefixed decode).
pub fn get_pairs<S: KvStore>(state: &WorldState<S>) -> Result<Vec<Vec<u8>>, ActuatorError> {
    let bytes = state.db.get(cf::MARKET_PAIRS, PAIRS_KEY).map_err(se)?.unwrap_or_default();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
        i += 2;
        if i + len > bytes.len() {
            break;
        }
        out.push(bytes[i..i + len].to_vec());
        i += len;
    }
    Ok(out)
}

fn put_pairs<S: KvStore>(state: &mut WorldState<S>, pairs: &[Vec<u8>]) -> Result<(), ActuatorError> {
    if pairs.is_empty() {
        return state.db.delete(cf::MARKET_PAIRS, PAIRS_KEY).map_err(se);
    }
    let mut bytes = Vec::new();
    for p in pairs {
        bytes.extend_from_slice(&(p.len() as u16).to_be_bytes());
        bytes.extend_from_slice(p);
    }
    state.db.put(cf::MARKET_PAIRS, PAIRS_KEY, &bytes).map_err(se)
}

/// Insert `pair` into the all-pairs index (idempotent).
pub fn add_pair<S: KvStore>(state: &mut WorldState<S>, pair: &[u8]) -> Result<(), ActuatorError> {
    let mut pairs = get_pairs(state)?;
    if !pairs.iter().any(|p| p.as_slice() == pair) {
        pairs.push(pair.to_vec());
        put_pairs(state, &pairs)?;
    }
    Ok(())
}

/// Remove `pair` from the all-pairs index.
pub fn remove_pair<S: KvStore>(state: &mut WorldState<S>, pair: &[u8]) -> Result<(), ActuatorError> {
    let mut pairs = get_pairs(state)?;
    pairs.retain(|p| p.as_slice() != pair);
    put_pairs(state, &pairs)
}
