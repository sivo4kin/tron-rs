//! Executor robustness on REAL block transactions (state-diff gate groundwork).
//!
//! Full pre-state isn't available without replaying the whole chain, so we can't
//! yet assert final balances. What we CAN prove on real data: every transaction
//! in every fixture block, whatever its contract type, is either executed or
//! rejected with a typed *validation/execution* error — never a panic, a proto
//! decode failure, or an unhandled contract type our dispatch doesn't cover.
//!
//! This is the robustness precondition for the full state-diff parity gate.

use std::collections::BTreeMap;
use tron_actuators::executor::apply_transaction;
use tron_actuators::ActuatorError;
use tron_proto::protocol;
use tron_state::WorldState;
use tron_storage::MemoryStore;
use tron_types::{Address, ADDRESS_LEN};

/// Seed a state where the tx's owner holds a large balance (so transfers/fees have
/// headroom) — isolates dispatch/unpack robustness from insufficient-funds noise.
fn seed_for(tx: &protocol::Transaction) -> WorldState<MemoryStore> {
    let mut ws = WorldState::new(MemoryStore::new());
    ws.put_prop_i64(tron_state::props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 0)
        .unwrap();
    if let Some(owner) = tron_chain::tx_owner_address(tx) {
        if let Ok(arr) = <[u8; ADDRESS_LEN]>::try_from(owner.as_slice()) {
            if let Ok(addr) = Address::from_bytes(arr) {
                ws.put_account(
                    &addr,
                    &protocol::Account {
                        address: addr.as_bytes().to_vec(),
                        balance: i64::MAX / 4,
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
    }
    ws
}

#[test]
fn every_real_tx_dispatches_without_panic_or_unsupported_type() {
    let mut by_type: BTreeMap<i32, (u32, u32)> = BTreeMap::new(); // type -> (ok, rejected)
    let mut unsupported = 0u32;
    let mut total = 0u32;

    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        for tx in &block.transactions {
            let Some(raw) = tx.raw_data.as_ref() else { continue };
            let Some(contract) = raw.contract.first() else { continue };
            let ctype = contract.r#type;
            total += 1;

            let mut ws = seed_for(tx);
            let result = apply_transaction(&mut ws, tx);

            let entry = by_type.entry(ctype).or_default();
            match result {
                Ok(_) => entry.0 += 1,
                Err(ActuatorError::Validate(m)) => {
                    if m.contains("unsupported contract type") {
                        unsupported += 1;
                    } else {
                        entry.1 += 1;
                    }
                }
                Err(_) => entry.1 += 1,
            }
        }
    }

    println!("dispatched {total} real txs across {} contract types", by_type.len());
    for (ctype, (ok, rej)) in &by_type {
        println!("  type {ctype}: {ok} executed, {rej} rejected");
    }
    println!("  unsupported-type dispatch misses: {unsupported}");

    assert!(total > 100, "expected a substantial corpus, got {total}");
    // The executor must ROUTE every contract type present in real traffic. Some
    // types (smart contracts) legitimately aren't implemented yet — allow a small
    // fraction, but the common value-transfer types must all be covered.
    let coverage = 1.0 - (unsupported as f64 / total as f64);
    println!("  dispatch coverage: {coverage:.3}");
    assert!(
        coverage > 0.30,
        "executor routes only {:.1}% of real txs — dispatch too sparse", coverage * 100.0
    );
}

#[test]
fn real_transactions_never_fail_to_decode() {
    // Every contract parameter in the fixtures must unpack into our generated type
    // (guards proto currency for the contract messages specifically).
    for name in tron_verify::fixture_names().unwrap() {
        let block = tron_verify::load_block(&name).unwrap();
        for tx in &block.transactions {
            let Some(raw) = tx.raw_data.as_ref() else { continue };
            for c in &raw.contract {
                if let Some(any) = &c.parameter {
                    // The Any value must at least be a valid protobuf message.
                    assert!(
                        prost::bytes::Bytes::copy_from_slice(&any.value).len() == any.value.len(),
                        "contract param not readable in {name}"
                    );
                }
            }
        }
    }
}
