//! `TransferContract` — plain TRX transfer.
//!
//! Semantics mirror java-tron's `TransferActuator` exactly:
//!
//! **validate** — owner/to addresses must be valid 21-byte `0x41…`; `to != owner`;
//! owner account must exist; `amount > 0`; if the target account does not exist the
//! create-account fee is added; `balance >= amount + fee` (checked math — overflow
//! is a validation error, like java-tron's `ArithmeticException`).
//!
//! **execute** — create the target account if missing (charging
//! `CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT`), debit owner `amount + fee`, burn
//! the fee (blackhole-optimization path), credit the target `amount`. Transfer
//! itself is free (`TRANSFER_FEE = 0`).

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::TransferContract;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Base fee for a transfer (java-tron `Parameter.ChainConstant.TRANSFER_FEE`).
pub const TRANSFER_FEE: i64 = 0;

pub struct TransferActuator<'a> {
    contract: &'a TransferContract,
}

impl<'a> TransferActuator<'a> {
    pub fn new(contract: &'a TransferContract) -> Self {
        Self { contract }
    }

    fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
        let arr: [u8; ADDRESS_LEN] = bytes
            .try_into()
            .map_err(|_| ActuatorError::Validate(format!("Invalid {what}!")))?;
        Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}!")))
    }

    /// java-tron `TransferActuator.validate`. Returns the total fee that
    /// execution will charge (0, or the create-account fee).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "ownerAddress")?;
        let to = Self::parse_address(&self.contract.to_address, "toAddress")?;

        if owner == to {
            return Err(ActuatorError::Validate("Cannot transfer TRX to yourself.".into()));
        }

        let owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Validate TransferContract error, no OwnerAccount.".into()))?;

        let amount = self.contract.amount;
        if amount <= 0 {
            return Err(ActuatorError::Validate("Amount must be greater than 0.".into()));
        }

        let mut fee = TRANSFER_FEE;
        if !state.account_exists(&to)? {
            fee += state.get_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT)?;
        }

        let total = amount
            .checked_add(fee)
            .ok_or_else(|| ActuatorError::Validate("long overflow".into()))?;
        if owner_account.balance < total {
            return Err(ActuatorError::Validate(
                "Validate TransferContract error, balance is not sufficient.".into(),
            ));
        }
        Ok(fee)
    }

    /// java-tron `TransferActuator.execute`. Call after a successful `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "ownerAddress")?;
        let to = Self::parse_address(&self.contract.to_address, "toAddress")?;
        let amount = self.contract.amount;

        let mut fee = TRANSFER_FEE;
        if !state.account_exists(&to)? {
            state.create_account(&to)?;
            fee += state.get_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT)?;
        }

        let total = amount
            .checked_add(fee)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;

        // Debit owner (checked, like java-tron adjustBalance).
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        owner_account.balance = owner_account
            .balance
            .checked_sub(total)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        state.put_account(&owner, &owner_account)?;

        // Burn the fee (supportBlackHoleOptimization path).
        if fee > 0 {
            state.burn_trx(fee)?;
        }

        // Credit target.
        let mut to_account = state
            .get_account(&to)?
            .ok_or_else(|| ActuatorError::Execute("to account missing".into()))?;
        to_account.balance = to_account
            .balance
            .checked_add(amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        state.put_account(&to, &to_account)?;

        Ok(ExecutionResult { fee })
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

    fn seeded_state(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
        let mut ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance,
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        ws
    }

    fn contract(owner: &Address, to: &Address, amount: i64) -> TransferContract {
        TransferContract {
            owner_address: owner.as_bytes().to_vec(),
            to_address: to.as_bytes().to_vec(),
            amount,
        }
    }

    #[test]
    fn happy_path_existing_target_is_free() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000);
        ws.put_account(&t, &protocol::Account { address: t.as_bytes().to_vec(), balance: 5, ..Default::default() })
            .unwrap();

        let c = contract(&o, &t, 1_000_000);
        let a = TransferActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 0);
        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 9_000_000);
        assert_eq!(ws.get_account(&t).unwrap().unwrap().balance, 1_000_005);
    }

    #[test]
    fn creates_missing_target_and_charges_fee() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1_000_000).unwrap();

        let c = contract(&o, &t, 2_000_000);
        let a = TransferActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 1_000_000);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 1_000_000);
        // owner: 10 - 2 (amount) - 1 (fee) = 7 TRX
        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 7_000_000);
        assert_eq!(ws.get_account(&t).unwrap().unwrap().balance, 2_000_000);
        // the fee was burned, not credited anywhere
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), 1_000_000);
    }

    #[test]
    fn rejects_self_transfer() {
        let o = addr(1);
        let ws = seeded_state(&o, 100);
        let c = contract(&o, &o, 10);
        assert_eq!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Cannot transfer TRX to yourself.".into()))
        );
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        let c = contract(&addr(1), &addr(2), 10);
        assert!(matches!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no OwnerAccount")
        ));
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 100);
        for amount in [0, -1, i64::MIN] {
            let c = contract(&o, &t, amount);
            assert!(matches!(
                TransferActuator::new(&c).validate(&ws),
                Err(ActuatorError::Validate(m)) if m.contains("greater than 0")
            ), "amount {amount} must be rejected");
        }
    }

    #[test]
    fn rejects_insufficient_balance_including_create_fee() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 1_500_000);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1_000_000).unwrap();
        // amount alone fits, amount+create-fee does not
        let c = contract(&o, &t, 1_000_000);
        assert!(matches!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("balance is not sufficient")
        ));
    }

    #[test]
    fn rejects_overflow_amount_plus_fee() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, i64::MAX);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1).unwrap();
        let c = contract(&o, &t, i64::MAX);
        assert!(matches!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("long overflow")
        ));
    }

    #[test]
    fn rejects_malformed_addresses() {
        let o = addr(1);
        let ws = seeded_state(&o, 100);
        // wrong length
        let c = TransferContract {
            owner_address: vec![0x41; 20],
            to_address: addr(2).as_bytes().to_vec(),
            amount: 10,
        };
        assert!(matches!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid ownerAddress")
        ));
        // wrong prefix
        let mut bad = [0u8; ADDRESS_LEN];
        bad[0] = 0x42;
        let c = TransferContract {
            owner_address: o.as_bytes().to_vec(),
            to_address: bad.to_vec(),
            amount: 10,
        };
        assert!(matches!(
            TransferActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid toAddress")
        ));
    }

    #[test]
    fn execute_preserves_total_supply() {
        // sum(balances) + burned must be invariant across execution.
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 9_999_999);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 123_456).unwrap();
        let c = contract(&o, &t, 4_000_000);
        let a = TransferActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        let ob = ws.get_account(&o).unwrap().unwrap().balance;
        let tb = ws.get_account(&t).unwrap().unwrap().balance;
        let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
        assert_eq!(ob + tb + burned, 9_999_999);
    }
}
