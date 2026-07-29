//! On-chain Bancor DEX — `ExchangeCreateContract`, `ExchangeInjectContract`,
//! `ExchangeWithdrawContract`, and `ExchangeTransactionContract`.
//!
//! Semantics mirror java-tron's `ExchangeCreateActuator`, `ExchangeInjectActuator`,
//! `ExchangeWithdrawActuator`, and `ExchangeTransactionActuator`, plus the AMM
//! integer math in `ExchangeCapsule`/`ExchangeProcessor`.
//!
//! An exchange is a two-token liquidity pool (TRX/TRC10 or TRC10/TRC10). Records
//! live in the `"exchange"` column family, keyed by the 8-byte big-endian
//! exchange id (java-tron `ByteArray.fromLong`), value = prost `protocol.Exchange`.
//! The id counter is the dynamic property [`LATEST_EXCHANGE_NUM`].
//!
//! Token balances on an account: TRX (token id `"_"`, [`TRX_SYMBOL_BYTES`]) lives
//! in `Account.balance`; a TRC10 token lives in `Account.asset_v2`, keyed by the
//! token-id string (same convention as `asset_transfer`).
//!
//! **Create** — two distinct tokens, positive balances within the balance limit,
//! creator holds enough of each (and enough TRX for the create fee); debit the
//! creator, burn the fee, store the pool, bump the id counter.
//!
//! **Inject / Withdraw** — creator-only; adds/removes liquidity keeping the pool
//! ratio: the paired amount is `floor(otherBalance * quant / thisBalance)`.
//!
//! **Transaction** — the Bancor swap: sell `quant` of one token, receive
//! [`bancor_exchange`]`(...)` of the other; the caller must receive at least the
//! contract's `expected`. Pool balances and the caller's holdings update
//! accordingly.
//!
//! ### Bancor formula (mirrors `ExchangeProcessor`, non-hardened path)
//! With `supply = 1e18` (as an i64, converted to f64 only at each use):
//! ```text
//! exchangeToSupply(balance, quant):
//!     newBalance = balance + quant
//!     relay = (i64) ( -supply * (1 - (1 + quant/newBalance) ^ 0.0005) )
//!     supply += relay
//! exchangeFromSupply(balance, relay):
//!     supply -= relay                       // supply back to 1e18
//!     buy = (i64) ( balance * ((1 + relay/supply) ^ 2000 - 1) )
//! exchange(sellBal, buyBal, sellQuant) = exchangeFromSupply(buyBal,
//!                                            exchangeToSupply(sellBal, sellQuant))
//! ```
//! `pow` is IEEE-754 `f64` (Rust `powf` == Java `Math.pow`); the `(i64)` casts
//! truncate toward zero, matching Java's `(long)` cast.
//!
//! Deviations from java-tron (differences are data-only, documented here):
//! - Only the modern path is modelled: `allowSameTokenName == 1` (single store,
//!   TRC10 ids must be numeric), non-hardened arithmetic, `Math.pow` (not
//!   `StrictMath`). The legacy dual-store (`ExchangeStore`/`ExchangeV2Store`) and
//!   `resetTokenWithID` are not modelled.
//! - The withdraw "Not precise enough" check uses an `f64` approximation of
//!   java-tron's `BigDecimal` HALF_UP-to-4-decimals computation.
//! - No `AssetIssueStore`: a TRC10 token the account does not hold reads as a zero
//!   balance (rejected as "not enough"), rather than a missing-asset error.

use crate::{ActuatorError, ExecutionResult};
use prost::Message;
use tron_proto::protocol::{
    Account, Exchange, ExchangeCreateContract, ExchangeInjectContract,
    ExchangeTransactionContract, ExchangeWithdrawContract,
};
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// TRX token id (java-tron `ChainSymbol.TRX_SYMBOL_BYTES` = `"_"`).
pub const TRX_SYMBOL_BYTES: &[u8] = b"_";

/// Column family for exchange records (java-tron `ExchangeV2Store`).
/// Not yet present in `tron_state::cf`, so declared locally.
const CF_EXCHANGE: &str = tron_state::cf::EXCHANGE;

/// Dynamic-property key: monotonic exchange id counter.
pub const LATEST_EXCHANGE_NUM: &str = "LATEST_EXCHANGE_NUM";

/// Dynamic-property key: TRX fee to create an exchange.
pub const EXCHANGE_CREATE_FEE: &str = "EXCHANGE_CREATE_FEE";

/// java-tron genesis default exchange-create fee: 1024 TRX.
pub const DEFAULT_EXCHANGE_CREATE_FEE: i64 = 1_024_000_000;

/// Dynamic-property key: max per-token balance in an exchange.
pub const EXCHANGE_BALANCE_LIMIT: &str = "EXCHANGE_BALANCE_LIMIT";

