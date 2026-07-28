//! End-to-end pipeline: genesis -> apply block -> serve via HTTP handler.
//!
//! Ties P1 (genesis init + executor) to P4 (the tron-openapi HTTP surface) in a
//! single flow: seed a genesis state with funded accounts, apply a block of real
//! transfer transactions through the executor, then query balances via the same
//! JSON handler the node serves — proving the layers compose correctly.

use prost::Message;
use serde_json::json;
use tron_actuators::executor::apply_block;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::genesis::{apply_genesis, GenesisAccount, GenesisConfig};
use tron_state::WorldState;
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
    protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw {
            contract: vec![protocol::transaction::Contract {
                r#type: ContractType::TransferContract as i32,
                parameter: Some(prost_types::Any {
                    type_url: "type.googleapis.com/protocol.TransferContract".into(),
                    value: c.encode_to_vec(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn genesis_apply_block_serve_balances() {
    // 1. Genesis: fund alice with 100 TRX, bob with 0.
    let (alice, bob, carol) = (addr(1), addr(2), addr(3));
    let mut ws = WorldState::new(MemoryStore::new());
    apply_genesis(
        &mut ws,
        &GenesisConfig {
            timestamp: 1_700_000_000_000,
            accounts: vec![
                GenesisAccount { address: alice, name: "alice".into(), balance: 100_000_000 },
                GenesisAccount { address: bob, name: "bob".into(), balance: 0 },
            ],
            witnesses: vec![],
        },
    )
    .unwrap();

    // 2. Apply a block: alice -> bob 30 TRX, then bob -> carol 10 TRX.
    let block = protocol::Block {
        transactions: vec![
            transfer_tx(&alice, &bob, 30_000_000),
            transfer_tx(&bob, &carol, 10_000_000),
        ],
        ..Default::default()
    };
    let results = apply_block(&mut ws, &block).unwrap();
    assert_eq!(results.len(), 2);

    // 3. Serve balances via the HTTP handler (the tron-openapi contract).
    let alice_resp = tron_rpc::http::get_account(&ws, &json!({ "address": alice.to_hex() }));
    let bob_resp = tron_rpc::http::get_account(&ws, &json!({ "address": bob.to_base58check(), "visible": true }));
    let carol_resp = tron_rpc::http::get_account(&ws, &json!({ "address": carol.to_hex() }));

    assert_eq!(alice_resp["balance"], 70_000_000); // 100 - 30
    assert_eq!(bob_resp["balance"], 20_000_000); //   0 + 30 - 10
    assert_eq!(carol_resp["balance"], 10_000_000); //  0 + 10
    // visible rendering carried through the handler
    assert!(bob_resp["address"].as_str().unwrap().starts_with('T'));

    // 4. Value conservation across the whole pipeline (fees are 0 for existing targets;
    //    carol was created implicitly — its create fee defaulted to 0 in genesis).
    let total: i64 = [alice_resp, bob_resp, carol_resp]
        .iter()
        .map(|r| r["balance"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 100_000_000);
}
