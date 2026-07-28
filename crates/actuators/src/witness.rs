//! Witness (super-representative) lifecycle — `WitnessCreateContract` and
//! `WitnessUpdateContract`.
//!
//! Semantics mirror java-tron's `WitnessCreateActuator` and
//! `WitnessUpdateActuator`.
//!
//! Witnesses are stored in the `WITNESS` column family, keyed by the 21-byte
//! owner address, value = prost-encoded `protocol.Witness` (same layout the
//! proposal actuators read for witness-membership checks).
//!
//! **Create — validate** — owner address valid; url is 1..=256 bytes
//! (java-tron `TransactionUtil.validUrl`: non-empty, `<= MAX_URL_LEN`); owner
//! account exists; owner is not already a witness; owner balance is at least the
//! account-upgrade cost ([`ACCOUNT_UPGRADE_COST`] dynamic property, defaulting to
//! [`DEFAULT_ACCOUNT_UPGRADE_COST`]).
//!
//! **Create — execute** — store a `Witness { address, url, is_jobs: true }`,
//! mark the owner account `is_witness = true`, debit the cost from the owner,
//! and burn it (blackhole-optimization path). The fee returned equals the cost
//! (java-tron `WitnessCreateActuator.calcFee` returns the upgrade cost).
//!
//! **Update — validate** — owner valid; owner account exists; url valid; owner
//! is a witness.
//!
//! **Update — execute** — overwrite the stored witness's `url` only; `address`
//! and `is_jobs` are untouched. Fee is 0.
//!
//! Deviations from java-tron (differences are data-only, documented here):
//! - `is_jobs` is set to `true` on create per the task contract; java-tron's
//!   3-arg `WitnessCapsule(address, voteCount, url)` leaves `is_jobs` at its
//!   proto default (`false`).
//! - Account-upgrade cost falls back to [`DEFAULT_ACCOUNT_UPGRADE_COST`] when the
//!   dynamic property is unset; java-tron initialises the property at genesis, so
//!   the effective default matches.
//! - Multi-sign default-permission setup (`getAllowMultiSign`), the non-blackhole
//!   fee path, and `addTotalCreateWitnessCost` bookkeeping are not modelled — the
//!   cost is always burned via [`WorldState::burn_trx`].

use crate::{ActuatorError, ExecutionResult};
use prost::Message;
use tron_proto::protocol::Witness;
use tron_proto::protocol::{WitnessCreateContract, WitnessUpdateContract};
use tron_state::{cf, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Dynamic-property key: cost (in sun) to upgrade an account to a witness.
pub const ACCOUNT_UPGRADE_COST: &str = "ACCOUNT_UPGRADE_COST";

/// java-tron genesis default account-upgrade cost: 9999 TRX.
pub const DEFAULT_ACCOUNT_UPGRADE_COST: i64 = 9_999_000_000;

/// java-tron `TransactionUtil.MAX_URL_LEN`.
pub const MAX_URL_LEN: usize = 256;

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

/// java-tron `TransactionUtil.validUrl`: non-empty and no longer than the max.
fn valid_url(url: &[u8]) -> bool {
    !url.is_empty() && url.len() <= MAX_URL_LEN
}

fn storage_err(e: tron_storage::StorageError) -> ActuatorError {
    ActuatorError::State(e.to_string())
}

fn get_witness<S: KvStore>(
    state: &WorldState<S>,
    addr: &Address,
) -> Result<Option<Witness>, ActuatorError> {
    match state.db.get(cf::WITNESS, addr.as_bytes()).map_err(storage_err)? {
        Some(bytes) => Ok(Some(
            Witness::decode(bytes.as_slice()).map_err(|e| ActuatorError::State(e.to_string()))?,
        )),
        None => Ok(None),
    }
}

fn put_witness<S: KvStore>(state: &mut WorldState<S>, witness: &Witness) -> Result<(), ActuatorError> {
    state
        .db
        .put(cf::WITNESS, &witness.address, &witness.encode_to_vec())
        .map_err(storage_err)
}

fn witness_exists<S: KvStore>(state: &WorldState<S>, addr: &Address) -> Result<bool, ActuatorError> {
    state.db.exists(cf::WITNESS, addr.as_bytes()).map_err(storage_err)
}

fn account_upgrade_cost<S: KvStore>(state: &WorldState<S>) -> Result<i64, ActuatorError> {
    let cost = state.get_prop_i64(ACCOUNT_UPGRADE_COST)?;
    Ok(if cost > 0 { cost } else { DEFAULT_ACCOUNT_UPGRADE_COST })
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

pub struct WitnessCreateActuator<'a> {
    contract: &'a WitnessCreateContract,
}

impl<'a> WitnessCreateActuator<'a> {
    pub fn new(contract: &'a WitnessCreateContract) -> Self {
        Self { contract }
    }

    /// java-tron `WitnessCreateActuator.validate`. Returns the fee (the upgrade
    /// cost), matching `calcFee`.
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !valid_url(&self.contract.url) {
            return Err(ActuatorError::Validate("Invalid url".into()));
        }

        let account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("account[{}] not exists", hex(owner.as_bytes())))
        })?;

        if witness_exists(state, &owner)? {
            return Err(ActuatorError::Validate(format!(
                "Witness[{}] has existed",
                hex(owner.as_bytes())
            )));
        }

        let cost = account_upgrade_cost(state)?;
        if account.balance < cost {
            return Err(ActuatorError::Validate("balance < AccountUpgradeCost".into()));
        }

        Ok(cost)
    }

    /// java-tron `WitnessCreateActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let cost = account_upgrade_cost(state)?;

        // Store the new witness.
        let witness = Witness {
            address: owner.as_bytes().to_vec(),
            url: String::from_utf8_lossy(&self.contract.url).into_owned(),
            is_jobs: true,
            ..Default::default()
        };
        put_witness(state, &witness)?;

        // Mark the account as a witness and debit the cost.
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.is_witness = true;
        account.balance = account
            .balance
            .checked_sub(cost)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        state.put_account(&owner, &account)?;

        // Burn the cost (blackhole-optimization path).
        state.burn_trx(cost)?;

        Ok(ExecutionResult { fee: cost })
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub struct WitnessUpdateActuator<'a> {
    contract: &'a WitnessUpdateContract,
}