/// java-tron genesis default exchange balance limit (1e15).
pub const DEFAULT_EXCHANGE_BALANCE_LIMIT: i64 = 1_000_000_000_000_000;

/// Bancor connector supply constant (`ExchangeCapsule.transaction`).
const BANCOR_SUPPLY: i64 = 1_000_000_000_000_000_000;

// -- helpers ----------------------------------------------------------------

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn storage_err(e: tron_storage::StorageError) -> ActuatorError {
    ActuatorError::State(e.to_string())
}

fn is_trx(id: &[u8]) -> bool {
    id == TRX_SYMBOL_BYTES
}

fn token_key(id: &[u8]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

/// java-tron `TransactionUtil.isNumber`: non-empty, all ASCII digits, no leading
/// zero (except the single-digit `"0"`).
fn is_number(id: &[u8]) -> bool {
    if id.is_empty() {
        return false;
    }
    if !id.iter().all(|b| b.is_ascii_digit()) {
        return false;
    }
    !(id.len() > 1 && id[0] == b'0')
}

fn exchange_key(id: i64) -> [u8; 8] {
    id.to_be_bytes()
}

fn get_exchange<S: KvStore>(
    state: &WorldState<S>,
    id: i64,
) -> Result<Option<Exchange>, ActuatorError> {
    match state.db.get(CF_EXCHANGE, &exchange_key(id)).map_err(storage_err)? {
        Some(bytes) => Ok(Some(
            Exchange::decode(bytes.as_slice()).map_err(|e| ActuatorError::State(e.to_string()))?,
        )),
        None => Ok(None),
    }
}

fn put_exchange<S: KvStore>(state: &mut WorldState<S>, ex: &Exchange) -> Result<(), ActuatorError> {
    state
        .db
        .put(CF_EXCHANGE, &exchange_key(ex.exchange_id), &ex.encode_to_vec())
        .map_err(storage_err)
}

fn create_fee<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
    let f = state.get_prop_i64(EXCHANGE_CREATE_FEE)?;
    Ok(if f > 0 { f } else { DEFAULT_EXCHANGE_CREATE_FEE })
}

fn balance_limit<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
    let l = state.get_prop_i64(EXCHANGE_BALANCE_LIMIT)?;
    Ok(if l > 0 { l } else { DEFAULT_EXCHANGE_BALANCE_LIMIT })
}

/// Account's balance of `token_id` (TRX from `balance`, TRC10 from `asset_v2`).
fn token_balance(account: &Account, id: &[u8]) -> i64 {
    if is_trx(id) {
        account.balance
    } else {
        account.asset_v2.get(&token_key(id)).copied().unwrap_or(0)
    }
}

