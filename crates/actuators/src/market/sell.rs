//! `MarketSellAssetContract` — place a DEX sell order and match it against the
//! order book. Port of java-tron `MarketSellAssetActuator`.
//!
//! **validate** — market feature on (TODO feature-gate); owner exists; sell/buy
//! ids valid + distinct; quantities positive and within the limit; owner holds
//! the sell quantity (+ TRX fee). **execute** — charge + burn the fee, escrow the
//! sell quantity, create an order, match it against the best crossing maker
//! orders (price-time priority, [`price::price_match`] / [`price::multiply_and_divide`]),
//! settle fills, then book any residual.
//!
//! Deviations from java-tron (data-only):
//! - Gated on `ALLOW_MARKET_TRANSACTION` (java `supportAllowMarketTransaction`).
//! - Single V2 asset store; the sell-token AssetIssue existence check is folded
//!   into the balance check (a token the owner doesn't hold reads as 0).
//! - The per-account active-order-count cap and the intrusive prev/next order
//!   linked-list are not modelled; the FIFO at each price is an explicit id list
//!   (see [`super::order_book`]). Order ids are a monotonic counter, not a hash.

use crate::market::order_book as ob;
use crate::market::price::{multiply_and_divide, price_match};
use crate::market::{
    credit_token, debit_token, is_number, is_trx, token_balance, MARKET_ORDER_NUM,
    MARKET_QUANTITY_LIMIT, MARKET_SELL_FEE, MAX_MATCH_NUM,
};
use crate::market::DEFAULT_MARKET_QUANTITY_LIMIT;
use crate::{require_feature, ActuatorError, ExecutionResult};
use tron_proto::protocol::market_order::State;
use tron_proto::protocol::{MarketOrder, MarketSellAssetContract};
use tron_state::features::flags;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

fn valid_token(id: &[u8]) -> bool {
    is_trx(id) || is_number(id)
}

pub struct MarketSellAssetActuator<'a> {
    contract: &'a MarketSellAssetContract,
}

impl<'a> MarketSellAssetActuator<'a> {
    pub fn new(contract: &'a MarketSellAssetContract) -> Self {
        Self { contract }
    }

