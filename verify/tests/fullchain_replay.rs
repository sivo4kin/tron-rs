//! Contiguous full-chain EXECUTION replay at scale (V01, SPEC §7).
//!
//! `chain_replay.rs` proves the contiguous run *validates and chain-links*; this test
//! feeds the same longest contiguous run of real Nile blocks through the **executor**
//! in order, accumulating into one `WorldState`, and asserts the invariants that hold
//! on real data without an absolute pre-state:
//!
//! - **structural validation** passes for every block at scale;
//! - **dispatch robustness**: every transaction either executes or is rejected with a
//!   typed `ActuatorError` — never a panic or a proto-decode failure (the block_apply
//!   invariant, but here in-order over an accumulating state);
//! - **dispatch coverage** (fraction routed to an actuator) stays at/above the current
//!   bar, and is reported;
//! - **value invariant**: every executed `TransferContract` conserves value across the
//!   run — `Δsender + Δreceiver + Δburn == 0`;
//! - **burn monotonicity**: `BURN_TRX_AMOUNT` never decreases (fees are never refunded);
//! - **weight invariant**: the global staked-weight totals never go negative.
//!
//! **Why invariants, not absolute state (SPEC §7):** Tron blocks carry no header state
//! root, and full historical pre-state isn't in the committed fixtures, so we assert
//! these run-wide invariants rather than absolute post-balances. Owners are funded
//! just-in-time (their non-balance fields preserved) so execution isn't drowned in
//! insufficient-funds rejections — the documented synthetic-seed approach, at scale.
//!
//! **Scale note:** run OFFLINE against the committed fixtures. No new blocks were
//! captured (no live gRPC endpoint here); the assertions scale over the existing
//! longest contiguous Nile run (`nile-69590116..69590139`, 24 blocks). To extend the
//! range, capture more with `src/bin/capture.rs` and commit them — the test picks up
//! any longer contiguous run automatically.

use std::collections::BTreeMap;
use tron_actuators::executor::apply_transaction;
use tron_actuators::ActuatorError;
use tron_consensus::validation::{validate_block, ValidationOptions};
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::{props, WorldState};
use tron_storage::MemoryStore;
use tron_types::{Address, ADDRESS_LEN};

use prost::Message;

/// The longest contiguous ascending run of `nile-<n>` block fixtures.
fn contiguous_nile_blocks() -> Vec<(i64, protocol::Block)> {
    let mut blocks: Vec<(i64, protocol::Block)> = tron_verify::fixture_names()
        .unwrap()
        .into_iter()
        .filter_map(|name| {
            let num: i64 = name.strip_prefix("nile-")?.parse().ok()?;
            Some((num, tron_verify::load_block(&name).unwrap()))
        })
        .collect();
    blocks.sort_by_key(|(n, _)| *n);
    let mut best: Vec<(i64, protocol::Block)> = Vec::new();
    let mut run: Vec<(i64, protocol::Block)> = Vec::new();
    for (n, b) in blocks {
        if run.last().map(|(p, _)| *p + 1 == n).unwrap_or(true) {
            run.push((n, b));
        } else {
            if run.len() > best.len() {
                best = std::mem::take(&mut run);
            }
            run = vec![(n, b)];
        }
    }
    if run.len() > best.len() {
        best = run;
    }
    best
}

fn addr_of(bytes: &[u8]) -> Option<Address> {
    let arr: [u8; ADDRESS_LEN] = bytes.try_into().ok()?;
    Address::from_bytes(arr).ok()
}

fn balance(ws: &WorldState<MemoryStore>, addr: &Address) -> i64 {
    ws.get_account(addr).unwrap().map(|a| a.balance).unwrap_or(0)
}

/// Top the tx owner's balance up to `floor` if below, preserving every other account
/// field (frozen_v2, votes, …) so accumulated state survives across the run.
fn fund_owner(ws: &mut WorldState<MemoryStore>, tx: &protocol::Transaction, floor: i64) {
    let Some(owner) = tron_chain::tx_owner_address(tx) else { return };
    let Some(addr) = addr_of(&owner) else { return };
    let mut acc = ws.get_account(&addr).unwrap().unwrap_or(protocol::Account {
        address: addr.as_bytes().to_vec(),
        ..Default::default()
    });
    if acc.balance < floor {
        acc.balance = floor;
    }
    ws.put_account(&addr, &acc).unwrap();
}

