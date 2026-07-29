//! DPoS maintenance cycle: count votes, update witness tallies, elect the active set.
//!
//! Mirrors java-tron `MaintenanceManager.doMaintenance` / `DposService.updateWitness`:
//! - accumulate each voter's votes per witness (java iterates the VotesStore; here
//!   we sum `Account.votes` across the provided voter accounts),
//! - add the tallies to each witness's running `vote_count`,
//! - sort all witnesses by vote count **descending**, tie-broken by hex-address
//!   string **descending** (the `allowWitnessSortOptimization` comparator), and
//!   take the top [`MAX_ACTIVE_WITNESSES`] as the active producing set.
//!
//! Deviation: java's per-voter old/new vote deltas + reward accumulation are not
//! modeled; we recompute tallies from current `Account.votes`. Reward/mortgage
//! bookkeeping is out of scope here.

use tron_consensus::MAX_ACTIVE_WITNESSES;
use prost::Message;
use std::collections::HashMap;
use tron_proto::protocol;
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

const CF_WITNESS: &str = tron_state::cf::WITNESS;

/// Sum `vote_count` per voted witness across a set of voter accounts.
pub fn count_votes(voters: &[protocol::Account]) -> HashMap<Vec<u8>, i64> {
    let mut tally: HashMap<Vec<u8>, i64> = HashMap::new();
    for voter in voters {
        for v in &voter.votes {
            *tally.entry(v.vote_address.clone()).or_insert(0) += v.vote_count;
        }
    }
    tally
}

/// The java-tron witness comparator (sort-optimization on): vote count descending,
/// then address bytes descending. Returns addresses in elected order. Delegates to
/// the single canonical comparator [`tron_consensus::election::cmp_witness`] so the
/// election path and the standalone [`tron_consensus::election::rank_witnesses`]
/// helper can never disagree on the ordering.
pub fn sort_witnesses(mut witnesses: Vec<(Vec<u8>, i64)>) -> Vec<Vec<u8>> {
    witnesses.sort_by(|a, b| {
        tron_consensus::election::cmp_witness((&a.0, a.1), (&b.0, b.1))
    });
    witnesses.into_iter().map(|(addr, _)| addr).collect()
}

/// Elect the active producing set: sorted witnesses truncated to 27.
pub fn active_set(witnesses: Vec<(Vec<u8>, i64)>) -> Vec<Vec<u8>> {
    let mut sorted = sort_witnesses(witnesses);
    sorted.truncate(MAX_ACTIVE_WITNESSES);
    sorted
}

/// Read all witnesses from the witness store.
pub fn all_witnesses<S: KvStore>(state: &WorldState<S>, addrs: &[Address]) -> Vec<protocol::Witness> {
    addrs
        .iter()
        .filter_map(|a| {
            state
                .db
                .get(CF_WITNESS, a.as_bytes())
                .ok()
                .flatten()
                .and_then(|b| protocol::Witness::decode(b.as_slice()).ok())
        })
        .collect()
}

