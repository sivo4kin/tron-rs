//! On-chain DEX (Stake-independent) — `MarketSellAssetContract` order placement
//! and matching. Split to keep each file < 500 lines:
//! - [`price`]: exact price-comparison / fill integer math (java `MarketUtils`).
//! - [`order_book`]: explicit index records for the order book (no iteration).
//! - [`sell`]: the [`sell::MarketSellAssetActuator`].
//!
//! Token ids are ascii: `"_"` ([`TRX_SYMBOL_BYTES`]) is TRX (balance in
//! `Account.balance`); a numeric id is TRC10 (balance in `Account.asset_v2`,
//! keyed by the id string — same convention as `exchange`/`asset_transfer`).

pub mod cancel;
pub mod order_book;
pub mod price;
pub mod sell;

use crate::ActuatorError;
use tron_proto::protocol::Account;

/// TRX token id (java-tron `"_"`).
pub const TRX_SYMBOL_BYTES: &[u8] = b"_";

/// Dynamic-property key: monotonic market order counter (for order ids).
pub const MARKET_ORDER_NUM: &str = "MARKET_ORDER_NUM";
/// Dynamic-property key: TRX fee to place a market sell order.
pub const MARKET_SELL_FEE: &str = "MARKET_SELL_FEE";
/// Dynamic-property key: TRX fee to cancel a market order.
pub const MARKET_CANCEL_FEE: &str = "MARKET_CANCEL_FEE";
/// Dynamic-property key: max sell/buy quantity per order.
pub const MARKET_QUANTITY_LIMIT: &str = "MARKET_QUANTITY_LIMIT";
/// java-tron genesis default `getMarketQuantityLimit()`.
pub const DEFAULT_MARKET_QUANTITY_LIMIT: i64 = 1_000_000_000_000_000;
/// java-tron `MarketSellAssetActuator.MAX_MATCH_NUM`.
pub const MAX_MATCH_NUM: u32 = 20;

pub fn is_trx(id: &[u8]) -> bool {
    id == TRX_SYMBOL_BYTES
}

fn token_key(id: &[u8]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

/// java-tron `isNumber`: non-empty ascii digits, no leading zero (except `"0"`).
pub fn is_number(id: &[u8]) -> bool {
    !id.is_empty() && id.iter().all(|b| b.is_ascii_digit()) && !(id.len() > 1 && id[0] == b'0')
}

/// Account balance of `token_id` (TRX from `balance`, TRC10 from `asset_v2`).
pub fn token_balance(account: &Account, id: &[u8]) -> i64 {
    if is_trx(id) {
        account.balance
    } else {
        account.asset_v2.get(&token_key(id)).copied().unwrap_or(0)
    }
}

/// Debit `amount` of `token_id` from `account` (checked, non-negative).
pub fn debit_token(account: &mut Account, id: &[u8], amount: i64) -> Result<(), ActuatorError> {
    if is_trx(id) {
        account.balance = account
            .balance
            .checked_sub(amount)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
    } else {
        let key = token_key(id);
        let bal = account.asset_v2.get(&key).copied().unwrap_or(0);
        let nb = bal
            .checked_sub(amount)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("asset balance is not sufficient".into()))?;
        account.asset_v2.insert(key, nb);
    }
    Ok(())
}

/// Credit `amount` of `token_id` to `account` (checked).
pub fn credit_token(account: &mut Account, id: &[u8], amount: i64) -> Result<(), ActuatorError> {
    if is_trx(id) {
        account.balance = account
            .balance
            .checked_add(amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
    } else {
        let key = token_key(id);
        let bal = account.asset_v2.get(&key).copied().unwrap_or(0);
        let nb = bal
            .checked_add(amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        account.asset_v2.insert(key, nb);
    }
    Ok(())
}
