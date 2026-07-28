//! `VoteWitnessContract` — cast super-representative votes.
//!
//! Semantics mirror java-tron's `VoteWitnessActuator` exactly:
//!
//! **validate** — owner address must be valid 21-byte `0x41…`; the votes list must
//! be non-empty and no larger than `MAX_VOTE_NUMBER` (30); each vote's `vote_address`
//! must be valid and name a witness that exists in the witness store; each
//! `vote_count > 0`; the owner account must exist; and the total votes
//! (`sum(vote_count)`) must not exceed the owner's Tron Power. java-tron compares
//! `sum * TRX_PRECISION` (votes are denominated in TRX) against the Tron Power in sun,
//! which is the same as requiring `sum <= tron_power_sun / TRX_PRECISION`.
//!
//! **execute** — replace the owner account's `votes` (Stake-2.0 model): clear the old
//! votes and record the new ones on the `protocol.Account.votes` field. Voting is free
//! (`calcFee = 0`). java-tron additionally withdraws mortgage rewards and mirrors the
//! votes into a dedicated `VotesStore`; both are out of scope here (the account-level
//! vote set is the on-chain source of truth this actuator maintains).
//!
//! **Tron Power** — computed from the account's `frozen_v2` total (Stake 2.0): the sum
//! of all `FreezeV2.amount` entries, in sun. Legacy `frozen` (Stake 1.0) Tron Power is
//! out of scope.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::{self, VoteWitnessContract};
use tron_state::{cf, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// Maximum number of votes a single `VoteWitnessContract` may carry
/// (java-tron `Parameter.ChainConstant.MAX_VOTE_NUMBER`).
pub const MAX_VOTE_NUMBER: usize = 30;

/// TRX precision — 1 TRX = 1_000_000 sun
/// (java-tron `Parameter.ChainConstant.TRX_PRECISION`). A vote count is denominated in
/// TRX; Tron Power is measured in sun, so votes are scaled by this to compare.
pub const TRX_PRECISION: i64 = 1_000_000;

pub struct VoteWitnessActuator<'a> {
    contract: &'a VoteWitnessContract,
}

impl<'a> VoteWitnessActuator<'a> {
    pub fn new(contract: &'a VoteWitnessContract) -> Self {
        Self { contract }
    }

    fn parse_address(bytes: &[u8], msg: &str) -> Result<Address, ActuatorError> {
        let arr: [u8; ADDRESS_LEN] = bytes
            .try_into()
            .map_err(|_| ActuatorError::Validate(msg.to_string()))?;
        Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(msg.to_string()))
    }

    /// java-tron `VoteWitnessActuator.validate`. Voting charges no fee, so on success
    /// this returns `0` (kept parallel to the other actuators' `validate` signature).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "Invalid address")?;

        if self.contract.votes.is_empty() {
            return Err(ActuatorError::Validate("VoteNumber must more than 0".into()));
        }
        if self.contract.votes.len() > MAX_VOTE_NUMBER {
            return Err(ActuatorError::Validate(format!(
                "VoteNumber more than maxVoteNumber {MAX_VOTE_NUMBER}"
            )));
        }

        let mut sum: i64 = 0;
        for vote in &self.contract.votes {
            let witness = Self::parse_address(&vote.vote_address, "Invalid vote address!")?;
            if vote.vote_count <= 0 {
                return Err(ActuatorError::Validate(
                    "vote count must be greater than 0".into(),
                ));
            }
            if !witness_exists(state, &witness)? {
                return Err(ActuatorError::Validate(format!(
                    "Witness[{}] not exists",
                    hex(&vote.vote_address)
                )));
            }
            sum = sum
                .checked_add(vote.vote_count)
                .ok_or_else(|| ActuatorError::Validate("long overflow".into()))?;
        }

        let owner_account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!("Account[{}] not exists", hex(&self.contract.owner_address)))
        })?;

        let tron_power = tron_power_sun(&owner_account);
        let scaled = sum
            .checked_mul(TRX_PRECISION)
            .ok_or_else(|| ActuatorError::Validate("long overflow".into()))?;
        if scaled > tron_power {
            return Err(ActuatorError::Validate(format!(
                "The total number of votes[{scaled}] is greater than the tronPower[{tron_power}]"
            )));
        }

        Ok(0)
    }

    /// java-tron `VoteWitnessActuator.execute` (the `countVoteAccount` core). Call after
    /// a successful `validate`. Replaces the owner account's vote set.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = Self::parse_address(&self.contract.owner_address, "Invalid address")?;

        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;

        // clearVotes + addVotes: the new list wholly replaces the old one.
        owner_account.votes.clear();
        for vote in &self.contract.votes {
            owner_account.votes.push(protocol::Vote {
                vote_address: vote.vote_address.clone(),
                vote_count: vote.vote_count,
            });
        }
        state.put_account(&owner, &owner_account)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