#[test]
fn fullchain_execution_replay_invariants_and_coverage() {
    let blocks = contiguous_nile_blocks();
    assert!(blocks.len() >= 20, "need the scaled contiguous run, got {}", blocks.len());

    let mut ws = WorldState::new(MemoryStore::new());
    // create-account fee 0 keeps the run focused on dispatch + invariants (V02 covers
    // the create-fee delta specifically).
    ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, 0).unwrap();
    let floor = i64::MAX / 4;

    let mut total = 0u32;
    let mut executed = 0u32;
    let mut rejected = 0u32;
    let mut unsupported = 0u32;
    let mut transfers_conserved = 0u32;
    let mut by_type: BTreeMap<i32, (u32, u32)> = BTreeMap::new();

    for (num, block) in &blocks {
        validate_block(block, ValidationOptions { require_witness_signature: false })
            .unwrap_or_else(|e| panic!("block {num} failed structural validation: {e}"));

        for tx in &block.transactions {
            let Some(raw) = tx.raw_data.as_ref() else { continue };
            let Some(contract) = raw.contract.first() else { continue };
            let ctype = contract.r#type;
            total += 1;

            fund_owner(&mut ws, tx, floor);

            // Capture pre-state for a value-conservation check on transfers.
            let pre = if contract.r#type() == ContractType::TransferContract {
                protocol::TransferContract::decode(contract.parameter.as_ref().unwrap().value.as_slice())
                    .ok()
                    .and_then(|c| {
                        let (o, t) = (addr_of(&c.owner_address)?, addr_of(&c.to_address)?);
                        if o == t {
                            return None; // self-transfer is rejected; not a conservation case
                        }
                        Some((o, t, balance(&ws, &o), balance(&ws, &t)))
                    })
            } else {
                None
            };

            let burn_before = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
            match apply_transaction(&mut ws, tx) {
                Ok(_) => {
                    executed += 1;
                    by_type.entry(ctype).or_default().0 += 1;
                    if let Some((o, t, ob, tb)) = pre {
                        let burn_after = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
                        let d_owner = balance(&ws, &o) - ob;
                        let d_to = balance(&ws, &t) - tb;
                        let d_burn = burn_after - burn_before;
                        assert_eq!(
                            d_owner + d_to + d_burn,
                            0,
                            "transfer value not conserved in block {num}"
                        );
                        transfers_conserved += 1;
                    }
                }
                Err(ActuatorError::Validate(m)) if m.contains("unsupported contract type") => {
                    unsupported += 1;
                }
                Err(_) => {
                    rejected += 1;
                    by_type.entry(ctype).or_default().1 += 1;
                }
            }

            // Burn monotonicity: fees are never refunded.
            let burn_after = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
            assert!(burn_after >= burn_before, "burn counter decreased in block {num}");
        }
    }

    // Weight invariant: global staked-weight totals never go negative.
    for key in [props::TOTAL_NET_WEIGHT, props::TOTAL_ENERGY_WEIGHT, props::TOTAL_TRON_POWER_WEIGHT] {
        assert!(ws.get_prop_i64(key).unwrap() >= 0, "global weight {key} went negative");
    }

    let coverage = 1.0 - (unsupported as f64 / total.max(1) as f64);
    println!(
        "fullchain: {} contiguous blocks, {total} txs -> {executed} executed, {rejected} rejected, {unsupported} unsupported",
        blocks.len()
    );
    println!("  dispatch coverage: {coverage:.3}");
    println!("  transfers value-conserved: {transfers_conserved}");
    for (ct, (ok, rej)) in &by_type {
        println!("  contract type {ct}: {ok} executed, {rej} rejected");
    }

    assert!(total > 40, "expected a substantial in-order corpus, got {total}");
    assert!(coverage >= 0.30, "dispatch coverage {coverage:.3} fell below the current bar");
    assert!(transfers_conserved > 5, "expected several conserved transfers, got {transfers_conserved}");
}