/// Debit `amount` of `token_id` from `account` (checked, non-negative result).
fn debit_token(account: &mut Account, id: &[u8], amount: i64) -> Result<(), ActuatorError> {
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
fn credit_token(account: &mut Account, id: &[u8], amount: i64) -> Result<(), ActuatorError> {
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

/// `floor(other * quant / this)` in i128, rejecting out-of-range results
/// (java-tron `BigInteger…longValueExact`).
fn proportional(other: i64, quant: i64, this: i64) -> Result<i64, ActuatorError> {
    let v = (other as i128 * quant as i128) / this as i128;
    i64::try_from(v).map_err(|_| ActuatorError::Validate("long overflow".into()))
}

/// Bancor buy amount: sell `sell_quant` from a pool side of `sell_balance` into
/// the side of `buy_balance`. Mirrors `ExchangeProcessor.exchange`.
pub fn bancor_exchange(sell_balance: i64, buy_balance: i64, sell_quant: i64) -> i64 {
    let mut supply: i64 = BANCOR_SUPPLY;

    // exchangeToSupply(sell_balance, sell_quant)
    let new_balance = sell_balance + sell_quant;
    let issued_supply =
        -(supply as f64) * (1.0 - (1.0 + sell_quant as f64 / new_balance as f64).powf(0.0005));
    let relay = issued_supply as i64;
    supply += relay;

    // exchangeFromSupply(buy_balance, relay)
    supply -= relay;
    let exchange_balance =
        buy_balance as f64 * ((1.0 + relay as f64 / supply as f64).powf(2000.0) - 1.0);
    exchange_balance as i64
}

/// Apply a swap to a pool: returns `(buy_quant, new_first_balance,
/// new_second_balance)`. Mirrors `ExchangeCapsule.transaction` (non-hardened).
fn pool_transaction(ex: &Exchange, sell_id: &[u8], sell_quant: i64) -> (i64, i64, i64) {
    let (fb, sb) = (ex.first_token_balance, ex.second_token_balance);
    if ex.first_token_id.as_slice() == sell_id {
        let buy = bancor_exchange(fb, sb, sell_quant);
        (buy, fb + sell_quant, sb - buy)
    } else {
        let buy = bancor_exchange(sb, fb, sell_quant);
        (buy, fb - buy, sb + sell_quant)
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

pub struct ExchangeCreateActuator<'a> {
    contract: &'a ExchangeCreateContract,
}

impl<'a> ExchangeCreateActuator<'a> {
    pub fn new(contract: &'a ExchangeCreateContract) -> Self {
        Self { contract }
    }

    /// java-tron `ExchangeCreateActuator.validate`. Returns the fee (create fee).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("account[{}] not exists", hex(owner.as_bytes())))
        })?;

        let fee = create_fee(state)?;
        if account.balance < fee {
            return Err(ActuatorError::Validate(
                "No enough balance for exchange create fee!".into(),
            ));
        }

        let first_id = &self.contract.first_token_id;
        let second_id = &self.contract.second_token_id;
        let first_bal = self.contract.first_token_balance;
        let second_bal = self.contract.second_token_balance;

        if !is_trx(first_id) && !is_number(first_id) {
            return Err(ActuatorError::Validate("first token id is not a valid number".into()));
        }
        if !is_trx(second_id) && !is_number(second_id) {
            return Err(ActuatorError::Validate("second token id is not a valid number".into()));
        }

        if first_id == second_id {
            return Err(ActuatorError::Validate("cannot exchange same tokens".into()));
        }

        if first_bal <= 0 || second_bal <= 0 {
            return Err(ActuatorError::Validate("token balance must greater than zero".into()));
        }

        let limit = balance_limit(state)?;
        if first_bal > limit || second_bal > limit {
            return Err(ActuatorError::Validate(format!("token balance must less than {limit}")));
        }

        // First token holdings (TRX must also cover the fee).
        if is_trx(first_id) {
            if account.balance < first_bal + fee {
                return Err(ActuatorError::Validate("balance is not enough".into()));
            }
        } else if token_balance(&account, first_id) < first_bal {
            return Err(ActuatorError::Validate("first token balance is not enough".into()));
        }

        if is_trx(second_id) {
            if account.balance < second_bal + fee {
                return Err(ActuatorError::Validate("balance is not enough".into()));
            }
        } else if token_balance(&account, second_id) < second_bal {
            return Err(ActuatorError::Validate("second token balance is not enough".into()));
        }

        Ok(fee)
    }

    /// java-tron `ExchangeCreateActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let fee = create_fee(state)?;
        let first_id = &self.contract.first_token_id;
        let second_id = &self.contract.second_token_id;
        let first_bal = self.contract.first_token_balance;
        let second_bal = self.contract.second_token_balance;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        // Debit the create fee, then each token's contributed balance.
        account.balance = account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        debit_token(&mut account, first_id, first_bal)?;
        debit_token(&mut account, second_id, second_bal)?;
        state.put_account(&owner, &account)?;

        // Burn the fee (blackhole-optimization path).
        if fee > 0 {
            state.burn_trx(fee)?;
        }

        // Store the new pool and bump the id counter.
        let id = state.get_prop_i64(LATEST_EXCHANGE_NUM)? + 1;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let exchange = Exchange {
            exchange_id: id,
            creator_address: owner.as_bytes().to_vec(),
            create_time: now,
            first_token_id: first_id.clone(),
            first_token_balance: first_bal,
            second_token_id: second_id.clone(),
            second_token_balance: second_bal,
        };
        put_exchange(state, &exchange)?;
        state.put_prop_i64(LATEST_EXCHANGE_NUM, id)?;

        Ok(ExecutionResult { fee })
    }
}

// ---------------------------------------------------------------------------
// Shared load: owner account + pool + creator check
// ---------------------------------------------------------------------------

/// Load the owner account and the pool, verifying existence and (optionally)
/// creator ownership. `verb` shapes the fee error message.
fn load_owner_and_pool<S: KvStore>(
    state: &WorldState<S>,
    owner_bytes: &[u8],
    exchange_id: i64,
    verb: &str,
    require_creator: bool,
) -> Result<(Address, Account, Exchange), ActuatorError> {
    let owner = parse_address(owner_bytes)?;

    let account = state.get_account(&owner)?.ok_or_else(|| {
        ActuatorError::Validate(format!("account[{}] not exists", hex(owner.as_bytes())))
    })?;

    // fee is 0 for inject/withdraw/transaction; keep the check shape faithful.
    if account.balance < 0 {
        return Err(ActuatorError::Validate(format!(
            "No enough balance for exchange {verb} fee!"
        )));
    }

    let exchange = get_exchange(state, exchange_id)?
        .ok_or_else(|| ActuatorError::Validate(format!("Exchange[{exchange_id}] not exists")))?;

    if require_creator && exchange.creator_address.as_slice() != owner.as_bytes() {
        return Err(ActuatorError::Validate(format!(
            "account[{}] is not creator",
            hex(owner.as_bytes())
        )));
    }

    Ok((owner, account, exchange))
}

