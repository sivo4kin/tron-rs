//! `ParticipateAssetIssueContract` — buy TRC10 tokens during their sale window
//! with TRX.
//!
//! Mirrors java-tron `ParticipateAssetIssueActuator` (V2 path): the buyer must
//! exist, differ from the issuer, and hold enough TRX; the asset must exist, be
//! issued by `to_address`, and be inside its `[start, end)` sale window; the
//! token amount `floor(amount * num / trx_num)` must be positive and still held
//! by the issuer. Execute moves `amount` TRX buyer→issuer and that token amount
//! issuer→buyer in `asset_v2`.
//!
//! Deviations from java-tron: V2 asset store only (see I02); fee is 0.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::ParticipateAssetIssueContract;
use tron_state::{asset_v2_balance, props, set_asset_v2_balance, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

fn parse_address(bytes: &[u8], what: &str) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(format!("Invalid {what}")))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse the ascii token id in `asset_name` to a numeric id.
fn asset_id(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok().and_then(|s| s.parse::<i64>().ok())
}

/// `floor(amount * num / trx_num)` in i128, rejecting out-of-range results.
fn exchange_amount(amount: i64, num: i32, trx_num: i32) -> Result<i64, ActuatorError> {
    let v = (amount as i128 * num as i128) / trx_num as i128;
    i64::try_from(v).map_err(|_| ActuatorError::Validate("long overflow".into()))
}

pub struct ParticipateAssetIssueActuator<'a> {
    contract: &'a ParticipateAssetIssueContract,
}

impl<'a> ParticipateAssetIssueActuator<'a> {
    pub fn new(contract: &'a ParticipateAssetIssueContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let c = self.contract;
        let owner = parse_address(&c.owner_address, "ownerAddress")?;
        let to = parse_address(&c.to_address, "toAddress")?;

        if c.amount <= 0 {
            return Err(ActuatorError::Validate("Amount must greater than 0!".into()));
        }
        if owner == to {
            return Err(ActuatorError::Validate("Cannot participate asset Issue yourself !".into()));
        }

        let owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Validate("Account does not exist!".into()))?;
        // fee is 0; buyer must still cover `amount`.
        if owner_account.balance < c.amount {
            return Err(ActuatorError::Validate("No enough balance !".into()));
        }

        let id = asset_id(&c.asset_name).ok_or_else(|| {
            ActuatorError::Validate(format!("No asset named {}", String::from_utf8_lossy(&c.asset_name)))
        })?;
        let asset = state.get_asset_issue(id)?.ok_or_else(|| {
            ActuatorError::Validate(format!("No asset named {}", String::from_utf8_lossy(&c.asset_name)))
        })?;

        if asset.owner_address.as_slice() != to.as_bytes() {
            return Err(ActuatorError::Validate(format!(
                "The asset is not issued by {}",
                hex(to.as_bytes())
            )));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        if now >= asset.end_time || now < asset.start_time {
            return Err(ActuatorError::Validate("No longer valid period!".into()));
        }

        let exchange = exchange_amount(c.amount, asset.num, asset.trx_num)?;
        if exchange <= 0 {
            return Err(ActuatorError::Validate("Can not process the exchange!".into()));
        }

        let to_account = state
            .get_account(&to)?
            .ok_or_else(|| ActuatorError::Validate("To account does not exist!".into()))?;
        if asset_v2_balance(&to_account, id) < exchange {
            return Err(ActuatorError::Validate("Asset balance is not enough !".into()));
        }

        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let c = self.contract;
        let owner = parse_address(&c.owner_address, "ownerAddress")?;
        let to = parse_address(&c.to_address, "toAddress")?;
        let id = asset_id(&c.asset_name)
            .ok_or_else(|| ActuatorError::Execute("bad asset name".into()))?;
        let asset = state
            .get_asset_issue(id)?
            .ok_or_else(|| ActuatorError::Execute("asset record missing".into()))?;
        let exchange = exchange_amount(c.amount, asset.num, asset.trx_num)
            .map_err(|e| ActuatorError::Execute(format!("{e:?}")))?;

        // Buyer: -amount TRX, +exchange tokens.
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        owner_account.balance = owner_account
            .balance
            .checked_sub(c.amount)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("balance is not sufficient".into()))?;
        let new_owner_tokens = asset_v2_balance(&owner_account, id)
            .checked_add(exchange)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        set_asset_v2_balance(&mut owner_account, id, new_owner_tokens);

        // Issuer: +amount TRX, -exchange tokens.
        let mut to_account = state
            .get_account(&to)?
            .ok_or_else(|| ActuatorError::Execute("to account missing".into()))?;
        to_account.balance = to_account
            .balance
            .checked_add(c.amount)
            .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?;
        let new_to_tokens = asset_v2_balance(&to_account, id)
            .checked_sub(exchange)
            .filter(|b| *b >= 0)
            .ok_or_else(|| ActuatorError::Execute("reduceAssetAmount failed !".into()))?;
        set_asset_v2_balance(&mut to_account, id, new_to_tokens);

