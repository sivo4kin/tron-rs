//! Genesis-state initialization.
//!
//! Populates a fresh [`WorldState`] from a genesis specification (java-tron's
//! `genesis.block` config): reserve-balance accounts and the initial witnesses
//! with their bootstrap vote counts. This is the starting point for from-scratch
//! chain replay.

use crate::{cf, props, StateError, WorldState};
use prost::Message;
use tron_proto::protocol;
use tron_storage::KvStore;
use tron_types::Address;

/// A genesis account with a reserve balance.
#[derive(Debug, Clone)]
pub struct GenesisAccount {
    pub address: Address,
    pub name: String,
    pub balance: i64,
}

/// A genesis (bootstrap) witness.
#[derive(Debug, Clone)]
pub struct GenesisWitness {
    pub address: Address,
    pub url: String,
    pub vote_count: i64,
}

/// The genesis specification.
#[derive(Debug, Clone, Default)]
pub struct GenesisConfig {
    pub timestamp: i64,
    pub accounts: Vec<GenesisAccount>,
    pub witnesses: Vec<GenesisWitness>,
}

impl GenesisConfig {
    /// Total issued balance across genesis accounts (excludes the negative-balance
    /// blackhole sentinel java-tron uses).
    pub fn total_supply(&self) -> i128 {
        self.accounts.iter().filter(|a| a.balance >= 0).map(|a| a.balance as i128).sum()
    }
}

/// Apply a genesis config to a fresh world state.
pub fn apply_genesis<S: KvStore>(
    state: &mut WorldState<S>,
    config: &GenesisConfig,
) -> Result<(), StateError> {
    state.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, config.timestamp)?;

    for ga in &config.accounts {
        let account = protocol::Account {
            address: ga.address.as_bytes().to_vec(),
            account_name: ga.name.as_bytes().to_vec(),
            balance: ga.balance,
            create_time: config.timestamp,
            ..Default::default()
        };
        state.put_account(&ga.address, &account)?;
    }

    for gw in &config.witnesses {
        let witness = protocol::Witness {
            address: gw.address.as_bytes().to_vec(),
            url: gw.url.clone(),
            vote_count: gw.vote_count,
            is_jobs: true,
            ..Default::default()
        };
        state
            .db
            .put(cf::WITNESS, gw.address.as_bytes(), &witness.encode_to_vec())?;
        // Ensure the witness has an account too (java-tron creates both).
        if !state.account_exists(&gw.address)? {
            let account = protocol::Account {
                address: gw.address.as_bytes().to_vec(),
                create_time: config.timestamp,
                ..Default::default()
            };
            state.put_account(&gw.address, &account)?;
        }
    }

    // Seed the elected active-witness set so the intake gate has a set from block 1
    // (the maintenance/election cycle refreshes it thereafter — TODO).
    let active: Vec<Address> = config.witnesses.iter().map(|w| w.address).collect();
    state.put_active_witnesses(&active)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn sample() -> GenesisConfig {
        GenesisConfig {
            timestamp: 1_700_000_000_000,
            accounts: vec![
                GenesisAccount { address: addr(1), name: "Zion".into(), balance: 10_000_000_000 },
                GenesisAccount { address: addr(2), name: "Sun".into(), balance: 5_000_000_000 },
                GenesisAccount {
                    address: addr(3),
                    name: "Blackhole".into(),
                    balance: i64::MIN,
                },
            ],
            witnesses: vec![
                GenesisWitness { address: addr(10), url: "http://GR1.com".into(), vote_count: 100_000_026 },
                GenesisWitness { address: addr(11), url: "http://GR2.com".into(), vote_count: 100_000_025 },
            ],
        }
    }

    #[test]
    fn applies_accounts_and_witnesses() {
        let mut ws = WorldState::new(MemoryStore::new());
        let cfg = sample();
        apply_genesis(&mut ws, &cfg).unwrap();

        let zion = ws.get_account(&addr(1)).unwrap().unwrap();
        assert_eq!(zion.balance, 10_000_000_000);
        assert_eq!(zion.account_name, b"Zion");
        assert_eq!(zion.create_time, 1_700_000_000_000);

        // witness stored with its vote count, and has an account
        let w = protocol::Witness::decode(
            ws.db.get(cf::WITNESS, addr(10).as_bytes()).unwrap().unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(w.vote_count, 100_000_026);
        assert_eq!(w.url, "http://GR1.com");
        assert!(w.is_jobs);
        assert!(ws.account_exists(&addr(10)).unwrap());

        assert_eq!(ws.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP).unwrap(), 1_700_000_000_000);

        // The active-witness set is seeded from the genesis witnesses (H05), so the
        // intake gate has a set from block 1.
        let active = ws.get_active_witnesses().unwrap();
        assert_eq!(active, vec![addr(10).as_bytes().to_vec(), addr(11).as_bytes().to_vec()]);
    }

    #[test]
    fn total_supply_excludes_blackhole() {
        assert_eq!(sample().total_supply(), 15_000_000_000);
    }

    #[test]
    fn genesis_then_maintenance_elects_bootstrap_witnesses() {
        // The elected set right after genesis is the bootstrap witnesses,
        // ordered by their genesis vote counts.
        let mut ws = WorldState::new(MemoryStore::new());
        let cfg = sample();
        apply_genesis(&mut ws, &cfg).unwrap();
        let w1 = protocol::Witness::decode(
            ws.db.get(cf::WITNESS, addr(10).as_bytes()).unwrap().unwrap().as_slice(),
        )
        .unwrap();
        let w2 = protocol::Witness::decode(
            ws.db.get(cf::WITNESS, addr(11).as_bytes()).unwrap().unwrap().as_slice(),
        )
        .unwrap();
        // addr(10) has more votes than addr(11)
        assert!(w1.vote_count > w2.vote_count);
    }
}