// ---------------------------------------------------------------------------
// Inject
// ---------------------------------------------------------------------------

pub struct ExchangeInjectActuator<'a> {
    contract: &'a ExchangeInjectContract,
}

impl<'a> ExchangeInjectActuator<'a> {
    pub fn new(contract: &'a ExchangeInjectContract) -> Self {
        Self { contract }
    }

    /// Compute the paired token id and amount for an inject/withdraw of `quant`
    /// of `token_id`. Returns `(another_id, another_quant)`.
    fn paired(ex: &Exchange, token_id: &[u8], quant: i64) -> Result<(Vec<u8>, i64), ActuatorError> {
        if token_id == ex.first_token_id.as_slice() {
            let q = proportional(ex.second_token_balance, quant, ex.first_token_balance)?;
            Ok((ex.second_token_id.clone(), q))
        } else {
            let q = proportional(ex.first_token_balance, quant, ex.second_token_balance)?;
            Ok((ex.first_token_id.clone(), q))
        }
    }

    /// java-tron `ExchangeInjectActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let (_, account, ex) = load_owner_and_pool(
            state,
            &self.contract.owner_address,
            self.contract.exchange_id,
            "inject",
            true,
        )?;

        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;

        if !is_trx(token_id) && !is_number(token_id) {
            return Err(ActuatorError::Validate("token id is not a valid number".into()));
        }
        if token_id != &ex.first_token_id && token_id != &ex.second_token_id {
            return Err(ActuatorError::Validate("token id is not in exchange".into()));
        }
        if ex.first_token_balance == 0 || ex.second_token_balance == 0 {
            return Err(ActuatorError::Validate(
                "Token balance in exchange is equal with 0,the exchange has been closed".into(),
            ));
        }
        if quant <= 0 {
            return Err(ActuatorError::Validate("injected token quant must greater than zero".into()));
        }

        let (another_id, another_quant) = Self::paired(&ex, token_id, quant)?;
        if another_quant <= 0 {
            return Err(ActuatorError::Validate(
                "the calculated token quant  must be greater than 0".into(),
            ));
        }

        // New pool balances against the limit.
        let (new_token_bal, new_another_bal) = if token_id == &ex.first_token_id {
            (ex.first_token_balance + quant, ex.second_token_balance + another_quant)
        } else {
            (ex.second_token_balance + quant, ex.first_token_balance + another_quant)
        };
        let limit = balance_limit(state)?;
        if new_token_bal > limit || new_another_bal > limit {
            return Err(ActuatorError::Validate(format!("token balance must less than {limit}")));
        }

        if token_balance(&account, token_id) < quant {
            return Err(ActuatorError::Validate(Self::not_enough(token_id, false)));
        }
        if token_balance(&account, &another_id) < another_quant {
            return Err(ActuatorError::Validate(Self::not_enough(&another_id, true)));
        }

        Ok(0)
    }

    fn not_enough(id: &[u8], another: bool) -> String {
        if is_trx(id) {
            "balance is not enough".into()
        } else if another {
            "another token balance is not enough".into()
        } else {
            "token balance is not enough".into()
        }
    }

    /// java-tron `ExchangeInjectActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;

        let mut ex = get_exchange(state, self.contract.exchange_id)?
            .ok_or_else(|| ActuatorError::Execute("exchange missing".into()))?;
        let (another_id, another_quant) = Self::paired(&ex, token_id, quant)?;

        if token_id == &ex.first_token_id {
            ex.first_token_balance += quant;
            ex.second_token_balance += another_quant;
        } else {
            ex.second_token_balance += quant;
            ex.first_token_balance += another_quant;
        }

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        debit_token(&mut account, token_id, quant)?;
        debit_token(&mut account, &another_id, another_quant)?;
        state.put_account(&owner, &account)?;
        put_exchange(state, &ex)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// Withdraw
// ---------------------------------------------------------------------------

pub struct ExchangeWithdrawActuator<'a> {
    contract: &'a ExchangeWithdrawContract,
}

impl<'a> ExchangeWithdrawActuator<'a> {
    pub fn new(contract: &'a ExchangeWithdrawContract) -> Self {
        Self { contract }
    }

    fn paired(ex: &Exchange, token_id: &[u8], quant: i64) -> Result<(Vec<u8>, i64), ActuatorError> {
        if token_id == ex.first_token_id.as_slice() {
            let q = proportional(ex.second_token_balance, quant, ex.first_token_balance)?;
            Ok((ex.second_token_id.clone(), q))
        } else {
            let q = proportional(ex.first_token_balance, quant, ex.second_token_balance)?;
            Ok((ex.first_token_id.clone(), q))
        }
    }