        state.put_account(&owner, &owner_account)?;
        state.put_account(&to, &to_account)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const ID: i64 = 1_000_001;
    const NOW: i64 = 1_700_000_000_000;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// Buyer `owner` (with `trx` TRX), issuer `to` holding `issuer_tokens` of the
    /// asset, and the asset record (num/trx_num, sale window open).
    fn scenario(
        owner: &Address,
        to: &Address,
        trx: i64,
        issuer_tokens: i64,
        num: i32,
        trx_num: i32,
    ) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(
            owner,
            &protocol::Account { address: owner.as_bytes().to_vec(), balance: trx, ..Default::default() },
        )
        .unwrap();
        let mut issuer = protocol::Account {
            address: to.as_bytes().to_vec(),
            balance: 0,
            ..Default::default()
        };
        set_asset_v2_balance(&mut issuer, ID, issuer_tokens);
        ws.put_account(to, &issuer).unwrap();
        let asset = protocol::AssetIssueContract {
            id: "1000001".into(),
            owner_address: to.as_bytes().to_vec(),
            name: b"MyToken".to_vec(),
            total_supply: 1_000_000,
            num,
            trx_num,
            start_time: NOW - 1_000,
            end_time: NOW + 1_000,
            ..Default::default()
        };
        ws.put_asset_issue(ID, &asset).unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        ws
    }

    fn contract(owner: &Address, to: &Address, amount: i64) -> ParticipateAssetIssueContract {
        ParticipateAssetIssueContract {
            owner_address: owner.as_bytes().to_vec(),
            to_address: to.as_bytes().to_vec(),
            asset_name: b"1000001".to_vec(),
            amount,
        }
    }

    #[test]
    fn happy_path_exchanges_trx_for_tokens_and_conserves() {
        let (o, t) = (addr(1), addr(2));
        // num=100, trx_num=1 => 1000 TRX buys 100_000 tokens.
        let mut ws = scenario(&o, &t, 2_000, 500_000, 100, 1);

        let pre_trx = ws.get_account(&o).unwrap().unwrap().balance
            + ws.get_account(&t).unwrap().unwrap().balance;
        let pre_tok = asset_v2_balance(&ws.get_account(&o).unwrap().unwrap(), ID)
            + asset_v2_balance(&ws.get_account(&t).unwrap().unwrap(), ID);

        let c = contract(&o, &t, 1_000);
        let a = ParticipateAssetIssueActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let owner = ws.get_account(&o).unwrap().unwrap();
        let issuer = ws.get_account(&t).unwrap().unwrap();
        // exchange = 1000 * 100 / 1 = 100_000 tokens.
        assert_eq!(owner.balance, 1_000); // 2000 - 1000
        assert_eq!(asset_v2_balance(&owner, ID), 100_000);
        assert_eq!(issuer.balance, 1_000); // 0 + 1000
        assert_eq!(asset_v2_balance(&issuer, ID), 400_000); // 500_000 - 100_000

        // Conservation of TRX and tokens across buyer+issuer.
        assert_eq!(owner.balance + issuer.balance, pre_trx);
        assert_eq!(asset_v2_balance(&owner, ID) + asset_v2_balance(&issuer, ID), pre_tok);
    }

    #[test]
    fn exchange_math_uses_num_over_trx_num_with_floor() {
        let (o, t) = (addr(1), addr(2));
        // num=3, trx_num=7 => floor(1000 * 3 / 7) = floor(428.57) = 428.
        let mut ws = scenario(&o, &t, 5_000, 1_000_000, 3, 7);
        let c = contract(&o, &t, 1_000);
        let a = ParticipateAssetIssueActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        assert_eq!(asset_v2_balance(&ws.get_account(&o).unwrap().unwrap(), ID), 428);
    }

    #[test]
    fn rejects_outside_window() {
        let (o, t) = (addr(1), addr(2));
        let ws = scenario(&o, &t, 5_000, 1_000_000, 100, 1);
        // Move the clock past end_time.
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW + 5_000).unwrap();
        assert!(matches!(
            ParticipateAssetIssueActuator::new(&contract(&o, &t, 1_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("No longer valid period!")
        ));
    }

    #[test]
    fn rejects_insufficient_trx() {
        let (o, t) = (addr(1), addr(2));
        let ws = scenario(&o, &t, 500, 1_000_000, 100, 1);
        assert!(matches!(
            ParticipateAssetIssueActuator::new(&contract(&o, &t, 1_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("No enough balance !")
        ));
    }

    #[test]
    fn rejects_insufficient_remaining_supply() {
        let (o, t) = (addr(1), addr(2));
        // Issuer only holds 50_000 tokens; buying 1000 TRX needs 100_000.
        let ws = scenario(&o, &t, 5_000, 50_000, 100, 1);
        assert!(matches!(
            ParticipateAssetIssueActuator::new(&contract(&o, &t, 1_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Asset balance is not enough !")
        ));
    }

    #[test]
    fn rejects_self_participation() {
        let o = addr(1);
        let ws = scenario(&o, &o, 5_000, 1_000_000, 100, 1);
        assert!(matches!(
            ParticipateAssetIssueActuator::new(&contract(&o, &o, 1_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Cannot participate asset Issue yourself !")
        ));
    }

    #[test]
    fn rejects_missing_asset() {
        let (o, t) = (addr(1), addr(2));
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(&o, &protocol::Account { address: o.as_bytes().to_vec(), balance: 5_000, ..Default::default() }).unwrap();
        ws.put_account(&t, &protocol::Account { address: t.as_bytes().to_vec(), ..Default::default() }).unwrap();
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        assert!(matches!(
            ParticipateAssetIssueActuator::new(&contract(&o, &t, 1_000)).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("No asset named")
        ));
    }
}
