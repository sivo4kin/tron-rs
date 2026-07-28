//! State-layer parity on REAL account data (SPEC section 7 item 3 groundwork).
//!
//! Fixtures are prost-encoded `protocol.Account` responses captured from a live
//! java-tron node (`capture_accounts`). Each must:
//! - decode into our generated type and re-encode **byte-identically** — if
//!   java-tron served fields our vendored proto lacks, prost would drop them and
//!   the bytes would differ (this guards proto currency), and
//! - round-trip unchanged through our `WorldState` account store.

use prost::Message;
use tron_proto::protocol;
use tron_state::WorldState;
use tron_storage::MemoryStore;
use tron_types::{Address, ADDRESS_LEN};

fn account_fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = format!("{}/{}/accounts", env!("CARGO_MANIFEST_DIR"), tron_verify::FIXTURE_DIR);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("accounts fixture dir") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "pb") {
            out.push((
                path.file_stem().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&path).unwrap(),
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn real_accounts_reencode_byte_identically() {
    let fixtures = account_fixtures();
    assert!(!fixtures.is_empty(), "run capture_accounts to create fixtures");
    for (name, bytes) in &fixtures {
        let account = protocol::Account::decode(bytes.as_slice()).expect("decode");
        let reencoded = account.encode_to_vec();
        assert_eq!(
            &reencoded, bytes,
            "{name}: re-encode differs — vendored proto is missing fields java-tron serves"
        );
    }
}

#[test]
fn real_accounts_roundtrip_through_world_state() {
    let mut ws = WorldState::new(MemoryStore::new());
    for (name, bytes) in account_fixtures() {
        let account = protocol::Account::decode(bytes.as_slice()).unwrap();
        let arr: [u8; ADDRESS_LEN] = account.address.as_slice().try_into().unwrap();
        let addr = Address::from_bytes(arr).unwrap();
        // Fixture name is the Base58Check of the address — cross-checks encoding.
        assert_eq!(addr.to_base58check(), name);

        ws.put_account(&addr, &account).unwrap();
        let loaded = ws.get_account(&addr).unwrap().unwrap();
        assert_eq!(loaded, account, "{name}: state store roundtrip changed the account");
        assert_eq!(
            loaded.encode_to_vec(),
            bytes,
            "{name}: stored bytes differ from java-tron's"
        );
    }
}

#[test]
fn real_accounts_expose_stake2_data() {
    // The captured accounts hold frozen_v2 entries — prove our typed access to
    // the Stake 2.0 fields the freeze/vote actuators rely on.
    let mut with_stake = 0;
    for (_, bytes) in account_fixtures() {
        let account = protocol::Account::decode(bytes.as_slice()).unwrap();
        if !account.frozen_v2.is_empty() {
            with_stake += 1;
            let total: i64 = account.frozen_v2.iter().map(|f| f.amount).sum();
            assert!(total >= 0);
        }
    }
    assert!(with_stake > 0, "expected at least one account with frozen_v2 data");
}
