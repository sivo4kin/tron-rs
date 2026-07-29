//! `MarketCancelOrderContract` — cancel a resting order and refund its unfilled
//! remainder. Port of java-tron `MarketCancelOrderActuator`.
//!
//! **validate** — market feature on (TODO feature-gate); owner exists; the order
//! exists, is `ACTIVE`, and belongs to the owner; owner covers the cancel fee.
//! **execute** — charge + burn the fee, refund `sell_token_quantity_remain` to
//! the owner (TRX or `asset_v2`), mark the order `CANCELED`, and delist it from
//! its price level (removing the level, then the pair, when they empty).
//!
//! Deviations from java-tron: gated on `ALLOW_MARKET_TRANSACTION`
//! (java `supportAllowMarketTransaction`); fee burned via `burn_trx`.

use crate::market::order_book as ob;
use crate::market::{credit_token, MARKET_CANCEL_FEE};
use crate::{require_feature, ActuatorError, ExecutionResult};
use tron_proto::protocol::market_order::State;
use tron_proto::protocol::MarketCancelOrderContract;
use tron_state::features::flags;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

fn order_id(bytes: &[u8]) -> Result<ob::OrderId, ActuatorError> {
    bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("orderId not exists".into()))
}

pub struct MarketCancelOrderActuator<'a> {
    contract: &'a MarketCancelOrderContract,
}

impl<'a> MarketCancelOrderActuator<'a> {
    pub fn new(contract: &'a MarketCancelOrderContract) -> Self {
        Self { contract }
    }

    fn fee<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
        Ok(state.get_prop_i64(MARKET_CANCEL_FEE)?.max(0))
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        require_feature(
            state,
            flags::ALLOW_MARKET_TRANSACTION,
            "Not support Market Transaction, need to be opened by the committee",
        )?;

        let owner = parse_address(&self.contract.owner_address)?;

        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist!".into()))?;

        let id = order_id(&self.contract.order_id)?;
        let order = ob::get_order(state, &id)?
            .ok_or_else(|| ActuatorError::Validate("orderId not exists".into()))?;

        if order.state != State::Active as i32 {
            return Err(ActuatorError::Validate("Order is not active!".into()));
        }
        if order.owner_address.as_slice() != owner.as_bytes() {
            return Err(ActuatorError::Validate("Order does not belong to the account!".into()));
        }

        let fee = Self::fee(state)?;
        if account.balance < fee {
            return Err(ActuatorError::Validate("No enough balance !".into()));
        }

        Ok(fee)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let fee = Self::fee(state)?;
        let id = order_id(&self.contract.order_id)?;

        let mut order = ob::get_order(state, &id)?
            .ok_or_else(|| ActuatorError::Execute("orderId not exists".into()))?;

        // Fee + refund the unfilled remainder to the owner.
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.balance = account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;

        let refund = order.sell_token_quantity_remain;
        credit_token(&mut account, &order.sell_token_id, refund)?;
        order.sell_token_quantity_return += refund;
        order.sell_token_quantity_remain = 0;
        order.state = State::Canceled as i32;

        state.put_account(&owner, &account)?;
        ob::put_order(state, &order)?;
        if fee > 0 {
            state.burn_trx(fee)?;
        }

        // Delist from the price level (remove the level/pair when they empty).
        let pair = ob::pair_key(&order.sell_token_id, &order.buy_token_id);
        let price = (order.sell_token_quantity, order.buy_token_quantity);
        let mut ids = ob::get_order_ids(state, &pair, price)?;
        ids.retain(|x| *x != id);
        ob::put_order_ids(state, &pair, price, &ids)?;
        if ids.is_empty() {
            ob::remove_price(state, &pair, price)?; // also drops the pair if its book empties
        }
        // Drop from the owner's secondary index (A11).
        ob::remove_account_order(state, &order.owner_address, id)?;