    /// java-tron `ExchangeWithdrawActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let (_, _, ex) = load_owner_and_pool(
            state,
            &self.contract.owner_address,
            self.contract.exchange_id,
            "withdraw",
            true,
        )?;

        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;

        if !is_trx(token_id) && !is_number(token_id) {
            return Err(ActuatorError::Validate("token id is not a valid number".into()));
        }
        if token_id != &ex.first_token_id && token_id != &ex.second_token_id {
            return Err(ActuatorError::Validate("token is not in exchange".into()));
        }
        if quant <= 0 {
            return Err(ActuatorError::Validate("withdraw token quant must greater than zero".into()));
        }
        if ex.first_token_balance == 0 || ex.second_token_balance == 0 {
            return Err(ActuatorError::Validate(
                "Token balance in exchange is equal with 0,the exchange has been closed".into(),
            ));
        }

        let (this_bal, other_bal) = if token_id == &ex.first_token_id {
            (ex.first_token_balance, ex.second_token_balance)
        } else {
            (ex.second_token_balance, ex.first_token_balance)
        };
        let another_quant = proportional(other_bal, quant, this_bal)?;
        if this_bal < quant || other_bal < another_quant {
            return Err(ActuatorError::Validate("exchange balance is not enough".into()));
        }
        if another_quant <= 0 {
            return Err(ActuatorError::Validate(
                "withdraw another token quant must greater than zero".into(),
            ));
        }

        // Precision check (non-hardened f64 path).
        let exact = other_bal as f64 * quant as f64 / this_bal as f64;
        let rounded = (exact * 10_000.0).round() / 10_000.0;
        let remainder = rounded - another_quant as f64;
        if remainder / another_quant as f64 > 0.0001 {
            return Err(ActuatorError::Validate("Not precise enough".into()));
        }

        Ok(0)
    }

    /// java-tron `ExchangeWithdrawActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;

        let mut ex = get_exchange(state, self.contract.exchange_id)?
            .ok_or_else(|| ActuatorError::Execute("exchange missing".into()))?;
        let (another_id, another_quant) = Self::paired(&ex, token_id, quant)?;

        if token_id == &ex.first_token_id {
            ex.first_token_balance -= quant;
            ex.second_token_balance -= another_quant;
        } else {
            ex.second_token_balance -= quant;
            ex.first_token_balance -= another_quant;
        }

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        credit_token(&mut account, token_id, quant)?;
        credit_token(&mut account, &another_id, another_quant)?;
        state.put_account(&owner, &account)?;
        put_exchange(state, &ex)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// Transaction (swap)
// ---------------------------------------------------------------------------

pub struct ExchangeTransactionActuator<'a> {
    contract: &'a ExchangeTransactionContract,
}

impl<'a> ExchangeTransactionActuator<'a> {
    pub fn new(contract: &'a ExchangeTransactionContract) -> Self {
        Self { contract }
    }

