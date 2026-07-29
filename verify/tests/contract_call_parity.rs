//! TVM contract-call differential parity (T03, SPEC §7).
//!
//! Proves our contract execution (`apply_transaction`, full execution after T02) is
//! deterministic and reproduces the exact SSTORE-heavy state delta of a TRC20-style
//! `transfer`, and anchors that against a REAL captured java-tron receipt.
//!
//! **Why two layers.** Absolute energy parity for a real SSTORE-heavy call needs the
//! call's *historical pre-state storage* (the sender/recipient balance slots at block
//! N-1). The standard gRPC surface exposes no archival `getStorageAt`, so that pre-state
//! is not in the committed fixtures — the same limit V02 documents for account state.
//! Therefore:
//!   1. `controlled_transfer_*` seeds a KNOWN pre-state and asserts our executor's exact
//!      energy + per-slot storage delta (deterministic, fully offline) — the real parity
//!      assertion, differential vs the seeded pre-state.
//!   2. `captured_receipt_*` loads the real `TransactionInfo` captured from nile
//!      (`capture_contract_case`) and asserts what is pre-state-INDEPENDENT (it is a
//!      successful TRC20 `transfer`, non-empty code, positive energy) and reports our
//!      controlled energy against java-tron's `energy_usage_total` as the parity report.
//!
//! Absolute energy parity for the captured tx is left to a full-chain replay / archive
//! node (SPEC §7 deviation, documented).

use prost::Message;
use tron_actuators::executor::apply_transaction;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::{cf, props, WorldState};
use tron_storage::{KvStore, MemoryStore};
use tron_types::Address;

/// TRC20 `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// Energy price we pin so `energy_used == fee / price` is exact.
const ENERGY_PRICE: i64 = 100;

/// Golden energy our VM charges for the controlled transfer below. Deterministic for
/// the pinned bytecode + calldata + seeded pre-state (slot1 nonzero -> nonzero,
/// slot2 zero -> nonzero). A change here means the VM's gas schedule moved (T01).
const CONTROLLED_TRANSFER_ENERGY: u64 = 25145;

// Storage slots the controlled contract touches.
const SLOT_FROM: u8 = 1;
const SLOT_TO: u8 = 2;

/// Minimal, deterministic transfer-like runtime: `slot1 -= amount; slot2 += amount;
/// return 1`, with `amount = CALLDATALOAD(4)` (i.e. the 32-byte word after a 4-byte
/// selector). SSTORE-heavy, exactly the core of a TRC20 `transfer`.
fn transfer_like_runtime() -> Vec<u8> {
    vec![
        0x60, 0x04, // PUSH1 4
        0x35, //       CALLDATALOAD        -> [amount]
        0x80, //       DUP1                -> [amount, amount]
        0x60, SLOT_FROM, // PUSH1 1
        0x54, //       SLOAD               -> [amount, amount, from_bal]
        0x03, //       SUB                 -> [amount, from_bal-amount]
        0x60, SLOT_FROM, // PUSH1 1
        0x55, //       SSTORE(slot1, from_bal-amount) -> [amount]
        0x60, SLOT_TO, // PUSH1 2
        0x54, //       SLOAD               -> [amount, to_bal]
        0x01, //       ADD                 -> [to_bal+amount]
        0x60, SLOT_TO, // PUSH1 2
        0x55, //       SSTORE(slot2, to_bal+amount) -> []
        0x60, 0x01, // PUSH1 1
        0x60, 0x00, // PUSH1 0
        0x52, //       MSTORE              mem[0]=1
        0x60, 0x20, // PUSH1 32
        0x60, 0x00, // PUSH1 0
        0xf3, //       RETURN mem[0..32]
    ]
}

fn storage_key(addr: &Address, slot: u8) -> Vec<u8> {
    let mut key = addr.as_bytes().to_vec(); // 21 bytes
    let mut slot_be = [0u8; 32];
    slot_be[31] = slot;
    key.extend_from_slice(&slot_be);
    key
}

fn seed_slot(ws: &WorldState<MemoryStore>, addr: &Address, slot: u8, value: u64) {
    let mut val_be = [0u8; 32];
    val_be[24..].copy_from_slice(&value.to_be_bytes());
    ws.db.put(cf::CONTRACT_STORAGE, &storage_key(addr, slot), &val_be).unwrap();
}

fn read_slot(ws: &WorldState<MemoryStore>, addr: &Address, slot: u8) -> u64 {
    match ws.db.get(cf::CONTRACT_STORAGE, &storage_key(addr, slot)).unwrap() {
        Some(b) if b.len() == 32 => {
            let mut last8 = [0u8; 8];
            last8.copy_from_slice(&b[24..]);
            u64::from_be_bytes(last8)
        }
        _ => 0,
    }
}