        Ok(ExecutionResult { fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::sell::MarketSellAssetActuator;
    use crate::market::token_balance;
    use tron_proto::protocol;
    use tron_proto::protocol::MarketSellAssetContract;
    use tron_state::props;
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

    fn sell(owner: &Address, sid: &[u8], sq: i64, bid: &[u8], bq: i64) -> MarketSellAssetContract {
        MarketSellAssetContract {
            owner_address: owner.as_bytes().to_vec(),
            sell_token_id: sid.to_vec(),
            sell_token_quantity: sq,
            buy_token_id: bid.to_vec(),
            buy_token_quantity: bq,
        }
    }

    /// Place a sell order and return its id (the only order id booked at `price`).
    fn place(w: &mut WorldState<MemoryStore>, c: &MarketSellAssetContract) -> ob::OrderId {
        MarketSellAssetActuator::new(c).execute(w).unwrap();
        let pair = ob::pair_key(&c.sell_token_id, &c.buy_token_id);
        let price = (c.sell_token_quantity, c.buy_token_quantity);
        *ob::get_order_ids(w, &pair, price).unwrap().last().unwrap()
    }

    fn cancel(owner: &Address, id: ob::OrderId) -> MarketCancelOrderContract {
        MarketCancelOrderContract { owner_address: owner.as_bytes().to_vec(), order_id: id.to_vec() }
    }

    #[test]
    fn cancel_refunds_remainder_and_delists() {
        let mut w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 100);
        let id = place(&mut w, &sell(&m, A, 100, TRX, 100));
        // escrowed: A balance 0.
        assert_eq!(token_balance(&w.get_account(&m).unwrap().unwrap(), A), 0);

        let c = cancel(&m, id);
        assert_eq!(MarketCancelOrderActuator::new(&c).validate(&w).unwrap(), 0);
        MarketCancelOrderActuator::new(&c).execute(&mut w).unwrap();

        // Full remainder refunded, order CANCELED, level delisted.
        assert_eq!(token_balance(&w.get_account(&m).unwrap().unwrap(), A), 100);
        let order = ob::get_order(&w, &id).unwrap().unwrap();
        assert_eq!(order.state, State::Canceled as i32);
        assert_eq!(order.sell_token_quantity_remain, 0);
        assert_eq!(ob::best_price(&w, &ob::pair_key(A, TRX)).unwrap(), None);
    }

    #[test]
    fn cancel_prunes_account_index_and_pair_list() {
        let mut w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 200);
        // Two resting orders on the same pair.
        let id1 = place(&mut w, &sell(&m, A, 100, TRX, 100));
        let id2 = place(&mut w, &sell(&m, A, 100, TRX, 90));
        assert_eq!(ob::get_account_orders(&w, m.as_bytes()).unwrap().len(), 2);
        assert_eq!(ob::get_pairs(&w).unwrap(), vec![ob::pair_key(A, TRX)]);

        // Cancel one: it drops from the owner index; the pair remains (still has id2).
        MarketCancelOrderActuator::new(&cancel(&m, id1)).execute(&mut w).unwrap();
        assert_eq!(ob::get_account_orders(&w, m.as_bytes()).unwrap(), vec![id2]);
        assert_eq!(ob::get_pairs(&w).unwrap().len(), 1);

        // Cancel the last: owner index empty and the pair leaves the all-pairs list.
        MarketCancelOrderActuator::new(&cancel(&m, id2)).execute(&mut w).unwrap();
        assert!(ob::get_account_orders(&w, m.as_bytes()).unwrap().is_empty());
        assert!(ob::get_pairs(&w).unwrap().is_empty());
    }

    #[test]
    fn refund_equals_residual_after_partial_fill() {
        let mut w = ws();
        let (m, t) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100);
        set_account(&w, &t, 40, 0);
        let id = place(&mut w, &sell(&m, A, 100, TRX, 100));
        // Taker buys 40 A for 40 TRX -> maker remainder 60 A.
        MarketSellAssetActuator::new(&sell(&t, TRX, 40, A, 40)).execute(&mut w).unwrap();
        let residual = ob::get_order(&w, &id).unwrap().unwrap().sell_token_quantity_remain;
        assert_eq!(residual, 60);

        // Maker got 40 TRX from the fill; cancel refunds the 60 A residual.
        let before_a = token_balance(&w.get_account(&m).unwrap().unwrap(), A);
        MarketCancelOrderActuator::new(&cancel(&m, id)).execute(&mut w).unwrap();
        let after_a = token_balance(&w.get_account(&m).unwrap().unwrap(), A);
        assert_eq!(after_a - before_a, residual);
        assert_eq!(w.get_account(&m).unwrap().unwrap().balance, 40); // filled proceeds kept
    }

    #[test]
    fn rejects_unknown_order() {
        let w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 100);
        let c = cancel(&m, [7u8; 32]);
        assert!(matches!(
            MarketCancelOrderActuator::new(&c).validate(&w),
            Err(ActuatorError::Validate(msg)) if msg.contains("orderId not exists")
        ));
    }

    #[test]
    fn rejects_already_inactive_order() {
        let mut w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 100);
        let id = place(&mut w, &sell(&m, A, 100, TRX, 100));
        MarketCancelOrderActuator::new(&cancel(&m, id)).execute(&mut w).unwrap();
        // Second cancel: order now CANCELED.
        assert!(matches!(
            MarketCancelOrderActuator::new(&cancel(&m, id)).validate(&w),
            Err(ActuatorError::Validate(msg)) if msg.contains("Order is not active!")
        ));
    }

    #[test]
    fn rejects_non_owner() {
        let mut w = ws();
        let (m, other) = (addr(1), addr(2));
        set_account(&w, &m, 0, 100);
        set_account(&w, &other, 0, 0);
        let id = place(&mut w, &sell(&m, A, 100, TRX, 100));
        assert!(matches!(
            MarketCancelOrderActuator::new(&cancel(&other, id)).validate(&w),
            Err(ActuatorError::Validate(msg)) if msg.contains("does not belong to the account")
        ));
    }

    #[test]
    fn rejects_when_market_feature_disabled() {
        let mut w = ws();
        let m = addr(1);
        set_account(&w, &m, 0, 100);
        let id = place(&mut w, &sell(&m, A, 100, TRX, 100));
        w.put_prop_i64(flags::ALLOW_MARKET_TRANSACTION, 0).unwrap();
        assert!(matches!(
            MarketCancelOrderActuator::new(&cancel(&m, id)).validate(&w),
            Err(ActuatorError::Validate(msg)) if msg.contains("Not support Market Transaction")
        ));
    }
}