impl<'a> WitnessUpdateActuator<'a> {
    pub fn new(contract: &'a WitnessUpdateContract) -> Self {
        Self { contract }
    }

    /// java-tron `WitnessUpdateActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate("account does not exist".into()));
        }

        if !valid_url(&self.contract.update_url) {
            return Err(ActuatorError::Validate("Invalid url".into()));
        }

        if !witness_exists(state, &owner)? {
            return Err(ActuatorError::Validate("Witness does not exist".into()));
        }

        Ok(0)
    }

    /// java-tron `WitnessUpdateActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        let mut witness = get_witness(state, &owner)?
            .ok_or_else(|| ActuatorError::Execute("Witness does not exist".into()))?;
        witness.url = String::from_utf8_lossy(&self.contract.update_url).into_owned();
        put_witness(state, &witness)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// Fresh state with `owner` as an existing account holding `balance`.
    fn seeded_state(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance,
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        ws
    }

    fn create_contract(owner: &Address, url: &[u8]) -> WitnessCreateContract {
        WitnessCreateContract {
            owner_address: owner.as_bytes().to_vec(),
            url: url.to_vec(),
        }
    }

    fn update_contract(owner: &Address, url: &[u8]) -> WitnessUpdateContract {
        WitnessUpdateContract {
            owner_address: owner.as_bytes().to_vec(),
            update_url: url.to_vec(),
        }
    }

    /// Put a witness record for `owner` directly.
    fn seed_witness(ws: &mut WorldState<MemoryStore>, owner: &Address, url: &str, is_jobs: bool) {
        let w = Witness {
            address: owner.as_bytes().to_vec(),
            url: url.to_string(),
            is_jobs,
            ..Default::default()
        };
        put_witness(ws, &w).unwrap();
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn create_happy_path_stores_witness_and_burns_cost() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 500_000_000);
        ws.put_prop_i64(ACCOUNT_UPGRADE_COST, 100_000_000).unwrap();

        let c = create_contract(&o, b"http://sr.example.com");
        let a = WitnessCreateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 100_000_000);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 100_000_000);

        // Witness stored with url + is_jobs.
        let w = get_witness(&ws, &o).unwrap().unwrap();
        assert_eq!(w.address, o.as_bytes().to_vec());
        assert_eq!(w.url, "http://sr.example.com");
        assert!(w.is_jobs);

        // Account flagged and debited.
        let acct = ws.get_account(&o).unwrap().unwrap();
        assert!(acct.is_witness);
        assert_eq!(acct.balance, 400_000_000);

        // Cost was burned; balance + burn is conserved.
        let burned = ws.get_prop_i64(tron_state::props::BURN_TRX_AMOUNT).unwrap();
        assert_eq!(burned, 100_000_000);
        assert_eq!(acct.balance + burned, 500_000_000);
    }

    #[test]
    fn create_uses_default_cost_when_prop_unset() {
        let o = addr(1);
        // No ACCOUNT_UPGRADE_COST prop => default 9999 TRX applies.
        let mut ws = seeded_state(&o, DEFAULT_ACCOUNT_UPGRADE_COST);
        let c = create_contract(&o, b"url");
        let a = WitnessCreateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), DEFAULT_ACCOUNT_UPGRADE_COST);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 0);

        // One sun short must be rejected.
        let o2 = addr(2);
        let ws2 = seeded_state(&o2, DEFAULT_ACCOUNT_UPGRADE_COST - 1);
        let c2 = create_contract(&o2, b"url");
        assert!(matches!(
            WitnessCreateActuator::new(&c2).validate(&ws2),
            Err(ActuatorError::Validate(m)) if m.contains("balance < AccountUpgradeCost")
        ));
    }

    #[test]
    fn create_duplicate_witness_rejected() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 500_000_000);
        ws.put_prop_i64(ACCOUNT_UPGRADE_COST, 100_000_000).unwrap();
        seed_witness(&mut ws, &o, "existing", true);

        let c = create_contract(&o, b"http://new.example.com");
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("has existed")
        ));
    }

    #[test]
    fn create_url_empty_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 500_000_000);
        let c = create_contract(&o, b"");
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid url")
        ));
    }

    #[test]
    fn create_url_too_long_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 500_000_000);
        ws.put_prop_i64(ACCOUNT_UPGRADE_COST, 100_000_000).unwrap();
        let c = create_contract(&o, &vec![b'a'; MAX_URL_LEN + 1]); // 257 bytes
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid url")
        ));
        // Exactly 256 bytes is accepted (the url-length boundary, not balance).
        let ok = create_contract(&o, &vec![b'a'; MAX_URL_LEN]);
        assert_eq!(WitnessCreateActuator::new(&ok).validate(&ws).unwrap(), 100_000_000);
    }

    #[test]
    fn create_insufficient_balance_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 50_000_000);
        ws.put_prop_i64(ACCOUNT_UPGRADE_COST, 100_000_000).unwrap();
        let c = create_contract(&o, b"url");
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("balance < AccountUpgradeCost")
        ));
    }

    #[test]
    fn create_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = create_contract(&addr(1), b"url");
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn create_malformed_address_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = WitnessCreateContract {
            owner_address: vec![0x41; 20], // 20 bytes, not 21
            url: b"url".to_vec(),
        };
        assert!(matches!(
            WitnessCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    // -- update ----------------------------------------------------------

    #[test]
    fn update_happy_path_changes_url() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 500_000_000);
        seed_witness(&mut ws, &o, "http://old.example.com", true);

        let c = update_contract(&o, b"http://new.example.com");
        let a = WitnessUpdateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let w = get_witness(&ws, &o).unwrap().unwrap();
        assert_eq!(w.url, "http://new.example.com");
        // address and is_jobs untouched.
        assert_eq!(w.address, o.as_bytes().to_vec());
        assert!(w.is_jobs);
    }

    #[test]
    fn update_by_non_witness_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, 500_000_000); // account exists, no witness record
        let c = update_contract(&o, b"http://new.example.com");
        assert!(matches!(
            WitnessUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Witness does not exist")
        ));
    }

    #[test]
    fn update_bad_url_rejected() {
        let o = addr(1);
        let mut ws = seeded_state(&o, 500_000_000);
        seed_witness(&mut ws, &o, "http://old.example.com", true);
        // Empty url.
        let c = update_contract(&o, b"");
        assert!(matches!(
            WitnessUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid url")
        ));
        // Too-long url.
        let c = update_contract(&o, &vec![b'a'; MAX_URL_LEN + 1]);
        assert!(matches!(
            WitnessUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid url")
        ));
    }

    #[test]
    fn update_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = update_contract(&addr(1), b"http://new.example.com");
        assert!(matches!(
            WitnessUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("account does not exist")
        ));
    }

    #[test]
    fn update_malformed_address_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = WitnessUpdateContract {
            owner_address: vec![0x41; 20],
            update_url: b"url".to_vec(),
        };
        assert!(matches!(
            WitnessUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }
}
