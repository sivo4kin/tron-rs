//! T10 — contract-call pre-state fixtures: format round-trip + the committed sample
//! drives a deterministic execution (the seam T11 asserts absolute parity on).
//!
//! Source availability (documented deviation): neither pre-state source yields the real
//! nile tx's N-1 state offline —
//!   * archive: public TronGrid JSON-RPC is latest-only (`eth_getStorageAt` rejects a
//!     historical QUANTITY block), so `capture_contract_case --prestate` reports the
//!     limitation and writes nothing;
//!   * replay: reconstructing state to block ~69.6M via `apply_block` is infeasible
//!     offline.
//! So T10 lands the capture/loader plumbing, the documented `*.prestate.pb` format, and a
//! hand-verified sample (`controlled-transfer.prestate.pb`) for the T03 controlled
//! contract — enabling T11's absolute-parity gate to run fully offline against committed
//! data.

use prost::Message;
use tron_actuators::executor::apply_transaction;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::{cf, WorldState};
use tron_storage::{KvStore, MemoryStore};
use tron_types::Address;
use tron_verify::PreState;

#[test]
fn prestate_fixture_roundtrips() {
    let mut slot = [0u8; 32];
    slot[31] = 7;
    let mut value = [0u8; 32];
    value[24..].copy_from_slice(&123_456u64.to_be_bytes());

    let ps = PreState {
        contract: Address::from_body([0xcc; 20]).as_bytes().to_vec(),
        block_number: 69_604_276,
        storage: vec![(slot, value), ([0xff; 32], [0x01; 32])],
        accounts: vec![
            (Address::from_body([0x11; 20]).as_bytes().to_vec(), 1_000),
            (Address::from_body([0x22; 20]).as_bytes().to_vec(), -1),
        ],
        source: "archive:https://example/jsonrpc".into(),
    };

    let bytes = ps.encode();
    let back = PreState::decode(&bytes).expect("decodes");
    assert_eq!(ps, back, "pre-state must round-trip byte-for-byte");
    // Truncated input is rejected, not silently accepted.
    assert!(PreState::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(PreState::decode(b"XXXX").is_err(), "bad magic rejected");
}

#[test]
fn mapping_slot_matches_known_solidity_layout() {
    // keccak256(pad32(addr) || pad32(slot)) — the canonical mapping(address=>_) key.
    // Address 0x00..00, mapping slot 0 -> a well-known constant.
    let slot = tron_verify::mapping_slot(&[0u8; 20], 0);
    assert_eq!(
        hex::encode(slot),
        "ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"
    );
}

/// The committed sample pre-state, loaded and used to seed a world state, drives the
/// T03 controlled transfer-like contract to a DETERMINISTIC result — the exact seam T11
/// asserts absolute energy + storage parity on, fully offline.
#[test]
fn committed_sample_prestate_seeds_a_deterministic_execution() {
    let ps = tron_verify::load_prestate("controlled-transfer").expect("sample fixture present");
    assert!(ps.source.contains("hand-verified"), "sample is flagged hand-verified");

    let contract = Address::from_bytes(ps.contract.as_slice().try_into().unwrap()).unwrap();
    let owner = Address::from_body([0x11; 20]);

    // Seed a world from the pre-state fixture: contract storage slots + accounts.
    let ws = WorldState::new(MemoryStore::new());
    for (slot, value) in &ps.storage {
        let mut key = contract.as_bytes().to_vec();
        key.extend_from_slice(slot);
        ws.db.put(cf::CONTRACT_STORAGE, &key, value).unwrap();
    }
    for (addr, bal) in &ps.accounts {
        let a = Address::from_bytes(addr.as_slice().try_into().unwrap()).unwrap();
        ws.put_account(&a, &protocol::Account { address: a.as_bytes().to_vec(), balance: *bal, ..Default::default() })
            .unwrap();
    }
    // Deploy the controlled contract code + the pieces the executor needs.
    ws.put_prop_i64("ENERGY_FEE", 100).unwrap();
    ws.put_code(&contract, &transfer_like_runtime()).unwrap();
    ws.put_account(
        &owner,
        &protocol::Account { address: owner.as_bytes().to_vec(), balance: 1_000_000_000, ..Default::default() },
    )
    .unwrap();

    // The pre-state seeded slot 1 (sender balance) = 1000. Transfer 40.
    assert_eq!(read_slot(&ws, &contract, 1), 1000, "sample seeded the sender balance slot");

    let mut ws = ws;
    let res = apply_transaction(&mut ws, &trigger_tx(&owner, &contract, 40)).unwrap();

    // Deterministic: exact energy (25145) and exact storage delta from the seeded slot.
    let energy_used = (res.fee / 100) as u64;
    assert_eq!(energy_used, 25145, "energy is deterministic from the committed pre-state");
    assert_eq!(read_slot(&ws, &contract, 1), 960, "sender slot -= amount");
    assert_eq!(read_slot(&ws, &contract, 2), 40, "recipient slot += amount");
}

// --- controlled contract helpers (mirror T03's) --------------------------------

fn transfer_like_runtime() -> Vec<u8> {
    vec![
        0x60, 0x04, 0x35, 0x80, 0x60, 0x01, 0x54, 0x03, 0x60, 0x01, 0x55, 0x60, 0x02, 0x54, 0x01,
        0x60, 0x02, 0x55, 0x60, 0x01, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ]
}

fn read_slot(ws: &WorldState<MemoryStore>, addr: &Address, slot: u8) -> u64 {
    let mut key = addr.as_bytes().to_vec();
    let mut slot_be = [0u8; 32];
    slot_be[31] = slot;
    key.extend_from_slice(&slot_be);
    match ws.db.get(cf::CONTRACT_STORAGE, &key).unwrap() {
        Some(b) if b.len() == 32 => {
            let mut last8 = [0u8; 8];
            last8.copy_from_slice(&b[24..]);
            u64::from_be_bytes(last8)
        }
        _ => 0,
    }
}

fn trigger_tx(owner: &Address, contract: &Address, amount: u64) -> protocol::Transaction {
    let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
    let mut amount_be = [0u8; 32];
    amount_be[24..].copy_from_slice(&amount.to_be_bytes());
    data.extend_from_slice(&amount_be);
    let trigger = protocol::TriggerSmartContract {
        owner_address: owner.as_bytes().to_vec(),
        contract_address: contract.as_bytes().to_vec(),
        data,
        ..Default::default()
    };
    protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw {
            contract: vec![protocol::transaction::Contract {
                r#type: ContractType::TriggerSmartContract as i32,
                parameter: Some(prost_types::Any {
                    type_url: "type.googleapis.com/protocol.TriggerSmartContract".into(),
                    value: trigger.encode_to_vec(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}