    /// java-tron `ExchangeTransactionActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let (_, account, ex) = load_owner_and_pool(
            state,
            &self.contract.owner_address,
            self.contract.exchange_id,
            "transaction",
            false,
        )?;

        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;
        let expected = self.contract.expected;

        if !is_trx(token_id) && !is_number(token_id) {
            return Err(ActuatorError::Validate("token id is not a valid number".into()));
        }
        if token_id != &ex.first_token_id && token_id != &ex.second_token_id {
            return Err(ActuatorError::Validate("token is not in exchange".into()));
        }
        if quant <= 0 {
            return Err(ActuatorError::Validate("token quant must greater than zero".into()));
        }
        if expected <= 0 {
            return Err(ActuatorError::Validate("token expected must greater than zero".into()));
        }
        if ex.first_token_balance == 0 || ex.second_token_balance == 0 {
            return Err(ActuatorError::Validate(
                "Token balance in exchange is equal with 0,the exchange has been closed".into(),
            ));
        }

        let token_bal = if token_id == &ex.first_token_id {
            ex.first_token_balance
        } else {
            ex.second_token_balance
        };
        let limit = balance_limit(state)?;
        if token_bal + quant > limit {
            return Err(ActuatorError::Validate(format!("token balance must less than {limit}")));
        }

        if token_balance(&account, token_id) < quant {
            let msg = if is_trx(token_id) {
                "balance is not enough"
            } else {
                "token balance is not enough"
            };
            return Err(ActuatorError::Validate(msg.into()));
        }

        let (buy, _, _) = pool_transaction(&ex, token_id, quant);
        if buy < expected {
            return Err(ActuatorError::Validate("token required must greater than expected".into()));
        }

        Ok(0)
    }

    /// java-tron `ExchangeTransactionActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let token_id = &self.contract.token_id;
        let quant = self.contract.quant;

        let mut ex = get_exchange(state, self.contract.exchange_id)?
            .ok_or_else(|| ActuatorError::Execute("exchange missing".into()))?;

        let (buy, new_first, new_second) = pool_transaction(&ex, token_id, quant);
        let another_id: Vec<u8> = if token_id == &ex.first_token_id {
            ex.second_token_id.clone()
        } else {
            ex.first_token_id.clone()
        };
        ex.first_token_balance = new_first;
        ex.second_token_balance = new_second;

        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        debit_token(&mut account, token_id, quant)?;
        credit_token(&mut account, &another_id, buy)?;
        state.put_account(&owner, &account)?;
        put_exchange(state, &ex)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const TOKEN_A: &[u8] = b"1000001";
    const TOKEN_B: &[u8] = b"1000002";

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// Account with a TRX balance and a set of TRC10 balances.
    fn account_with(owner: &Address, trx: i64, assets: &[(&[u8], i64)]) -> protocol::Account {
        let mut a = protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance: trx,
            ..Default::default()
        };
        for (id, bal) in assets {
            a.asset_v2.insert(token_key(id), *bal);
        }
        a
    }

    fn state_with(account: protocol::Account) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        let owner = Address::from_bytes(account.address.clone().try_into().unwrap()).unwrap();
        ws.put_account(&owner, &account).unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, 1_700_000_000_000).unwrap();
        ws
    }

    fn create_contract(
        owner: &Address,
        first_id: &[u8],
        first_bal: i64,
        second_id: &[u8],
        second_bal: i64,
    ) -> ExchangeCreateContract {
        ExchangeCreateContract {
            owner_address: owner.as_bytes().to_vec(),
            first_token_id: first_id.to_vec(),
            first_token_balance: first_bal,
            second_token_id: second_id.to_vec(),
            second_token_balance: second_bal,
        }
    }

    /// Create a TOKEN_A/TOKEN_B pool owned by `owner`, returning its id (1).
    fn seed_pool(
        ws: &mut WorldState<MemoryStore>,
        owner: &Address,
        a_bal: i64,
        b_bal: i64,
    ) -> i64 {
        let c = create_contract(owner, TOKEN_A, a_bal, TOKEN_B, b_bal);
        ExchangeCreateActuator::new(&c).execute(ws).unwrap();
        1
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn create_happy_path_debits_creator_and_stores_pool() {
        let o = addr(1);
        // Enough TRX for the fee, and TRC10 balances for both tokens.
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000), (TOKEN_B, 50_000)]);
        let mut ws = state_with(acct);

        let c = create_contract(&o, TOKEN_A, 60_000, TOKEN_B, 30_000);
        let a = ExchangeCreateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), DEFAULT_EXCHANGE_CREATE_FEE);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, DEFAULT_EXCHANGE_CREATE_FEE);

        // Pool stored with id 1, correct balances.
        assert_eq!(ws.get_prop_i64(LATEST_EXCHANGE_NUM).unwrap(), 1);
        let ex = get_exchange(&ws, 1).unwrap().unwrap();
        assert_eq!(ex.creator_address, o.as_bytes().to_vec());
        assert_eq!(ex.first_token_id, TOKEN_A.to_vec());
        assert_eq!(ex.first_token_balance, 60_000);
        assert_eq!(ex.second_token_id, TOKEN_B.to_vec());
        assert_eq!(ex.second_token_balance, 30_000);
        assert_eq!(ex.create_time, 1_700_000_000_000);

        // Creator debited: fee burned, tokens moved to pool.
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.balance, 0); // all TRX went to the fee
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap(), 40_000);
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_B)).copied().unwrap(), 20_000);
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), DEFAULT_EXCHANGE_CREATE_FEE);
    }

    #[test]
    fn create_second_pool_increments_id() {
        let o = addr(1);
        let acct = account_with(&o, 2 * DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000), (TOKEN_B, 100_000)]);
        let mut ws = state_with(acct);
        seed_pool(&mut ws, &o, 10_000, 10_000);
        let c = create_contract(&o, TOKEN_A, 20_000, TOKEN_B, 20_000);
        ExchangeCreateActuator::new(&c).execute(&mut ws).unwrap();
        assert_eq!(ws.get_prop_i64(LATEST_EXCHANGE_NUM).unwrap(), 2);
        assert!(get_exchange(&ws, 2).unwrap().is_some());
    }

    #[test]
    fn create_same_tokens_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000)]);
        let ws = state_with(acct);
        let c = create_contract(&o, TOKEN_A, 10_000, TOKEN_A, 10_000);
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("cannot exchange same tokens")
        ));
    }

    #[test]
    fn create_non_positive_balance_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000), (TOKEN_B, 100_000)]);
        let ws = state_with(acct);
        let c = create_contract(&o, TOKEN_A, 0, TOKEN_B, 10_000);
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("token balance must greater than zero")
        ));
    }

    #[test]
    fn create_insufficient_token_balance_rejected() {
        let o = addr(1);
        // Holds only 5_000 of TOKEN_A but tries to seed 60_000.
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 5_000), (TOKEN_B, 100_000)]);
        let ws = state_with(acct);
        let c = create_contract(&o, TOKEN_A, 60_000, TOKEN_B, 30_000);
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("first token balance is not enough")
        ));
    }

    #[test]
    fn create_insufficient_fee_rejected() {
        let o = addr(1);
        // Not enough TRX to cover the create fee.
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE - 1, &[(TOKEN_A, 100_000), (TOKEN_B, 100_000)]);
        let ws = state_with(acct);
        let c = create_contract(&o, TOKEN_A, 10_000, TOKEN_B, 10_000);
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("No enough balance for exchange create fee!")
        ));
    }

    #[test]
    fn create_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = create_contract(&addr(1), TOKEN_A, 10_000, TOKEN_B, 10_000);
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn create_malformed_address_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = ExchangeCreateContract {
            owner_address: vec![0x41; 20],
            first_token_id: TOKEN_A.to_vec(),
            first_token_balance: 10_000,
            second_token_id: TOKEN_B.to_vec(),
            second_token_balance: 10_000,
        };
        assert!(matches!(
            ExchangeCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    #[test]
    fn create_trx_pair_moves_trx_into_pool() {
        // TRX / TOKEN_A pool: verify the TRX side leaves the account balance.
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE + 500_000, &[(TOKEN_A, 100_000)]);
        let mut ws = state_with(acct);
        let c = create_contract(&o, TRX_SYMBOL_BYTES, 500_000, TOKEN_A, 40_000);
        let a = ExchangeCreateActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        let acct = ws.get_account(&o).unwrap().unwrap();
        // balance = start - fee - 500_000(TRX into pool) = 0
        assert_eq!(acct.balance, 0);
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap(), 60_000);
        let ex = get_exchange(&ws, 1).unwrap().unwrap();
        assert_eq!(ex.first_token_id, TRX_SYMBOL_BYTES.to_vec());
        assert_eq!(ex.first_token_balance, 500_000);
    }

    // -- inject ----------------------------------------------------------

    #[test]
    fn inject_proportional_adds_liquidity() {
        let o = addr(1);
        // Fee + pool seed (10k A / 20k B) + spare for injection.
        let acct = account_with(
            &o,
            DEFAULT_EXCHANGE_CREATE_FEE,
            &[(TOKEN_A, 20_000), (TOKEN_B, 40_000)],
        );
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 10_000, 20_000); // account now: A=10k, B=20k

        // Inject 5_000 of A -> another = floor(20000*5000/10000) = 10_000 of B.
        let c = ExchangeInjectContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 5_000,
        };
        let a = ExchangeInjectActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let ex = get_exchange(&ws, id).unwrap().unwrap();
        assert_eq!(ex.first_token_balance, 15_000);
        assert_eq!(ex.second_token_balance, 30_000);
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap(), 5_000);
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_B)).copied().unwrap(), 10_000);
    }

    #[test]
    fn inject_by_non_creator_rejected() {
        let creator = addr(1);
        let acct = account_with(&creator, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 20_000), (TOKEN_B, 40_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &creator, 10_000, 20_000);

        // A different account tries to inject.
        let other = addr(2);
        ws.put_account(&other, &account_with(&other, 0, &[(TOKEN_A, 10_000), (TOKEN_B, 10_000)])).unwrap();
        let c = ExchangeInjectContract {
            owner_address: other.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 1_000,
        };
        assert!(matches!(
            ExchangeInjectActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is not creator")
        ));
    }

    #[test]
    fn inject_wrong_token_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 20_000), (TOKEN_B, 40_000), (b"1000009", 10_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 10_000, 20_000);
        let c = ExchangeInjectContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: b"1000009".to_vec(),
            quant: 1_000,
        };
        assert!(matches!(
            ExchangeInjectActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("token id is not in exchange")
        ));
    }

    #[test]
    fn inject_nonexistent_exchange_rejected() {
        let o = addr(1);
        let ws = state_with(account_with(&o, 0, &[(TOKEN_A, 10_000)]));
        let c = ExchangeInjectContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: 1,
            token_id: TOKEN_A.to_vec(),
            quant: 1_000,
        };
        assert!(matches!(
            ExchangeInjectActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Exchange[1] not exists")
        ));
    }

    // -- withdraw --------------------------------------------------------

    #[test]
    fn withdraw_proportional_removes_liquidity() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 10_000), (TOKEN_B, 20_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 10_000, 20_000); // account tokens now 0/0

        // Withdraw 4_000 of A -> another = floor(20000*4000/10000) = 8_000 of B.
        let c = ExchangeWithdrawContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 4_000,
        };
        let a = ExchangeWithdrawActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let ex = get_exchange(&ws, id).unwrap().unwrap();
        assert_eq!(ex.first_token_balance, 6_000);
        assert_eq!(ex.second_token_balance, 12_000);
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap(), 4_000);
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_B)).copied().unwrap(), 8_000);
    }

    #[test]
    fn withdraw_by_non_creator_rejected() {
        let creator = addr(1);
        let acct = account_with(&creator, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 10_000), (TOKEN_B, 20_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &creator, 10_000, 20_000);
        let other = addr(2);
        ws.put_account(&other, &account_with(&other, 0, &[])).unwrap();
        let c = ExchangeWithdrawContract {
            owner_address: other.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 1_000,
        };
        assert!(matches!(
            ExchangeWithdrawActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is not creator")
        ));
    }

    #[test]
    fn withdraw_more_than_pool_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 10_000), (TOKEN_B, 20_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 10_000, 20_000);
        let c = ExchangeWithdrawContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 15_000, // more than the 10_000 in the pool
        };
        assert!(matches!(
            ExchangeWithdrawActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("exchange balance is not enough")
        ));
    }

    // -- transaction (swap) ---------------------------------------------

    #[test]
    fn transaction_swap_matches_bancor_and_conserves_tokens() {
        let o = addr(1);
        // Trader holds 50_000 of TOKEN_A to sell; pool is 100k A / 100k B.
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000 + 50_000), (TOKEN_B, 100_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 100_000, 100_000);
        // After seeding: account A = 50_000, B = 0. Pool: A=100k, B=100k.

        // Pre-swap per-token totals (account + pool).
        let pre_a = ws.get_account(&o).unwrap().unwrap().asset_v2.get(&token_key(TOKEN_A)).copied().unwrap()
            + get_exchange(&ws, id).unwrap().unwrap().first_token_balance;
        let pre_b = ws.get_account(&o).unwrap().unwrap().asset_v2.get(&token_key(TOKEN_B)).copied().unwrap_or(0)
            + get_exchange(&ws, id).unwrap().unwrap().second_token_balance;

        // Sell 10_000 of A. Hand/independently computed bancor result = 9090.
        assert_eq!(bancor_exchange(100_000, 100_000, 10_000), 9090);
        let c = ExchangeTransactionContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 10_000,
            expected: 9_000,
        };
        let a = ExchangeTransactionActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        // Pool updated: A += 10_000, B -= 9_090.
        let ex = get_exchange(&ws, id).unwrap().unwrap();
        assert_eq!(ex.first_token_balance, 110_000);
        assert_eq!(ex.second_token_balance, 90_910);
        // Trader: A -= 10_000 (50k -> 40k), B += 9_090.
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap(), 40_000);
        assert_eq!(acct.asset_v2.get(&token_key(TOKEN_B)).copied().unwrap(), 9_090);

        // Conservation: per-token (account + pool) totals unchanged.
        let post_a = acct.asset_v2.get(&token_key(TOKEN_A)).copied().unwrap() + ex.first_token_balance;
        let post_b = acct.asset_v2.get(&token_key(TOKEN_B)).copied().unwrap() + ex.second_token_balance;
        assert_eq!(post_a, pre_a);
        assert_eq!(post_b, pre_b);
    }

    #[test]
    fn transaction_wrong_token_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000), (TOKEN_B, 100_000), (b"1000009", 10_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 100_000, 100_000);
        let c = ExchangeTransactionContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: b"1000009".to_vec(),
            quant: 1_000,
            expected: 1,
        };
        assert!(matches!(
            ExchangeTransactionActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("token is not in exchange")
        ));
    }

    #[test]
    fn transaction_below_expected_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000 + 50_000), (TOKEN_B, 100_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 100_000, 100_000);
        // Selling 10_000 yields 9090; expecting 9_500 must be rejected.
        let c = ExchangeTransactionContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 10_000,
            expected: 9_500,
        };
        assert!(matches!(
            ExchangeTransactionActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("token required must greater than expected")
        ));
    }

    #[test]
    fn transaction_non_positive_quant_rejected() {
        let o = addr(1);
        let acct = account_with(&o, DEFAULT_EXCHANGE_CREATE_FEE, &[(TOKEN_A, 100_000), (TOKEN_B, 100_000)]);
        let mut ws = state_with(acct);
        let id = seed_pool(&mut ws, &o, 100_000, 100_000);
        let c = ExchangeTransactionContract {
            owner_address: o.as_bytes().to_vec(),
            exchange_id: id,
            token_id: TOKEN_A.to_vec(),
            quant: 0,
            expected: 1,
        };
        assert!(matches!(
            ExchangeTransactionActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("token quant must greater than zero")
        ));
    }

    #[test]
    fn bancor_asymmetric_pool_known_value() {
        // Independently computed: 1_000_000/500_000 pool, sell 100_000 -> 45454.
        assert_eq!(bancor_exchange(1_000_000, 500_000, 100_000), 45_454);
    }
}