    fn fee<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        Ok(state.get_prop_i64(MARKET_SELL_FEE)?.max(0))
    }

    fn quantity_limit<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let l = state.get_prop_i64(MARKET_QUANTITY_LIMIT)?;
        Ok(if l > 0 { l } else { DEFAULT_MARKET_QUANTITY_LIMIT })
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        // java rejects unless the market feature is committee-enabled.
        require_feature(
            state,
            flags::ALLOW_MARKET_TRANSACTION,
            "Not support Market Transaction, need to be opened by the committee",
        )?;

        let c = self.contract;
        let owner = parse_address(&c.owner_address)?;

        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist!".into()))?;

        if !valid_token(&c.sell_token_id) {
            return Err(ActuatorError::Validate("sellTokenId is not a valid number".into()));
        }
        if !valid_token(&c.buy_token_id) {
            return Err(ActuatorError::Validate("buyTokenId is not a valid number".into()));
        }
        if c.sell_token_id == c.buy_token_id {
            return Err(ActuatorError::Validate("cannot exchange same tokens".into()));
        }
        if c.sell_token_quantity <= 0 || c.buy_token_quantity <= 0 {
            return Err(ActuatorError::Validate("token quantity must greater than zero".into()));
        }
        let limit = Self::quantity_limit(state)?;
        if c.sell_token_quantity > limit || c.buy_token_quantity > limit {
            return Err(ActuatorError::Validate(format!("token quantity must less than {limit}")));
        }

        let fee = Self::fee(state)?;
        if is_trx(&c.sell_token_id) {
            if account.balance < c.sell_token_quantity + fee {
                return Err(ActuatorError::Validate("No enough balance !".into()));
            }
        } else {
            if account.balance < fee {
                return Err(ActuatorError::Validate("No enough balance !".into()));
            }
            if token_balance(&account, &c.sell_token_id) < c.sell_token_quantity {
                return Err(ActuatorError::Validate("SellToken balance is not enough !".into()));
            }
        }

        Ok(fee)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let c = self.contract;
        let owner = parse_address(&c.owner_address)?;
        let fee = Self::fee(state)?;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        // Fee (charged + burned) and escrow of the full sell quantity.
        account.balance = account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        debit_token(&mut account, &c.sell_token_id, c.sell_token_quantity)?;

        // Create the taker order.
        let id = Self::allocate_order_id(state)?;
        let mut taker = MarketOrder {
            order_id: id.to_vec(),
            owner_address: owner.as_bytes().to_vec(),
            create_time: now,
            sell_token_id: c.sell_token_id.clone(),
            sell_token_quantity: c.sell_token_quantity,
            buy_token_id: c.buy_token_id.clone(),
            buy_token_quantity: c.buy_token_quantity,
            sell_token_quantity_remain: c.sell_token_quantity,
            sell_token_quantity_return: 0,
            state: State::Active as i32,
            prev: vec![],
            next: vec![],
        };

        // Secondary index: record the order under its owner (A11).
        ob::add_account_order(state, owner.as_bytes(), id)?;

        Self::match_order(state, &mut taker, &mut account)?;

        // Book any residual on the taker's own side; otherwise it fully filled and
        // is no longer live, so drop it from the owner index.
        if taker.sell_token_quantity_remain != 0 {
            let pair = ob::pair_key(&c.sell_token_id, &c.buy_token_id);
            ob::add_order_to_book(state, &pair, (c.sell_token_quantity, c.buy_token_quantity), id)?;
        } else {
            ob::remove_account_order(state, owner.as_bytes(), id)?;
        }

        ob::put_order(state, &taker)?;
        state.put_account(&owner, &account)?;
        if fee > 0 {
            state.burn_trx(fee)?;
        }

        Ok(ExecutionResult { fee })
    }

    fn allocate_order_id<S: KvStore>(state: &mut WorldState<S>) -> Result<[u8; 32], ActuatorError> {
        let n = state.get_prop_i64(MARKET_ORDER_NUM)? + 1;
        state.put_prop_i64(MARKET_ORDER_NUM, n)?;
        let mut id = [0u8; 32];
        id[24..].copy_from_slice(&n.to_be_bytes());
        Ok(id)
    }

    /// Match `taker` against the best crossing maker orders (price-time priority).
    fn match_order<S: KvStore>(
        state: &mut WorldState<S>,
        taker: &mut MarketOrder,
        taker_account: &mut tron_proto::protocol::Account,
    ) -> Result<(), ActuatorError> {
        let taker_sell = taker.sell_token_quantity;
        let taker_buy = taker.buy_token_quantity;
        // Maker pair is the opposite side.
        let maker_pair = ob::pair_key(&taker.buy_token_id, &taker.sell_token_id);
        let mut match_count: u32 = 0;

        while taker.sell_token_quantity_remain != 0 {
            let best = match ob::best_price(state, &maker_pair)? {
                Some(p) => p,
                None => break,
            };
            if !price_match(taker_sell, taker_buy, best.0, best.1) {
                break;
            }

            let mut ids = ob::get_order_ids(state, &maker_pair, best)?;
            while taker.sell_token_quantity_remain != 0 && !ids.is_empty() {
                let maker_id = ids[0];
                let mut maker = ob::get_order(state, &maker_id)?
                    .ok_or_else(|| ActuatorError::Execute("maker order missing".into()))?;

                let stop = Self::match_single(state, taker, &mut maker, taker_account)?;

                if maker.sell_token_quantity_remain == 0 {
                    maker.state = State::Inactive as i32;
                    ob::put_order(state, &maker)?;
                    ob::remove_account_order(state, &maker.owner_address, maker_id)?;
                    ids.remove(0);
                } else {
                    ob::put_order(state, &maker)?;
                }

                match_count += 1;
                if match_count > MAX_MATCH_NUM {
                    return Err(ActuatorError::Validate(format!(
                        "Too many matches. MAX_MATCH_NUM = {MAX_MATCH_NUM}"
                    )));
                }
                if stop {
                    ob::put_order_ids(state, &maker_pair, best, &ids)?;
                    return Ok(());
                }
            }

            ob::put_order_ids(state, &maker_pair, best, &ids)?;
            if ids.is_empty() {
                ob::remove_price(state, &maker_pair, best)?;
            } else {
                break; // taker exhausted
            }
        }
        Ok(())
    }

    /// Settle one taker/maker fill. Returns `true` if matching must stop (the
    /// taker's remaining quantity was too small to buy anything and was returned).
    fn match_single<S: KvStore>(
        state: &mut WorldState<S>,
        taker: &mut MarketOrder,
        maker: &mut MarketOrder,
        taker_account: &mut tron_proto::protocol::Account,
    ) -> Result<bool, ActuatorError> {
        let taker_sell_remain = taker.sell_token_quantity_remain;
        let maker_sell = maker.sell_token_quantity;
        let maker_buy = maker.buy_token_quantity;
        let maker_sell_remain = maker.sell_token_quantity_remain;

        // How much of the maker's sell token the taker can buy for its remainder.
        let taker_buy_remain = multiply_and_divide(taker_sell_remain, maker_sell, maker_buy);
        if taker_buy_remain == 0 {
            // Too small — return the taker's remaining sell token to its owner.
            credit_token(taker_account, &taker.sell_token_id, taker_sell_remain)?;
            taker.sell_token_quantity_return = taker_sell_remain;
            taker.sell_token_quantity_remain = 0;
            taker.state = State::Inactive as i32;
            return Ok(true);
        }

        let taker_buy_receive;
        let maker_buy_receive;
        if taker_buy_remain == maker_sell_remain {
            maker_buy_receive = multiply_and_divide(maker_sell_remain, maker_buy, maker_sell);
            taker_buy_receive = maker_sell_remain;
            taker.sell_token_quantity_remain = taker_sell_remain - maker_buy_receive;
            maker.sell_token_quantity_remain = 0;
        } else if taker_buy_remain < maker_sell_remain {
            taker_buy_receive = taker_buy_remain;
            maker_buy_receive = taker_sell_remain;
            taker.sell_token_quantity_remain = 0;
            maker.sell_token_quantity_remain = maker_sell_remain - taker_buy_remain;
        } else {
            taker_buy_receive = maker_sell_remain;
            maker_buy_receive = multiply_and_divide(maker_sell_remain, maker_buy, maker_sell);
            if maker_buy_receive == 0 {
                // Return the maker's remainder to the maker (rare edge).
                let maker_owner = parse_address(&maker.owner_address)?;
                let mut ma = state
                    .get_account(&maker_owner)?
                    .ok_or_else(|| ActuatorError::Execute("maker account missing".into()))?;
                credit_token(&mut ma, &maker.sell_token_id, maker_sell_remain)?;
                state.put_account(&maker_owner, &ma)?;
                maker.sell_token_quantity_return = maker_sell_remain;
                maker.sell_token_quantity_remain = 0;
                maker.state = State::Inactive as i32;
                return Ok(false);
            }
            maker.sell_token_quantity_remain = 0;
            taker.sell_token_quantity_remain = taker_sell_remain - maker_buy_receive;
        }

        // Credit both sides from the other's escrow.
        credit_token(taker_account, &taker.buy_token_id, taker_buy_receive)?;
        let maker_owner = parse_address(&maker.owner_address)?;
        let mut ma = state
            .get_account(&maker_owner)?
            .ok_or_else(|| ActuatorError::Execute("maker account missing".into()))?;
        credit_token(&mut ma, &maker.buy_token_id, maker_buy_receive)?;
        state.put_account(&maker_owner, &ma)?;

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const TRX: &[u8] = b"_";
    const A: &[u8] = b"1000001";

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn ws() -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, 1_700_000_000_000).unwrap();
        ws.put_prop_i64(flags::ALLOW_MARKET_TRANSACTION, 1).unwrap(); // gate on
        ws
    }

    fn set_account(ws: &WorldState<MemoryStore>, a: &Address, trx: i64, a_tokens: i64) {
        let mut acc = protocol::Account { address: a.as_bytes().to_vec(), balance: trx, ..Default::default() };
        if a_tokens > 0 {
            acc.asset_v2.insert("1000001".into(), a_tokens);
        }
        ws.put_account(a, &acc).unwrap();
    }

    fn tok(acc: &protocol::Account, id: &[u8]) -> i64 {
        token_balance(acc, id)
    }

    fn sell(owner: &Address, sid: &[u8], sq: i64, bid: &[u8], bq: i64) -> MarketSellAssetContract {
        MarketSellAssetContract {
            owner_address: owner.as_bytes().to_vec(),
            sell_token_id: sid.to_vec(),
            sell_token_quantity: sq,
            buy_token_id: bid.to_vec(),
            buy_token_quantity: bq,
        }
    }

    #[test]
    fn no_match_order_is_booked() {
        let w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 100); // holds 100 A
        let c = sell(&m, A, 100, TRX, 100); // sell 100 A for 100 TRX
        let mut w = w;
        assert_eq!(MarketSellAssetActuator::new(&c).validate(&w).unwrap(), 0);
        MarketSellAssetActuator::new(&c).execute(&mut w).unwrap();

        // Escrowed: A balance now 0. Order booked at its price.
        assert_eq!(tok(&w.get_account(&m).unwrap().unwrap(), A), 0);
        let pair = ob::pair_key(A, TRX);
        assert_eq!(ob::best_price(&w, &pair).unwrap(), Some((100, 100)));
        let ids = ob::get_order_ids(&w, &pair, (100, 100)).unwrap();
        assert_eq!(ids.len(), 1);
        let order = ob::get_order(&w, &ids[0]).unwrap().unwrap();
        assert_eq!(order.sell_token_quantity_remain, 100);
    }

    #[test]
    fn two_crossing_orders_fully_match_and_settle() {
        let mut w = ws();
        let (m, t) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100); // maker holds 100 A
        set_account(&w, &t, 100, 0); // taker holds 100 TRX

        // Maker: sell 100 A for 100 TRX.
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 100)).execute(&mut w).unwrap();
        // Taker: sell 100 TRX for 100 A (crosses at price 1).
        MarketSellAssetActuator::new(&sell(&t, TRX, 100, A, 100)).execute(&mut w).unwrap();

        let ma = w.get_account(&m).unwrap().unwrap();
        let ta = w.get_account(&t).unwrap().unwrap();
        assert_eq!(ma.balance, 100); // maker got 100 TRX
        assert_eq!(tok(&ma, A), 0);
        assert_eq!(ta.balance, 0);
        assert_eq!(tok(&ta, A), 100); // taker got 100 A
        // Book empty on both sides.
        assert_eq!(ob::best_price(&w, &ob::pair_key(A, TRX)).unwrap(), None);
        assert_eq!(ob::best_price(&w, &ob::pair_key(TRX, A)).unwrap(), None);
    }

    #[test]
    fn account_index_and_pair_list_track_resting_orders() {
        let mut w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 300); // 300 A
        // Three non-crossing sell orders on pair (A, TRX) at distinct prices.
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 100)).execute(&mut w).unwrap();
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 90)).execute(&mut w).unwrap();
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 110)).execute(&mut w).unwrap();

        // Owner index lists all three; the all-pairs index has the single pair.
        assert_eq!(ob::get_account_orders(&w, m.as_bytes()).unwrap().len(), 3);
        let pairs = ob::get_pairs(&w).unwrap();
        assert_eq!(pairs, vec![ob::pair_key(A, TRX)]);
    }

    #[test]
    fn full_match_drops_both_from_account_index_and_empties_pairs() {
        let mut w = ws();
        let (m, t) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100);
        set_account(&w, &t, 100, 0);
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 100)).execute(&mut w).unwrap();
        assert_eq!(ob::get_account_orders(&w, m.as_bytes()).unwrap().len(), 1);
        assert_eq!(ob::get_pairs(&w).unwrap().len(), 1);

        // Taker fully consumes the maker: both leave the account index, pairs empty.
        MarketSellAssetActuator::new(&sell(&t, TRX, 100, A, 100)).execute(&mut w).unwrap();
        assert!(ob::get_account_orders(&w, m.as_bytes()).unwrap().is_empty()); // maker filled
        assert!(ob::get_account_orders(&w, t.as_bytes()).unwrap().is_empty()); // taker filled, never booked
        assert!(ob::get_pairs(&w).unwrap().is_empty()); // no resting orders
    }

    #[test]
    fn partial_fill_leaves_residual() {
        let mut w = ws();
        let (m, t) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100);
        set_account(&w, &t, 200, 0);
        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 100)).execute(&mut w).unwrap();
        // Taker wants 200 A for 200 TRX; only 100 A available -> 100 filled, 100 TRX residual.
        MarketSellAssetActuator::new(&sell(&t, TRX, 200, A, 200)).execute(&mut w).unwrap();

        let ta = w.get_account(&t).unwrap().unwrap();
        assert_eq!(tok(&ta, A), 100); // filled 100 A
        assert_eq!(ta.balance, 0); // 100 spent on fill, 100 escrowed in residual
        // Maker side consumed; taker residual booked on (TRX,A) at (200,200).
        assert_eq!(ob::best_price(&w, &ob::pair_key(A, TRX)).unwrap(), None);
        let resid = ob::get_order_ids(&w, &ob::pair_key(TRX, A), (200, 200)).unwrap();
        assert_eq!(resid.len(), 1);
        assert_eq!(
            ob::get_order(&w, &resid[0]).unwrap().unwrap().sell_token_quantity_remain,
            100
        );
    }

    #[test]
    fn price_priority_best_maker_first() {
        let mut w = ws();
        let (m1, m2, t) = (addr(1), addr(2), addr(3));
        set_account(&w, &m1, 0, 100);
        set_account(&w, &m2, 0, 100);
        set_account(&w, &t, 90, 0);
        // m1 asks 100 TRX for 100 A (price 1.0); m2 asks 90 TRX for 100 A (price 0.9, cheaper).
        MarketSellAssetActuator::new(&sell(&m1, A, 100, TRX, 100)).execute(&mut w).unwrap();
        MarketSellAssetActuator::new(&sell(&m2, A, 100, TRX, 90)).execute(&mut w).unwrap();
        // Taker sells 90 TRX wanting 100 A: should match the cheaper maker m2 fully.
        MarketSellAssetActuator::new(&sell(&t, TRX, 90, A, 100)).execute(&mut w).unwrap();

        assert_eq!(w.get_account(&m2).unwrap().unwrap().balance, 90); // m2 filled, got 90 TRX
        assert_eq!(w.get_account(&m1).unwrap().unwrap().balance, 0); // m1 untouched
        assert_eq!(tok(&w.get_account(&t).unwrap().unwrap(), A), 100);
        // m1 still booked, m2 gone.
        assert_eq!(ob::best_price(&w, &ob::pair_key(A, TRX)).unwrap(), Some((100, 100)));
    }

    #[test]
    fn rejects_insufficient_balance() {
        let w = ws();
        let t = addr(1);
        set_account(&w, &t, 50, 0);
        let c = sell(&t, TRX, 100, A, 100);
        assert!(matches!(
            MarketSellAssetActuator::new(&c).validate(&w),
            Err(ActuatorError::Validate(m)) if m.contains("No enough balance !")
        ));
    }

    #[test]
    fn rejects_when_market_feature_disabled() {
        let w = ws();
        w.put_prop_i64(flags::ALLOW_MARKET_TRANSACTION, 0).unwrap();
        let t = addr(1);
        set_account(&w, &t, 100, 0);
        assert!(matches!(
            MarketSellAssetActuator::new(&sell(&t, TRX, 100, A, 100)).validate(&w),
            Err(ActuatorError::Validate(m)) if m.contains("Not support Market Transaction")
        ));
    }

    #[test]
    fn rejects_same_and_bad_tokens() {
        let w = ws();
        let t = addr(1);
        set_account(&w, &t, 100, 0);
        assert!(matches!(
            MarketSellAssetActuator::new(&sell(&t, TRX, 10, TRX, 10)).validate(&w),
            Err(ActuatorError::Validate(m)) if m.contains("cannot exchange same tokens")
        ));
        assert!(matches!(
            MarketSellAssetActuator::new(&sell(&t, b"abc", 10, TRX, 10)).validate(&w),
            Err(ActuatorError::Validate(m)) if m.contains("sellTokenId is not a valid number")
        ));
    }

    #[test]
    fn token_conservation_across_match() {
        let mut w = ws();
        let (m, t) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100);
        set_account(&w, &t, 150, 0);

        // Total A and TRX across accounts + open-order escrow is invariant.
        let total_a = |w: &WorldState<MemoryStore>| -> i64 {
            tok(&w.get_account(&m).unwrap().unwrap(), A) + tok(&w.get_account(&t).unwrap().unwrap(), A)
                + open_escrow(w, A)
        };
        let total_trx = |w: &WorldState<MemoryStore>| -> i64 {
            w.get_account(&m).unwrap().unwrap().balance + w.get_account(&t).unwrap().unwrap().balance
                + open_escrow(w, TRX)
        };

        MarketSellAssetActuator::new(&sell(&m, A, 100, TRX, 100)).execute(&mut w).unwrap();
        assert_eq!(total_a(&w), 100);
        assert_eq!(total_trx(&w), 150);
        MarketSellAssetActuator::new(&sell(&t, TRX, 150, A, 150)).execute(&mut w).unwrap();
        assert_eq!(total_a(&w), 100);
        assert_eq!(total_trx(&w), 150);
    }

    /// Sum sell-token-remain of open orders escrowing `id`, across both pairs.
    fn open_escrow(w: &WorldState<MemoryStore>, id: &[u8]) -> i64 {
        let mut total = 0;
        for pair in [ob::pair_key(A, TRX), ob::pair_key(TRX, A)] {
            for price in ob::get_prices(w, &pair).unwrap() {
                for oid in ob::get_order_ids(w, &pair, price).unwrap() {
                    let o = ob::get_order(w, &oid).unwrap().unwrap();
                    if o.sell_token_id == id {
                        total += o.sell_token_quantity_remain;
                    }
                }
            }
        }
        total
    }
}