/// Owner Tron Power in sun (Stake 2.0): the total of every `frozen_v2` entry's amount.
/// Legacy `frozen` (Stake 1.0) Tron Power is out of scope.
fn tron_power_sun(account: &protocol::Account) -> i64 {
    account
        .frozen_v2
        .iter()
        .map(|f| f.amount)
        .fold(0i64, |acc, a| acc.saturating_add(a))
}

/// Witness store lookup (java-tron `WitnessStore.has`). The witness store keys 21-byte
/// addresses to prost-encoded `protocol.Witness` values; only presence matters here.
fn witness_exists<S: KvStore>(
    state: &WorldState<S>,
    addr: &Address,
) -> Result<bool, ActuatorError> {
    state
        .db
        .exists(cf::WITNESS, addr.as_bytes())
        .map_err(|e| ActuatorError::State(e.to_string()))
}

/// Insert a witness into the witness store (test/seed helper). Key = 21-byte address,
/// value = prost-encoded `protocol.Witness`. Encoded by hand (the `tron-actuators`
/// crate does not depend on `prost` directly); the bytes are wire-identical to what
/// prost emits for the `address` (tag 1) and `vote_count` (tag 2) fields — the only
/// fields these actuator paths ever set — and the actuator only ever tests presence.
#[cfg_attr(not(test), allow(dead_code))]
fn put_witness<S: KvStore>(
    state: &mut WorldState<S>,
    witness: &protocol::Witness,
) -> Result<(), ActuatorError> {
    let mut buf = Vec::new();
    if !witness.address.is_empty() {
        buf.push(0x0A); // field 1, wire type 2 (length-delimited)
        encode_varint(witness.address.len() as u64, &mut buf);
        buf.extend_from_slice(&witness.address);
    }
    if witness.vote_count != 0 {
        buf.push(0x10); // field 2, wire type 0 (varint)
        encode_varint(witness.vote_count as u64, &mut buf);
    }
    state
        .db
        .put(cf::WITNESS, &witness.address, &buf)
        .map_err(|e| ActuatorError::State(e.to_string()))
}

fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol::{self, vote_witness_contract::Vote};
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// Seed the owner account with a Stake-2.0 frozen balance of `frozen` sun
    /// (its Tron Power). `existing_votes` pre-populate the account's vote set so that
    /// replacement (rather than accumulation) can be asserted.
    fn seed_owner(
        ws: &mut WorldState<MemoryStore>,
        owner: &Address,
        frozen: i64,
        existing_votes: &[(Address, i64)],
    ) {
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            frozen_v2: vec![protocol::account::FreezeV2 {
                r#type: protocol::ResourceCode::Bandwidth as i32,
                amount: frozen,
            }],
            votes: existing_votes
                .iter()
                .map(|(a, c)| protocol::Vote {
                    vote_address: a.as_bytes().to_vec(),
                    vote_count: *c,
                })
                .collect(),
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
    }

    fn seed_witness(ws: &mut WorldState<MemoryStore>, witness: &Address) {
        put_witness(
            ws,
            &protocol::Witness {
                address: witness.as_bytes().to_vec(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    fn vote(witness: &Address, count: i64) -> Vote {
        Vote {
            vote_address: witness.as_bytes().to_vec(),
            vote_count: count,
        }
    }

    fn contract(owner: &Address, votes: Vec<Vote>) -> VoteWitnessContract {
        VoteWitnessContract {
            owner_address: owner.as_bytes().to_vec(),
            votes,
            support: true,
        }
    }

    #[test]
    fn happy_path_records_votes_and_replaces_old() {
        let (o, w1, w2, stale) = (addr(1), addr(10), addr(11), addr(99));
        let mut ws = WorldState::new(MemoryStore::new());
        // Owner already voted for a stale witness; execution must wipe it.
        seed_owner(&mut ws, &o, 100_000_000, &[(stale, 7)]);
        seed_witness(&mut ws, &w1);
        seed_witness(&mut ws, &w2);

        let c = contract(&o, vec![vote(&w1, 10), vote(&w2, 20)]);
        let a = VoteWitnessActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        let res = a.execute(&mut ws).unwrap();
        assert_eq!(res.fee, 0);

        let recorded = ws.get_account(&o).unwrap().unwrap().votes;
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].vote_address, w1.as_bytes().to_vec());
        assert_eq!(recorded[0].vote_count, 10);
        assert_eq!(recorded[1].vote_address, w2.as_bytes().to_vec());
        assert_eq!(recorded[1].vote_count, 20);
        // stale vote is gone
        assert!(recorded.iter().all(|v| v.vote_address != stale.as_bytes().to_vec()));
    }

    #[test]
    fn rejects_vote_for_nonexistent_witness() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        // witness w is NOT seeded
        let c = contract(&o, vec![vote(&w, 5)]);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn rejects_zero_vote_count() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        seed_witness(&mut ws, &w);
        let c = contract(&o, vec![vote(&w, 0)]);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("vote count must be greater than 0")
        ));
    }

    #[test]
    fn rejects_negative_vote_count() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        seed_witness(&mut ws, &w);
        for count in [-1, i64::MIN] {
            let c = contract(&o, vec![vote(&w, count)]);
            assert!(
                matches!(
                    VoteWitnessActuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("vote count must be greater than 0")
                ),
                "vote_count {count} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_votes_list() {
        let o = addr(1);
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        let c = contract(&o, vec![]);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("VoteNumber must more than 0")
        ));
    }

    #[test]
    fn rejects_more_than_max_votes() {
        let o = addr(1);
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, i64::MAX, &[]);
        // 31 votes (> MAX_VOTE_NUMBER). The count check precedes witness lookups,
        // so the candidates need not be seeded.
        let votes: Vec<Vote> = (0..=MAX_VOTE_NUMBER as u8)
            .map(|i| vote(&addr(100 + i), 1))
            .collect();
        assert_eq!(votes.len(), MAX_VOTE_NUMBER + 1);
        let c = contract(&o, votes);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("VoteNumber more than maxVoteNumber")
        ));
    }

    #[test]
    fn rejects_votes_exceeding_tron_power() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        // 5 TRX of Tron Power (5_000_000 sun); 6 votes need 6 TRX.
        seed_owner(&mut ws, &o, 5_000_000, &[]);
        seed_witness(&mut ws, &w);
        let c = contract(&o, vec![vote(&w, 6)]);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("greater than the tronPower")
        ));
    }

    #[test]
    fn accepts_exactly_at_tron_power_boundary() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        // Exactly 5 TRX of Tron Power; 5 votes consume exactly 5 TRX.
        seed_owner(&mut ws, &o, 5_000_000, &[]);
        seed_witness(&mut ws, &w);
        let c = contract(&o, vec![vote(&w, 5)]);
        let a = VoteWitnessActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        assert_eq!(ws.get_account(&o).unwrap().unwrap().votes[0].vote_count, 5);
    }

    #[test]
    fn rejects_missing_owner() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        // Witness exists so validation reaches the owner-existence check.
        seed_witness(&mut ws, &w);
        let c = contract(&o, vec![vote(&w, 1)]);
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("not exists")
        ));
    }

    #[test]
    fn rejects_malformed_owner_address() {
        let (o, w) = (addr(1), addr(10));
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        seed_witness(&mut ws, &w);
        // owner address wrong length (20 bytes, missing 0x41 prefix byte)
        let c = VoteWitnessContract {
            owner_address: vec![0x41; 20],
            votes: vec![vote(&w, 1)],
            support: true,
        };
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    #[test]
    fn rejects_malformed_vote_address() {
        let o = addr(1);
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        // vote address wrong length
        let c = VoteWitnessContract {
            owner_address: o.as_bytes().to_vec(),
            votes: vec![Vote {
                vote_address: vec![0x41; 10],
                vote_count: 1,
            }],
            support: true,
        };
        assert!(matches!(
            VoteWitnessActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid vote address!")
        ));
    }

    #[test]
    fn revote_replaces_not_accumulates() {
        let (o, w1, w2) = (addr(1), addr(10), addr(11));
        let mut ws = WorldState::new(MemoryStore::new());
        seed_owner(&mut ws, &o, 100_000_000, &[]);
        seed_witness(&mut ws, &w1);
        seed_witness(&mut ws, &w2);

        // First vote: w1=10, w2=20.
        let c1 = contract(&o, vec![vote(&w1, 10), vote(&w2, 20)]);
        let a1 = VoteWitnessActuator::new(&c1);
        a1.validate(&ws).unwrap();
        a1.execute(&mut ws).unwrap();

        // Revote: w1=5 only. Must REPLACE, not accumulate.
        let c2 = contract(&o, vec![vote(&w1, 5)]);
        let a2 = VoteWitnessActuator::new(&c2);
        a2.validate(&ws).unwrap();
        a2.execute(&mut ws).unwrap();

        let recorded = ws.get_account(&o).unwrap().unwrap().votes;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].vote_address, w1.as_bytes().to_vec());
        assert_eq!(recorded[0].vote_count, 5);
    }
}
