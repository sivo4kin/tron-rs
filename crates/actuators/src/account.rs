//! Account actuators: create, update (name), set-account-id.
//!
//! Mirrors java-tron:
//! - `CreateAccountActuator` — owner pays `CREATE_ACCOUNT_FEE` (default 100_000
//!   sun) to create a not-yet-existing target account; the fee is burned
//!   (blackhole-optimization path).
//! - `UpdateAccountActuator` — sets `account_name` (<= 200 bytes; empty allowed by
//!   `validAccountName`); re-setting an already-set name is rejected unless the
//!   `ALLOW_UPDATE_ACCOUNT_NAME` committee property is on.
//! - `SetAccountIdActuator` — sets `account_id` once; 8..=32 bytes, every byte
//!   readable (0x21..=0x7E per java-tron `validReadableBytes`).
//!
//! Deviations (documented): the global `AccountIndexStore`/`AccountIdIndexStore`
//! uniqueness indexes are not modeled yet — only per-account rules are enforced.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::{AccountCreateContract, AccountUpdateContract, SetAccountIdContract};
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Dynamic-property key for the account creation fee (java-tron `getCreateAccountFee`,
/// default 100_000 sun).
pub const CREATE_ACCOUNT_FEE_KEY: &str = "CREATE_ACCOUNT_FEE";
/// Committee gate allowing an already-set account name to be changed.
pub const ALLOW_UPDATE_ACCOUNT_NAME_KEY: &str = "ALLOW_UPDATE_ACCOUNT_NAME";

pub const MAX_ACCOUNT_NAME_LEN: usize = 200;
pub const MIN_ACCOUNT_ID_LEN: usize = 8;
pub const MAX_ACCOUNT_ID_LEN: usize = 32;

fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))
}

/// java-tron `TransactionUtil.validAccountName`: empty allowed, <= 200 bytes.
fn valid_account_name(name: &[u8]) -> bool {
    name.len() <= MAX_ACCOUNT_NAME_LEN
}

/// java-tron `TransactionUtil.validAccountId`: 8..=32 bytes, all in 0x21..=0x7E.
fn valid_account_id(id: &[u8]) -> bool {
    (MIN_ACCOUNT_ID_LEN..=MAX_ACCOUNT_ID_LEN).contains(&id.len())
        && id.iter().all(|b| (0x21..=0x7e).contains(b))
}

// ---------------------------------------------------------------------------
// CreateAccount
// ---------------------------------------------------------------------------

pub struct CreateAccountActuator<'a> {
    contract: &'a AccountCreateContract,
}

impl<'a> CreateAccountActuator<'a> {
    pub fn new(contract: &'a AccountCreateContract) -> Self {
        Self { contract }
    }

    /// Returns the fee execution will charge (`CREATE_ACCOUNT_FEE`).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let target = parse_address(&self.contract.account_address, "account address")?;

        let owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account has not existed".into()))?;

        if state.account_exists(&target)? {
            return Err(ActuatorError::Validate("Account has existed".into()));
        }

        let fee = state.get_prop_i64(CREATE_ACCOUNT_FEE_KEY)?;
        if owner_account.balance < fee {
            return Err(ActuatorError::Validate(
                "Validate CreateAccountActuator error, insufficient fee.".into(),
            ));
        }
        Ok(fee)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let target = parse_address(&self.contract.account_address, "account address")?;
        let fee = state.get_prop_i64(CREATE_ACCOUNT_FEE_KEY)?;

        // Create the target with the contract's account type.
        let mut created = state.create_account(&target)?;
        created.r#type = self.contract.r#type;
        state.put_account(&target, &created)?;

        // Debit + burn the fee.
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        owner_account.balance = owner_account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        state.put_account(&owner, &owner_account)?;
        if fee > 0 {
            state.burn_trx(fee)?;
        }
        Ok(ExecutionResult { fee })
    }
}

// ---------------------------------------------------------------------------
// UpdateAccount (account name)
// ---------------------------------------------------------------------------

pub struct UpdateAccountActuator<'a> {
    contract: &'a AccountUpdateContract,
}

