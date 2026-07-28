//! Block-application executor: route transactions to actuators and apply them.
//!
//! Mirrors java-tron's `Manager.processTransaction` → `ActuatorFactory` routing:
//! each transaction carries exactly one contract whose `type` selects the actuator
//! and whose packed `Any` parameter decodes to the concrete contract message.
//! Contract types without an implemented actuator are reported as such —
//! coverage grows per iteration and is tracked by the differential harness.

use crate::transfer::TransferActuator;
use crate::{ActuatorError, ExecutionResult};
use prost::Message;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::WorldState;
use tron_storage::KvStore;

/// Decode the packed contract parameter (`google.protobuf.Any.value`).
fn unpack<M: Message + Default>(
    contract: &protocol::transaction::Contract,
) -> Result<M, ActuatorError> {
    let any = contract
        .parameter
        .as_ref()
        .ok_or_else(|| ActuatorError::Validate("contract parameter missing".into()))?;
    M::decode(any.value.as_slice())
        .map_err(|e| ActuatorError::Validate(format!("cannot unpack contract: {e}")))
}

/// Validate + execute a single transaction against the state.
pub fn apply_transaction<S: KvStore>(
    state: &mut WorldState<S>,
    tx: &protocol::Transaction,
) -> Result<ExecutionResult, ActuatorError> {
    let raw = tx
        .raw_data
        .as_ref()
        .ok_or_else(|| ActuatorError::Validate("transaction has no raw_data".into()))?;
    let contract = raw
        .contract
        .first()
        .ok_or_else(|| ActuatorError::Validate("transaction has no contract".into()))?;

    match contract.r#type() {
        ContractType::TransferContract => {
            let c: protocol::TransferContract = unpack(contract)?;
            let actuator = TransferActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::TransferAssetContract => {
            let c: protocol::TransferAssetContract = unpack(contract)?;
            let actuator = crate::asset_transfer::TransferAssetActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::FreezeBalanceV2Contract => {
            let c: protocol::FreezeBalanceV2Contract = unpack(contract)?;
            let actuator = crate::freeze_v2::FreezeBalanceV2Actuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::UnfreezeBalanceV2Contract => {
            let c: protocol::UnfreezeBalanceV2Contract = unpack(contract)?;
            let actuator = crate::freeze_v2::UnfreezeBalanceV2Actuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::VoteWitnessContract => {
            let c: protocol::VoteWitnessContract = unpack(contract)?;
            let actuator = crate::vote::VoteWitnessActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::WithdrawBalanceContract => {
            let c: protocol::WithdrawBalanceContract = unpack(contract)?;
            let actuator = crate::withdraw::WithdrawBalanceActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::UpdateBrokerageContract => {
            let c: protocol::UpdateBrokerageContract = unpack(contract)?;
            let actuator = crate::brokerage::UpdateBrokerageActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::WithdrawExpireUnfreezeContract => {
            let c: protocol::WithdrawExpireUnfreezeContract = unpack(contract)?;
            let actuator = crate::withdraw_unfreeze::WithdrawExpireUnfreezeActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::CancelAllUnfreezeV2Contract => {
            let c: protocol::CancelAllUnfreezeV2Contract = unpack(contract)?;
            let actuator = crate::cancel_unfreeze::CancelAllUnfreezeV2Actuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ProposalCreateContract => {
            let c: protocol::ProposalCreateContract = unpack(contract)?;
            let actuator = crate::proposal::ProposalCreateActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ProposalApproveContract => {
            let c: protocol::ProposalApproveContract = unpack(contract)?;
            let actuator = crate::proposal::ProposalApproveActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ProposalDeleteContract => {
            let c: protocol::ProposalDeleteContract = unpack(contract)?;
            let actuator = crate::proposal::ProposalDeleteActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::AccountCreateContract => {
            let c: protocol::AccountCreateContract = unpack(contract)?;
            let actuator = crate::account::CreateAccountActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::AccountUpdateContract => {
            let c: protocol::AccountUpdateContract = unpack(contract)?;
            let actuator = crate::account::UpdateAccountActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::SetAccountIdContract => {
            let c: protocol::SetAccountIdContract = unpack(contract)?;
            let actuator = crate::account::SetAccountIdActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::AccountPermissionUpdateContract => {
            let c: protocol::AccountPermissionUpdateContract = unpack(contract)?;
            let actuator = crate::permission::AccountPermissionUpdateActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::WitnessCreateContract => {
            let c: protocol::WitnessCreateContract = unpack(contract)?;
            let actuator = crate::witness::WitnessCreateActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::WitnessUpdateContract => {
            let c: protocol::WitnessUpdateContract = unpack(contract)?;
            let actuator = crate::witness::WitnessUpdateActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ExchangeCreateContract => {
            let c: protocol::ExchangeCreateContract = unpack(contract)?;
            let actuator = crate::exchange::ExchangeCreateActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ExchangeInjectContract => {
            let c: protocol::ExchangeInjectContract = unpack(contract)?;
            let actuator = crate::exchange::ExchangeInjectActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ExchangeWithdrawContract => {
            let c: protocol::ExchangeWithdrawContract = unpack(contract)?;
            let actuator = crate::exchange::ExchangeWithdrawActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::ExchangeTransactionContract => {
            let c: protocol::ExchangeTransactionContract = unpack(contract)?;
            let actuator = crate::exchange::ExchangeTransactionActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::DelegateResourceContract => {
            let c: protocol::DelegateResourceContract = unpack(contract)?;
            let actuator = crate::delegate::DelegateResourceActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::UnDelegateResourceContract => {
            let c: protocol::UnDelegateResourceContract = unpack(contract)?;
            let actuator = crate::delegate::UnDelegateResourceActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::TriggerSmartContract => {
            let c: protocol::TriggerSmartContract = unpack(contract)?;
            // Default per-tx energy ceiling; real limit derives from fee_limit/energy price (P2 follow-up).
            let actuator =
                crate::smart_contract::TriggerSmartContractActuator::new(&c, 10_000_000);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        ContractType::CreateSmartContract => {
            let c: protocol::CreateSmartContract = unpack(contract)?;
            let actuator = crate::smart_contract::CreateSmartContractActuator::new(&c);
            actuator.validate(state)?;
            actuator.execute(state)
        }
        other => Err(ActuatorError::Validate(format!(
            "unsupported contract type: {other:?}"
        ))),
    }
}

/// Apply every transaction of a block in order. Stops at the first failure
/// (java-tron: a block containing an invalid transaction is itself invalid).
pub fn apply_block<S: KvStore>(
    state: &mut WorldState<S>,
    block: &protocol::Block,
) -> Result<Vec<ExecutionResult>, ActuatorError> {
    let mut results = Vec::with_capacity(block.transactions.len());
    for tx in &block.transactions {
        results.push(apply_transaction(state, tx)?);
    }
    Ok(results)
}

/// Full block-processing step (java-tron `Manager.pushBlock` essence): apply all
/// transactions to the state, then persist the block and index its transactions
/// by id. Returns the per-transaction results. On a tx failure the whole step
/// errors (the caller reverts / rejects the block).
pub fn process_block<S: KvStore>(
    state: &mut WorldState<S>,
    block: &protocol::Block,
) -> Result<Vec<ExecutionResult>, ActuatorError> {
    let results = apply_block(state, block)?;
    state.put_block(block).map_err(ActuatorError::from)?;
    state.index_block_transactions(block).map_err(ActuatorError::from)?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;
    use tron_types::Address;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn transfer_tx(owner: &Address, to: &Address, amount: i64) -> protocol::Transaction {
        let c = protocol::TransferContract {
            owner_address: owner.as_bytes().to_vec(),
            to_address: to.as_bytes().to_vec(),
            amount,
        };
        let contract = protocol::transaction::Contract {
            r#type: ContractType::TransferContract as i32,
            parameter: Some(prost_types::Any {
                type_url: "type.googleapis.com/protocol.TransferContract".into(),
                value: c.encode_to_vec(),
            }),
            ..Default::default()
        };
        protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw {
                contract: vec![contract],
                ..Default::default()
            }),
            ..Default::default()
        }
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

    #[test]
    fn applies_a_block_of_transfers_in_order() {
        let (a, b, c) = (addr(1), addr(2), addr(3));
        let mut ws = seeded(&a, 10_000_000);
        // a -> b 4 TRX, then b -> c 1 TRX (depends on the first executing first)
        let block = protocol::Block {
            transactions: vec![transfer_tx(&a, &b, 4_000_000), transfer_tx(&b, &c, 1_000_000)],
            ..Default::default()
        };
        let results = apply_block(&mut ws, &block).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(ws.get_account(&a).unwrap().unwrap().balance, 6_000_000);
        assert_eq!(ws.get_account(&b).unwrap().unwrap().balance, 3_000_000);
        assert_eq!(ws.get_account(&c).unwrap().unwrap().balance, 1_000_000);
    }

    #[test]
    fn block_with_invalid_tx_fails_atomically_at_that_tx() {
        let (a, b) = (addr(1), addr(2));
        let mut ws = seeded(&a, 1_000_000);
        let block = protocol::Block {
            transactions: vec![
                transfer_tx(&a, &b, 500_000),
                transfer_tx(&a, &b, 10_000_000), // insufficient
            ],
            ..Default::default()
        };
        let err = apply_block(&mut ws, &block).unwrap_err();
        assert!(matches!(err, ActuatorError::Validate(m) if m.contains("not sufficient")));
    }

    #[test]
    fn process_block_applies_state_stores_block_and_indexes_txs() {
        let (a, b) = (addr(1), addr(2));
        let mut ws = seeded(&a, 10_000_000);
        let tx = transfer_tx(&a, &b, 3_000_000);
        let txid = tron_chain::tx_id(&tx);
        let block = protocol::Block {
            block_header: Some(protocol::BlockHeader {
                raw_data: Some(protocol::block_header::Raw { number: 1, ..Default::default() }),
                ..Default::default()
            }),
            transactions: vec![tx],
        };

        process_block(&mut ws, &block).unwrap();

        // state transition applied
        assert_eq!(ws.get_account(&b).unwrap().unwrap().balance, 3_000_000);
        // block persisted + head advanced
        assert_eq!(ws.get_block_by_num(1).unwrap().unwrap()
            .block_header.unwrap().raw_data.unwrap().number, 1);
        // transaction indexed by id
        assert!(ws.get_transaction(&txid.0).unwrap().is_some());
    }

    #[test]
    fn rejects_unsupported_contract_type() {
        let mut ws = WorldState::new(MemoryStore::new());
        let contract = protocol::transaction::Contract {
            r#type: ContractType::ShieldedTransferContract as i32,
            parameter: Some(prost_types::Any::default()),
            ..Default::default()
        };
        let tx = protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw {
                contract: vec![contract],
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = apply_transaction(&mut ws, &tx).unwrap_err();
        assert!(matches!(err, ActuatorError::Validate(m) if m.contains("unsupported contract type")));
    }

    #[test]
    fn rejects_contractless_and_rawless_txs() {
        let mut ws = WorldState::new(MemoryStore::new());
        let no_raw = protocol::Transaction::default();
        assert!(apply_transaction(&mut ws, &no_raw).is_err());
        let no_contract = protocol::Transaction {
            raw_data: Some(protocol::transaction::Raw::default()),
            ..Default::default()
        };
        assert!(apply_transaction(&mut ws, &no_contract).is_err());
    }
}
