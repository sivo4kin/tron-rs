//! Replay REAL transactions from live-chain fixtures through our executor.
//!
//! Historical pre-state is not available without full replay, so each transfer is
//! applied against a synthetic pre-state seeded with enough balance. What this
//! proves on real data (vs the synthetic executor unit tests):
//! - real `Any` packing (type_url variants) unpacks into the right contract,
//! - real 21-byte addresses parse,
//! - our TransferActuator accepts every transfer java-tron accepted on-chain,
//! - value conservation holds on every replayed tx.

use prost::Message;
use tron_actuators::executor::apply_transaction;
use tron_proto::protocol;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_state::{props, WorldState};
use tron_storage::MemoryStore;
use tron_types::{Address, ADDRESS_LEN};

fn addr_of(bytes: &[u8]) -> Option<Address> {
    let arr: [u8; ADDRESS_LEN] = bytes.try_into().ok()?;
    Address::from_bytes(arr).ok()
}

#[test]
fn real_transfer_txs_execute_and_conserve_value() {
    let mut replayed = 0u32;
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
            let owner = addr_of(&c.owner_address).expect("real owner address must parse");
            let to = addr_of(&c.to_address).expect("real to address must parse");

            // Seed a synthetic pre-state: owner holds amount + fee headroom.
            let mut ws = WorldState::new(MemoryStore::new());
            let create_fee = 1_000_000; // committee value on mainnet
            ws.put_prop_i64(props::CREATE_NEW_ACCOUNT_FEE_IN_SYSTEM_CONTRACT, create_fee)
                .unwrap();
            let seed = c.amount.saturating_add(create_fee).saturating_add(1);
            ws.put_account(
                &owner,
                &protocol::Account {
                    address: owner.as_bytes().to_vec(),
                    balance: seed,
                    ..Default::default()
                },
            )
            .unwrap();

            // The REAL tx (its real Any packing) must execute.
            let res = apply_transaction(&mut ws, tx)
                .unwrap_or_else(|e| panic!("real transfer in {name} rejected: {e}"));

            // Conservation: owner + to + burned == seed.
            let ob = ws.get_account(&owner).unwrap().unwrap().balance;
            let tb = ws.get_account(&to).unwrap().unwrap().balance;
            let burned = ws.get_prop_i64(props::BURN_TRX_AMOUNT).unwrap();
            assert_eq!(ob + tb + burned, seed, "value not conserved in {name}");
            assert_eq!(res.fee, burned, "fee accounting mismatch in {name}");
            replayed += 1;
        }
    }
    println!("replayed {replayed} real TransferContract txs");
    assert!(replayed > 10, "expected a meaningful corpus, got {replayed}");
}