impl<'a> UpdateAccountActuator<'a> {
    pub fn new(contract: &'a AccountUpdateContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        if !valid_account_name(&self.contract.account_name) {
            return Err(ActuatorError::Validate("Invalid accountName".into()));
        }
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist".into()))?;

        if !account.account_name.is_empty()
            && state.get_prop_i64(ALLOW_UPDATE_ACCOUNT_NAME_KEY)? == 0
        {
            return Err(ActuatorError::Validate("This account name is already existed".into()));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.account_name = self.contract.account_name.clone();
        state.put_account(&owner, &account)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// SetAccountId
// ---------------------------------------------------------------------------

pub struct SetAccountIdActuator<'a> {
    contract: &'a SetAccountIdContract,
}

impl<'a> SetAccountIdActuator<'a> {
    pub fn new(contract: &'a SetAccountIdContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        if !valid_account_id(&self.contract.account_id) {
            return Err(ActuatorError::Validate("Invalid accountId".into()));
        }
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account has not existed".into()))?;
        if !account.account_id.is_empty() {
            return Err(ActuatorError::Validate("This account id already set".into()));
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "ownerAddress")?;
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.account_id = self.contract.account_id.clone();
        state.put_account(&owner, &account)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_state::props;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn seeded(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            owner,
            &protocol::Account {
                address: owner.as_bytes().to_vec(),
                balance,
                ..Default::default()
            },
        )
        .unwrap();
        ws
    }

    // -- CreateAccount ----------------------------------------------------

    fn create_contract(owner: &Address, target: &Address) -> AccountCreateContract {
        AccountCreateContract {
            owner_address: owner.as_bytes().to_vec(),
            account_address: target.as_bytes().to_vec(),
            r#type: protocol::AccountType::Normal as i32,
        }
    }

