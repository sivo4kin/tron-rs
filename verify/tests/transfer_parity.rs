//! Per-actuator state-DELTA parity for `TransferContract` (V02, SPEC §7).
//!
//! Absolute historical pre-state is not in the committed fixtures — deriving it needs
//! a whole-chain replay (or an archive node with historical `getAccount`, which the
//! standard gRPC surface does not offer). So this is a **differential** parity gate:
//! for every real `TransferContract` in the committed nile/mainnet block fixtures we
//! seed a KNOWN pre-state and assert our executor produces EXACTLY the per-account
//! delta java-tron's `TransferActuator` is defined to produce:
//!
//! ```text
//!   sender.balance   -= amount + fee
//!   receiver.balance += amount
//!   BURN_TRX_AMOUNT  += fee      (fee = create-account fee iff the receiver is new, else 0)
//! ```
//!
//! Asserting the per-account deltas is strictly stronger than the sum-conservation
//! check in `executor_replay.rs`: it pins *which* account moved by *how much*, i.e. the
//! actuator reproduces the real state delta, not merely a conserved total.
//!
//! **Scope / deviation (SPEC §7):** only the fields `TransferActuator` owns are asserted
//! — `balance` and the burn counter, differential vs the seeded pre-state. Absolute
//! post-state, and unrelated fields java-tron also touches per block (bandwidth/energy
//! usage decay, `latest_operation_time`, etc.), are out of this test's scope.

use prost::Message;
use tron_actuators::executor::apply_transaction;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::{props, WorldState};
use tron_storage::MemoryStore;
use tron_types::{Address, ADDRESS_LEN};

const CREATE_FEE: i64 = 1_000_000; // committee create-account fee used for the seed

fn addr_of(bytes: &[u8]) -> Option<Address> {
    let arr: [u8; ADDRESS_LEN] = bytes.try_into().ok()?;
    Address::from_bytes(arr).ok()
}

/// Every real `TransferContract` across the committed fixtures, decoded with its
/// owner/receiver parsed and self-transfers (which java-tron rejects) filtered out.
fn real_transfers() -> Vec<(String, protocol::TransferContract, Address, Address)> {
    let mut out = Vec::new();
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        for tx in &block.transactions {
            let Some(raw) = tx.raw_data.as_ref() else { continue };
            let Some(contract) = raw.contract.first() else { continue };
            if contract.r#type() != ContractType::TransferContract {
                continue;
            }
            let c = protocol::TransferContract::decode(
                contract.parameter.as_ref().unwrap().value.as_slice(),
            )
            .expect("real TransferContract must decode");
            let (Some(owner), Some(to)) = (addr_of(&c.owner_address), addr_of(&c.to_address)) else {
                continue;
            };
            if owner == to {
                continue; // self-transfer is rejected by the actuator; not a delta case
            }
            out.push((name.clone(), c, owner, to));
        }
    }
    out
}

fn seed(owner: &Address, balance: i64) -> WorldState<MemoryStore> {
    let ws = WorldState::new(MemoryStore::new());
    ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, CREATE_FEE).unwrap();
    ws.put_account(
        owner,
        &protocol::Account { address: owner.as_bytes().to_vec(), balance, ..Default::default() },
    )
    .unwrap();
    ws
}

/// Rebuild each real transfer's tx from its fixture (so the REAL Any-packing is exercised).
fn find_tx(name: &str, want: &protocol::TransferContract) -> protocol::Transaction {
    let block = tron_verify::load_block(name).unwrap();
    for tx in block.transactions {
        let Some(raw) = tx.raw_data.as_ref() else { continue };
        let Some(contract) = raw.contract.first() else { continue };
        if contract.r#type() != ContractType::TransferContract {
            continue;
        }
        if let Ok(c) = protocol::TransferContract::decode(
            contract.parameter.as_ref().unwrap().value.as_slice(),
        ) {
            if &c == want {
                return tx;
            }
        }
    }
    panic!("transfer not found back in {name}");
}

#[test]
fn transfer_to_existing_receiver_moves_exactly_amount_no_fee() {
    let transfers = real_transfers();
    let mut checked = 0u32;
    for (name, c, owner, to) in &transfers {
        let sender_pre = c.amount.saturating_add(CREATE_FEE).saturating_add(500);
        let receiver_pre = 777i64; // nonzero -> the receiver already EXISTS, so fee = 0
        let mut ws = seed(owner, sender_pre);
        ws.put_account(
            to,
            &protocol::Account { address: to.as_bytes().to_vec(), balance: receiver_pre, ..Default::default() },
        )
        .unwrap();

        let tx = find_tx(name, c);
        let res = apply_transaction(&mut ws, &tx)
            .unwrap_or_else(|e| panic!("real transfer in {name} rejected: {e}"));

        let sender_post = ws.get_account(owner).unwrap().unwrap().balance;
        let receiver_post = ws.get_account(to).unwrap().unwrap().balance;
        let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();

        // Existing receiver -> no create fee, nothing burned.
        assert_eq!(res.fee, 0, "existing receiver must not be charged a create fee ({name})");
        assert_eq!(burned, 0, "nothing should burn for an existing receiver ({name})");
        // The exact per-account delta.
        assert_eq!(sender_post, sender_pre - c.amount, "sender delta != -amount ({name})");
        assert_eq!(receiver_post, receiver_pre + c.amount, "receiver delta != +amount ({name})");
        checked += 1;
    }
    println!("delta-parity checked {checked} real transfers (existing receiver)");
    assert!(checked > 10, "expected a meaningful corpus, got {checked}");
}

#[test]
fn transfer_to_new_receiver_charges_and_burns_create_fee() {
    let transfers = real_transfers();
    let mut checked = 0u32;
    for (name, c, owner, to) in &transfers {
        let sender_pre = c.amount.saturating_add(CREATE_FEE).saturating_add(500);
        // Seed ONLY the sender; the receiver is absent -> create-account path (fee charged).
        let mut ws = seed(owner, sender_pre);
        assert!(!ws.account_exists(to).unwrap());

        let tx = find_tx(name, c);
        let res = apply_transaction(&mut ws, &tx)
            .unwrap_or_else(|e| panic!("real transfer in {name} rejected: {e}"));

        let sender_post = ws.get_account(owner).unwrap().unwrap().balance;
        let receiver_post = ws.get_account(to).unwrap().unwrap().balance;
        let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();

        // New receiver -> fee = create fee, which is burned.
        assert_eq!(res.fee, CREATE_FEE, "new receiver must be charged the create fee ({name})");
        assert_eq!(burned, CREATE_FEE, "the create fee must be burned ({name})");
        // Exact per-account delta: sender pays amount + fee; receiver is created with amount.
        assert_eq!(sender_post, sender_pre - c.amount - CREATE_FEE, "sender delta ({name})");
        assert_eq!(receiver_post, c.amount, "new receiver should hold exactly amount ({name})");
        checked += 1;
    }
    println!("delta-parity checked {checked} real transfers (new receiver)");
    assert!(checked > 10, "expected a meaningful corpus, got {checked}");
}