/// Build a real `TriggerSmartContract` tx (Any-packed) calling `contract` with
/// `transfer`-shaped calldata carrying `amount`.
fn trigger_tx(owner: &Address, contract: &Address, amount: u64) -> protocol::Transaction {
    let mut data = TRANSFER_SELECTOR.to_vec();
    let mut amount_be = [0u8; 32];
    amount_be[24..].copy_from_slice(&amount.to_be_bytes());
    data.extend_from_slice(&amount_be);

    let trigger = protocol::TriggerSmartContract {
        owner_address: owner.as_bytes().to_vec(),
        contract_address: contract.as_bytes().to_vec(),
        data,
        ..Default::default()
    };
    let any = prost_types::Any {
        type_url: "type.googleapis.com/protocol.TriggerSmartContract".into(),
        value: trigger.encode_to_vec(),
    };
    protocol::Transaction {
        raw_data: Some(protocol::transaction::Raw {
            contract: vec![protocol::transaction::Contract {
                r#type: ContractType::TriggerSmartContract as i32,
                parameter: Some(any),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn seed_state(owner: &Address, contract: &Address) -> WorldState<MemoryStore> {
    let ws = WorldState::new(MemoryStore::new());
    ws.put_prop_i64("ENERGY_FEE", ENERGY_PRICE).unwrap();
    ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 0).unwrap();
    ws.put_account(
        owner,
        &protocol::Account { address: owner.as_bytes().to_vec(), balance: 1_000_000_000, ..Default::default() },
    )
    .unwrap();
    ws.put_code(contract, &transfer_like_runtime()).unwrap();
    ws
}

#[test]
fn controlled_transfer_energy_and_storage_delta_are_deterministic() {
    let owner = Address::from_body([0x11; 20]);
    let contract = Address::from_body([0xcc; 20]);
    let from_pre: u64 = 1_000;
    let amount: u64 = 40;

    let ws = seed_state(&owner, &contract);
    seed_slot(&ws, &contract, SLOT_FROM, from_pre); // sender starts with 1000
    // SLOT_TO starts at 0 (recipient is new): zero -> nonzero SSTORE.

    let mut ws = ws;
    let tx = trigger_tx(&owner, &contract, amount);
    let res = apply_transaction(&mut ws, &tx).expect("controlled transfer must execute");

    // Energy: fee == energy_used * price, so energy_used == fee / price (exact).
    assert_eq!(res.fee % ENERGY_PRICE, 0, "fee must be a whole multiple of the energy price");
    let energy_used = (res.fee / ENERGY_PRICE) as u64;
    println!("controlled transfer: energy_used = {energy_used} (fee {} sun)", res.fee);
    assert_eq!(
        energy_used, CONTROLLED_TRANSFER_ENERGY,
        "VM energy for the transfer changed (gas schedule moved?)"
    );

    // Storage-delta parity: the exact SSTORE-heavy delta of a transfer.
    let from_post = read_slot(&ws, &contract, SLOT_FROM);
    let to_post = read_slot(&ws, &contract, SLOT_TO);
    assert_eq!(from_post, from_pre - amount, "sender slot must drop by amount");
    assert_eq!(to_post, amount, "recipient slot must rise by amount");

    // Owner paid exactly the energy fee (burned).
    let owner_bal = ws.get_account(&owner).unwrap().unwrap().balance;
    assert_eq!(owner_bal, 1_000_000_000 - res.fee, "owner charged exactly the energy fee");
    assert_eq!(ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap(), res.fee, "energy fee is burned");
}

#[test]
fn controlled_transfer_touched_slot_set_is_nonempty_and_scales_with_amount() {
    // A second amount to show the delta tracks the calldata, not a constant.
    let owner = Address::from_body([0x22; 20]);
    let contract = Address::from_body([0xcd; 20]);
    let from_pre: u64 = 5_000;
    let amount: u64 = 1_234;

    let ws = seed_state(&owner, &contract);
    seed_slot(&ws, &contract, SLOT_FROM, from_pre);
    let mut ws = ws;

    apply_transaction(&mut ws, &trigger_tx(&owner, &contract, amount)).unwrap();

    let touched: Vec<u8> = [SLOT_FROM, SLOT_TO]
        .into_iter()
        .filter(|&s| read_slot(&ws, &contract, s) != 0)
        .collect();
    assert!(!touched.is_empty(), "at least one storage slot must be touched");
    assert_eq!(read_slot(&ws, &contract, SLOT_FROM), from_pre - amount);
    assert_eq!(read_slot(&ws, &contract, SLOT_TO), amount);
}

#[test]
fn captured_receipt_is_a_wellformed_successful_transfer_and_energy_reported() {
    let cases = tron_verify::contract_cases().expect("read contract fixtures");
    if cases.is_empty() {
        println!(
            "no captured contract-call fixtures committed; run `capture_contract_case --discover` \
             against a live node to add one. Controlled parity still covers execution offline."
        );
        return;
    }

    for case in &cases {
        // The captured tx is a real TRC20 transfer TriggerSmartContract.
        let raw = case.tx.raw_data.as_ref().expect("tx has raw_data");
        let contract = raw.contract.first().expect("tx has a contract");
        assert_eq!(
            contract.r#type(),
            ContractType::TriggerSmartContract,
            "{}: captured tx must be a TriggerSmartContract",
            case.label
        );
        let trigger = protocol::TriggerSmartContract::decode(
            contract.parameter.as_ref().unwrap().value.as_slice(),
        )
        .expect("TriggerSmartContract decodes");
        assert!(
            trigger.data.len() >= 4 && trigger.data[..4] == TRANSFER_SELECTOR,
            "{}: captured call must be a TRC20 transfer",
            case.label
        );

        // Ground-truth receipt: java-tron reported success and positive energy.
        assert_eq!(case.info.result, 0, "{}: captured receipt result must be SUCESS", case.label);
        let energy_total = case.info.receipt.as_ref().map(|r| r.energy_usage_total).unwrap_or(0);
        assert!(energy_total > 0, "{}: captured receipt must report energy", case.label);
        assert!(!case.code.is_empty(), "{}: captured contract code must be non-empty", case.label);

        // Parity report (SPEC §7): our controlled transfer energy vs java-tron's real
        // number. These are NOT expected to be equal — the real call ran on the
        // contract's historical storage (unknown offline). We report the figures.
        println!(
            "[parity report] {}: java-tron energy_usage_total={energy_total}, \
             our controlled-transfer energy={CONTROLLED_TRANSFER_ENERGY}, code={} bytes. \
             Absolute parity for the captured tx needs archival pre-state (SPEC §7).",
            case.label,
            case.code.len()
        );
    }
}