    #[test]
    fn create_happy_path_charges_and_burns_fee() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded(&o, 1_000_000);
        ws.put_prop_i64(CREATE_ACCOUNT_FEE_KEY, 100_000).unwrap();
        let c = create_contract(&o, &t);
        let a = CreateAccountActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 100_000);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 100_000);
        assert!(ws.account_exists(&t).unwrap());
        let ob = ws.get_account(&o).unwrap().unwrap().balance;
        let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
        assert_eq!(ob, 900_000);
        assert_eq!(burned, 100_000);
        assert_eq!(ob + burned, 1_000_000); // conservation
    }

    #[test]
    fn create_existing_target_rejected() {
        let (o, t) = (addr(1), addr(2));
        let ws = seeded(&o, 1_000_000);
        ws.create_account(&t).unwrap();
        let c = create_contract(&o, &t);
        assert_eq!(
            CreateAccountActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Account has existed".into()))
        );
    }

    #[test]
    fn create_insufficient_fee_rejected() {
        let (o, t) = (addr(1), addr(2));
        let ws = seeded(&o, 50_000);
        ws.put_prop_i64(CREATE_ACCOUNT_FEE_KEY, 100_000).unwrap();
        let c = create_contract(&o, &t);
        assert!(matches!(
            CreateAccountActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("insufficient fee")
        ));
    }

    #[test]
    fn create_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = create_contract(&addr(1), &addr(2));
        assert_eq!(
            CreateAccountActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Account has not existed".into()))
        );
    }

    #[test]
    fn create_malformed_addresses_rejected() {
        let o = addr(1);
        let ws = seeded(&o, 1_000_000);
        let c = AccountCreateContract {
            owner_address: vec![0x41; 20], // wrong length
            account_address: addr(2).as_bytes().to_vec(),
            r#type: 0,
        };
        assert!(CreateAccountActuator::new(&c).validate(&ws).is_err());
        let mut bad = [0u8; ADDRESS_LEN];
        bad[0] = 0x42; // wrong prefix
        let c = AccountCreateContract {
            owner_address: o.as_bytes().to_vec(),
            account_address: bad.to_vec(),
            r#type: 0,
        };
        assert!(CreateAccountActuator::new(&c).validate(&ws).is_err());
    }

    // -- UpdateAccount ----------------------------------------------------

    fn update_contract(owner: &Address, name: &[u8]) -> AccountUpdateContract {
        AccountUpdateContract {
            owner_address: owner.as_bytes().to_vec(),
            account_name: name.to_vec(),
        }
    }

    #[test]
    fn update_happy_path_sets_name() {
        let o = addr(1);
        let mut ws = seeded(&o, 0);
        let c = update_contract(&o, b"alice");
        let a = UpdateAccountActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_account(&o).unwrap().unwrap().account_name, b"alice");
    }

    #[test]
    fn update_name_already_set_gated_by_committee_prop() {
        let o = addr(1);
        let mut ws = seeded(&o, 0);
        let a1 = update_contract(&o, b"alice");
        UpdateAccountActuator::new(&a1).execute(&mut ws).unwrap();
        // default (prop=0): rejected
        let a2 = update_contract(&o, b"bob");
        assert_eq!(
            UpdateAccountActuator::new(&a2).validate(&ws),
            Err(ActuatorError::Validate("This account name is already existed".into()))
        );
        // committee-enabled: accepted
        ws.put_prop_i64(ALLOW_UPDATE_ACCOUNT_NAME_KEY, 1).unwrap();
        assert_eq!(UpdateAccountActuator::new(&a2).validate(&ws).unwrap(), 0);
    }

    #[test]
    fn update_name_too_long_rejected() {
        let o = addr(1);
        let ws = seeded(&o, 0);
        let c = update_contract(&o, &[b'x'; 201]);
        assert_eq!(
            UpdateAccountActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Invalid accountName".into()))
        );
    }

    #[test]
    fn update_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = update_contract(&addr(1), b"alice");
        assert_eq!(
            UpdateAccountActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Account does not exist".into()))
        );
    }

    // -- SetAccountId -----------------------------------------------------

    fn id_contract(owner: &Address, id: &[u8]) -> SetAccountIdContract {
        SetAccountIdContract {
            owner_address: owner.as_bytes().to_vec(),
            account_id: id.to_vec(),
        }
    }

    #[test]
    fn set_account_id_happy_path() {
        let o = addr(1);
        let mut ws = seeded(&o, 0);
        let c = id_contract(&o, b"alice-01");
        let a = SetAccountIdActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_account(&o).unwrap().unwrap().account_id, b"alice-01");
    }

    #[test]
    fn set_account_id_twice_rejected() {
        let o = addr(1);
        let mut ws = seeded(&o, 0);
        SetAccountIdActuator::new(&id_contract(&o, b"alice-01")).execute(&mut ws).unwrap();
        assert_eq!(
            SetAccountIdActuator::new(&id_contract(&o, b"alice-02")).validate(&ws),
            Err(ActuatorError::Validate("This account id already set".into()))
        );
    }

    #[test]
    fn set_account_id_length_bounds() {
        let o = addr(1);
        let ws = seeded(&o, 0);
        for bad in [&b"seven77"[..], &[b'x'; 33][..]] {
            assert_eq!(
                SetAccountIdActuator::new(&id_contract(&o, bad)).validate(&ws),
                Err(ActuatorError::Validate("Invalid accountId".into())),
                "len {} must be rejected",
                bad.len()
            );
        }
        // boundaries 8 and 32 are valid
        for good in [&[b'x'; 8][..], &[b'x'; 32][..]] {
            assert!(SetAccountIdActuator::new(&id_contract(&o, good)).validate(&ws).is_ok());
        }
    }

    #[test]
    fn set_account_id_readable_bytes_only() {
        let o = addr(1);
        let ws = seeded(&o, 0);
        // space (0x20) and DEL (0x7F) and high bytes are unreadable per java-tron
        for bad in [&b"has space"[..], &b"del\x7fchar"[..], "high\u{e9}aa".as_bytes()] {
            assert_eq!(
                SetAccountIdActuator::new(&id_contract(&o, bad)).validate(&ws),
                Err(ActuatorError::Validate("Invalid accountId".into()))
            );
        }
    }

    #[test]
    fn set_account_id_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        assert_eq!(
            SetAccountIdActuator::new(&id_contract(&addr(1), b"alice-01")).validate(&ws),
            Err(ActuatorError::Validate("Account has not existed".into()))
        );
    }
}
