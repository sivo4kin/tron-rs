//! `TransferAssetContract` — TRC10 asset transfer.
//!
//! Semantics mirror java-tron's `TransferAssetActuator` exactly, adapted to the
//! P1 world-state model (which stores per-account asset balances in the prost
//! `Account.asset_v2` map, keyed by the token id/name string).
//!
//! **validate** — owner/to addresses must be valid 21-byte `0x41…`; `amount > 0`;
//! `to != owner`; owner account must exist; the owner must hold the asset with a
//! positive balance and `amount <= assetBalance`; if the target account already
//! holds the asset, `to.asset + amount` must not overflow; if the target account
//! is missing, the create-account fee is added and the owner's TRX balance must
//! cover it. Returns the TRX fee execution will charge.
//!
//! **execute** — create the target account if missing (adding
//! `CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT`), debit the owner's asset balance,
//! credit the target's asset balance, charge the TRX fee from the owner and burn
//! it (blackhole-optimization path). All arithmetic is checked; overflow/underflow
//! is an error, mirroring java-tron's `ArithmeticException`/`reduceAssetAmount`
//! failure.
//!
//! Deviations from java-tron (the P1 world-state has no `AssetIssueStore`):
//! - The global "No asset!" issue-store existence check is not modeled. A missing
//!   asset manifests as the owner not holding it, so it is rejected via
//!   "assetBalance must be greater than 0." (java-tron's `getAsset`-returns-null
//!   path).
//! - `AllowSameTokenName` / `assetOptimized` toggling between the legacy `asset`
//!   and `asset_v2` maps is not modeled; balances live in `asset_v2` only, keyed
//!   by the raw `asset_name` bytes interpreted as a UTF-8 string
//!   (java-tron `ByteArray.toStr`).
//! - The `ForbidTransferToContract` proposal check is not modeled (P1 has no
//!   account-type/contract distinction in this path).

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::TransferAssetContract;
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Base fee for an asset transfer (java-tron `TransferAssetActuator.calcFee` == 0).
pub const TRANSFER_ASSET_FEE: i64 = 0;

pub struct TransferAssetActuator<'a> {
    contract: &'a TransferAssetContract,
}

impl<'a> TransferAssetActuator<'a> {
    pub fn new(contract: &'a TransferAssetContract) -> Self {
        Self { contract }
    }

    fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
        let arr: [u8; ADDRESS_LEN] = bytes
            .try_into()
            .map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))?;
        Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))
    }

    /// Asset lookup key: java-tron `ByteArray.toStr(assetName)`.
    fn asset_key(&self) -> String {
        String::from_utf8_lossy(&self.contract.asset_name).into_owned()
    }

    /// java-tron `TransferAssetActuator.validate`. Returns the total TRX fee that
    /// execution will charge (0, or the create-account fee).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "ownerAddress")?;
        let to = Self::parse_address(&self.contract.to_address, "toAddress")?;

        let amount = self.contract.amount;
        if amount <= 0 {
            return Err(ActuatorError::Validate("Amount must be greater than 0.".into()));
        }

        if owner == to {
            return Err(ActuatorError::Validate(
                "Cannot transfer asset to yourself.".into(),
            ));
        }

        let owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("No owner account!".into()))?;

        let key = self.asset_key();
        let asset_balance = owner_account.asset_v2.get(&key).copied().unwrap_or(0);
        if asset_balance <= 0 {
            return Err(ActuatorError::Validate(
                "assetBalance must be greater than 0.".into(),
            ));
        }
        if amount > asset_balance {
            return Err(ActuatorError::Validate("assetBalance is not sufficient.".into()));
        }

        let mut fee = TRANSFER_ASSET_FEE;
        match state.get_account(&to)? {
            Some(to_account) => {
                // If the target already holds the asset, the credit must not overflow.
                if let Some(&to_balance) = to_account.asset_v2.get(&key) {
                    to_balance
                        .checked_add(amount)
                        .ok_or_else(|| ActuatorError::Validate("long overflow".into()))?;
                }
            }
            None => {
                fee += state.get_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT)?;
                if owner_account.balance < fee {
                    return Err(ActuatorError::Validate(
                        "Validate TransferAssetActuator error, insufficient fee.".into(),
                    ));
                }
            }
        }

        Ok(fee)
    }

    /// java-tron `TransferAssetActuator.execute`. Call after a successful `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "ownerAddress")?;
        let to = Self::parse_address(&self.contract.to_address, "toAddress")?;
        let amount = self.contract.amount;
        let key = self.asset_key();

        let mut fee = TRANSFER_ASSET_FEE;
        if !state.account_exists(&to)? {
            state.create_account(&to)?;
            fee += state.get_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT)?;
        }

        // Debit the owner's asset (checked, like java-tron reduceAssetAmountV2) and
        // charge the TRX fee from the owner's balance in the same write.
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        let owner_asset = owner_account.asset_v2.get(&key).copied().unwrap_or(0);
        let new_owner_asset = owner_asset
            .checked_sub(amount)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("reduceAssetAmount failed !".into()))?;
        owner_account.asset_v2.insert(key.clone(), new_owner_asset);
        owner_account.balance = owner_account
            .balance
            .checked_sub(fee)
            .filter(|b| *b >= 0)
            .ok_or_else(|| {
                ActuatorError::Execute(
                    "Validate TransferAssetActuator error, insufficient fee.".into(),
                )
            })?;
        state.put_account(&owner, &owner_account)?;

        // Credit the target's asset (checked, like java-tron addAssetAmountV2).
        let mut to_account = state
            .get_account(&to)?
            .ok_or_else(|| ActuatorError::Execute("to account missing".into()))?;
        let to_asset = to_account.asset_v2.get(&key).copied().unwrap_or(0);
        let new_to_asset = to_asset
            .checked_add(amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        to_account.asset_v2.insert(key, new_to_asset);
        state.put_account(&to, &to_account)?;

        // Burn the fee (supportBlackHoleOptimization path).
        if fee > 0 {
            state.burn_trx(fee)?;
        }

        Ok(ExecutionResult { fee })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const ASSET: &[u8] = b"1000001";
    const OTHER_ASSET: &[u8] = b"1000002";

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// An account holding `trx` sun and `asset_amount` of `ASSET`.
    fn account(a: &Address, trx: i64, asset_amount: i64) -> protocol::Account {
        let mut acc = protocol::Account {
            address: a.as_bytes().to_vec(),
            balance: trx,
            ..Default::default()
        };
        if asset_amount != 0 {
            acc.asset_v2
                .insert(String::from_utf8_lossy(ASSET).into_owned(), asset_amount);
        }
        acc
    }

    /// World state with an owner holding `trx` sun and `asset` of `ASSET`.
    fn seeded_state(owner: &Address, trx: i64, asset: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(owner, &account(owner, trx, asset)).unwrap();
        ws
    }

    fn contract(owner: &Address, to: &Address, asset_name: &[u8], amount: i64) -> TransferAssetContract {
        TransferAssetContract {
            asset_name: asset_name.to_vec(),
            owner_address: owner.as_bytes().to_vec(),
            to_address: to.as_bytes().to_vec(),
            amount,
        }
    }

    fn owner_asset(ws: &WorldState<MemoryStore>, a: &Address) -> i64 {
        ws.get_account(a)
            .unwrap()
            .unwrap()
            .asset_v2
            .get(&String::from_utf8_lossy(ASSET).into_owned())
            .copied()
            .unwrap_or(0)
    }

    #[test]
    fn happy_path_existing_target() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000, 1_000);
        ws.put_account(&t, &account(&t, 5, 7)).unwrap();

        let c = contract(&o, &t, ASSET, 400);
        let a = TransferAssetActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 0);
        assert_eq!(owner_asset(&ws, &o), 600);
        assert_eq!(owner_asset(&ws, &t), 407);
        // TRX untouched (no fee), no burn.
        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 10_000_000);
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), 0);
    }

    #[test]
    fn rejects_missing_asset() {
        // Owner exists and is funded but holds no asset at all.
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 10_000_000, 0);
        let c = contract(&o, &t, ASSET, 100);
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("assetBalance must be greater than 0")
        ));
    }

    #[test]
    fn rejects_wrong_asset_id() {
        // Owner holds ASSET but the contract references a different token id.
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 10_000_000, 1_000);
        let c = contract(&o, &t, OTHER_ASSET, 100);
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("assetBalance must be greater than 0")
        ));
    }

    #[test]
    fn rejects_insufficient_asset_balance() {
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 10_000_000, 100);
        let c = contract(&o, &t, ASSET, 101);
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("assetBalance is not sufficient")
        ));
    }

    #[test]
    fn rejects_self_transfer() {
        let o = addr(1);
        let ws = seeded_state(&o, 100, 1_000);
        let c = contract(&o, &o, ASSET, 10);
        assert_eq!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("Cannot transfer asset to yourself.".into()))
        );
    }

    #[test]
    fn rejects_missing_owner() {
        let ws = WorldState::new(MemoryStore::new());
        let c = contract(&addr(1), &addr(2), ASSET, 10);
        assert_eq!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate("No owner account!".into()))
        );
    }

    #[test]
    fn rejects_non_positive_amounts() {
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 100, 1_000);
        for amount in [0, -1, i64::MIN] {
            let c = contract(&o, &t, ASSET, amount);
            assert!(
                matches!(
                    TransferAssetActuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("greater than 0")
                ),
                "amount {amount} must be rejected"
            );
        }
    }

    #[test]
    fn creates_to_account_charges_trx_fee_and_burns_while_asset_moves() {
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000, 1_000);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1_000_000)
            .unwrap();

        let c = contract(&o, &t, ASSET, 1_000);
        let a = TransferAssetActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 1_000_000);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 1_000_000);

        // The full asset amount moved.
        assert_eq!(owner_asset(&ws, &o), 0);
        assert_eq!(owner_asset(&ws, &t), 1_000);
        // TRX fee debited from owner and burned (not credited to the new account).
        assert_eq!(ws.get_account(&o).unwrap().unwrap().balance, 9_000_000);
        assert_eq!(ws.get_account(&t).unwrap().unwrap().balance, 0);
        assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), 1_000_000);
    }

    #[test]
    fn rejects_insufficient_fee_for_new_account() {
        // Owner has plenty of asset but cannot cover the create-account fee.
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 500_000, 1_000);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 1_000_000)
            .unwrap();
        let c = contract(&o, &t, ASSET, 10);
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("insufficient fee")
        ));
    }

    #[test]
    fn rejects_asset_credit_overflow() {
        // Target already holds i64::MAX of the asset; any credit overflows.
        let (o, t) = (addr(1), addr(2));
        let ws = seeded_state(&o, 10_000_000, 1_000);
        ws.put_account(&t, &account(&t, 0, i64::MAX)).unwrap();
        let c = contract(&o, &t, ASSET, 1);
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("long overflow")
        ));
    }

    #[test]
    fn rejects_malformed_addresses() {
        let o = addr(1);
        let ws = seeded_state(&o, 100, 1_000);
        // wrong length owner
        let c = TransferAssetContract {
            asset_name: ASSET.to_vec(),
            owner_address: vec![0x41; 20],
            to_address: addr(2).as_bytes().to_vec(),
            amount: 10,
        };
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid ownerAddress")
        ));
        // wrong prefix on to
        let mut bad = [0u8; ADDRESS_LEN];
        bad[0] = 0x42;
        let c = TransferAssetContract {
            asset_name: ASSET.to_vec(),
            owner_address: o.as_bytes().to_vec(),
            to_address: bad.to_vec(),
            amount: 10,
        };
        assert!(matches!(
            TransferAssetActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid toAddress")
        ));
    }

    #[test]
    fn asset_conservation_invariant_existing_target() {
        // Sum of the asset across all accounts is invariant across execution.
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000, 9_999);
        ws.put_account(&t, &account(&t, 42, 1)).unwrap();
        let before = owner_asset(&ws, &o) + owner_asset(&ws, &t);

        let c = contract(&o, &t, ASSET, 4_321);
        let a = TransferAssetActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();

        let after = owner_asset(&ws, &o) + owner_asset(&ws, &t);
        assert_eq!(before, after);
        assert_eq!(owner_asset(&ws, &o), 9_999 - 4_321);
        assert_eq!(owner_asset(&ws, &t), 1 + 4_321);
    }

    #[test]
    fn asset_conservation_invariant_new_target() {
        // Asset conserved even when the target is created and a TRX fee is burned.
        let (o, t) = (addr(1), addr(2));
        let mut ws = seeded_state(&o, 10_000_000, 5_000);
        ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 123_456)
            .unwrap();
        let before_asset = owner_asset(&ws, &o);
        let before_trx = ws.get_account(&o).unwrap().unwrap().balance;

        let c = contract(&o, &t, ASSET, 2_000);
        let a = TransferAssetActuator::new(&c);
        let fee = a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();

        // Asset conserved.
        assert_eq!(owner_asset(&ws, &o) + owner_asset(&ws, &t), before_asset);
        // TRX conserved: owner_balance + burned == before.
        let owner_trx = ws.get_account(&o).unwrap().unwrap().balance;
        let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
        assert_eq!(owner_trx + burned, before_trx);
        assert_eq!(fee, 123_456);
    }
}