/// Run a maintenance cycle: apply the voters' tallies to the witnesses, persist the
/// updated vote counts, and return the elected active set (up to 27 addresses).
///
/// `witness_addrs` enumerates the candidate witnesses (java iterates the whole
/// witness store); `voter_addrs` the accounts whose current `Account.votes` count.
pub fn run_maintenance<S: KvStore>(
    state: &mut WorldState<S>,
    witness_addrs: &[Address],
    voter_addrs: &[Address],
) -> Vec<Vec<u8>> {
    // 1. Tally votes from voter accounts.
    let voters: Vec<protocol::Account> = voter_addrs
        .iter()
        .filter_map(|a| state.get_account(a).ok().flatten())
        .collect();
    let tally = count_votes(&voters);

    // 2. Apply tallies to each witness's running count; collect (addr, count).
    let mut scored: Vec<(Vec<u8>, i64)> = Vec::new();
    for addr in witness_addrs {
        if let Some(bytes) = state.db.get(CF_WITNESS, addr.as_bytes()).ok().flatten() {
            if let Ok(mut w) = protocol::Witness::decode(bytes.as_slice()) {
                let added = tally.get(addr.as_bytes().as_slice()).copied().unwrap_or(0);
                w.vote_count += added;
                let _ = state
                    .db
                    .put(CF_WITNESS, addr.as_bytes(), &w.encode_to_vec());
                scored.push((addr.as_bytes().to_vec(), w.vote_count));
            }
        }
    }

    // 3. Elect the active set and persist it, so the live sync intake gate
    //    (`apply_synced_blocks_gated` reading `get_active_witnesses`) tracks the
    //    current election rather than the genesis set. Mirrors java-tron writing
    //    the recomputed active list at each maintenance boundary.
    let elected = active_set(scored);
    let addrs: Vec<Address> = elected
        .iter()
        .filter_map(|b| {
            <[u8; ADDRESS_LEN]>::try_from(b.as_slice())
                .ok()
                .and_then(|a| Address::from_bytes(a).ok())
        })
        .collect();
    let _ = state.put_active_witnesses(&addrs);
    elected
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn voter_with(votes: &[(&Address, i64)]) -> protocol::Account {
        protocol::Account {
            votes: votes
                .iter()
                .map(|(a, c)| protocol::Vote {
                    vote_address: a.as_bytes().to_vec(),
                    vote_count: *c,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn put_witness(ws: &mut WorldState<MemoryStore>, a: &Address, votes: i64) {
        let w = protocol::Witness {
            address: a.as_bytes().to_vec(),
            vote_count: votes,
            ..Default::default()
        };
        ws.db.put(CF_WITNESS, a.as_bytes(), &w.encode_to_vec()).unwrap();
    }

    #[test]
    fn count_votes_accumulates_across_voters() {
        let (w1, w2) = (addr(10), addr(11));
        let voters = vec![
            voter_with(&[(&w1, 5), (&w2, 3)]),
            voter_with(&[(&w1, 2)]),
        ];
        let tally = count_votes(&voters);
        assert_eq!(tally[w1.as_bytes().as_slice()], 7);
        assert_eq!(tally[w2.as_bytes().as_slice()], 3);
    }

    #[test]
    fn sort_by_votes_desc_then_hex_addr_desc() {
        // equal votes -> higher hex address first
        let lo = addr(0x01);
        let hi = addr(0xff);
        let big = addr(0x02);
        let ordered = sort_witnesses(vec![
            (lo.as_bytes().to_vec(), 100),
            (hi.as_bytes().to_vec(), 100),
            (big.as_bytes().to_vec(), 200),
        ]);
        // big (200) first; then tie between lo/hi broken by hex desc -> hi(ff) before lo(01)
        assert_eq!(ordered[0], big.as_bytes().to_vec());
        assert_eq!(ordered[1], hi.as_bytes().to_vec());
        assert_eq!(ordered[2], lo.as_bytes().to_vec());
    }

    #[test]
    fn active_set_truncates_to_27() {
        let witnesses: Vec<(Vec<u8>, i64)> = (0..40u8)
            .map(|i| (Address::from_body([i; 20]).as_bytes().to_vec(), i as i64))
            .collect();
        let elected = active_set(witnesses);
        assert_eq!(elected.len(), MAX_ACTIVE_WITNESSES);
        // highest vote count (39) must be first
        assert_eq!(elected[0], Address::from_body([39; 20]).as_bytes().to_vec());
    }

    #[test]
    fn run_maintenance_applies_votes_and_elects() {
        let mut ws = WorldState::new(MemoryStore::new());
        let (w1, w2, w3) = (addr(10), addr(11), addr(12));
        put_witness(&mut ws, &w1, 0);
        put_witness(&mut ws, &w2, 100);
        put_witness(&mut ws, &w3, 0);

        // one voter throws 500 votes at w3, 50 at w1
        let voter = addr(1);
        ws.put_account(&voter, &voter_with(&[(&w3, 500), (&w1, 50)])).unwrap();

        let active = run_maintenance(&mut ws, &[w1, w2, w3], &[voter]);
        // final counts: w1=50, w2=100, w3=500 -> order w3, w2, w1
        assert_eq!(active[0], w3.as_bytes().to_vec());
        assert_eq!(active[1], w2.as_bytes().to_vec());
        assert_eq!(active[2], w1.as_bytes().to_vec());

        // persisted vote counts updated
        let w3_stored = protocol::Witness::decode(
            ws.db.get(CF_WITNESS, w3.as_bytes()).unwrap().unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(w3_stored.vote_count, 500);
    }

    #[test]
    fn run_maintenance_persists_active_set_for_the_intake_gate() {
        let mut ws = WorldState::new(MemoryStore::new());
        let (w1, w2, w3) = (addr(10), addr(11), addr(12));
        put_witness(&mut ws, &w1, 0);
        put_witness(&mut ws, &w2, 100);
        put_witness(&mut ws, &w3, 0);
        let voter = addr(1);
        ws.put_account(&voter, &voter_with(&[(&w3, 500), (&w1, 50)])).unwrap();

        let active = run_maintenance(&mut ws, &[w1, w2, w3], &[voter]);
        // The elected set is now stored exactly where the sync intake gate reads it
        // (get_active_witnesses) — closing the H05 genesis-only gap.
        let stored = ws.get_active_witnesses().unwrap();
        assert_eq!(stored, active, "maintenance must persist the elected set");
        assert_eq!(stored[0], w3.as_bytes().to_vec()); // top by votes
        assert_eq!(stored.len(), 3);
    }

    #[test]
    fn no_voters_leaves_counts_unchanged() {
        let mut ws = WorldState::new(MemoryStore::new());
        let w1 = addr(10);
        put_witness(&mut ws, &w1, 42);
        let active = run_maintenance(&mut ws, &[w1], &[]);
        assert_eq!(active, vec![w1.as_bytes().to_vec()]);
        let stored = protocol::Witness::decode(
            ws.db.get(CF_WITNESS, w1.as_bytes()).unwrap().unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(stored.vote_count, 42);
    }
}
