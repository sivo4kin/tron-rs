//! Committee governance proposals — `ProposalCreateContract`,
//! `ProposalApproveContract`, and `ProposalDeleteContract`.
//!
//! Semantics mirror java-tron's `ProposalCreateActuator`,
//! `ProposalApproveActuator`, and `ProposalDeleteActuator`.
//!
//! Proposals are stored in the `PROPOSAL` column family, keyed by the 8-byte
//! big-endian proposal id (java-tron `ByteArray.fromLong`), value = prost-encoded
//! `protocol.Proposal`. The monotonic id counter lives in the dynamic property
//! [`LATEST_PROPOSAL_NUM`]. Witness membership is read from the `WITNESS` column
//! family (key = 21-byte address).
//!
//! **Create — validate** — owner address valid; owner account exists; owner is a
//! witness; the parameters map is non-empty; every parameter key is a supported
//! chain parameter (membership in [`ALLOWED_PARAM_IDS`]).
//!
//! **Create — execute** — assign `id = LATEST_PROPOSAL_NUM + 1`, store a
//! `Proposal` with `state = PENDING`, `create_time = now`,
//! `expiration_time = now + MAINTENANCE_TIME_INTERVAL_MS`, and bump the counter.
//!
//! **Approve — validate** — owner valid + exists + is a witness; the proposal
//! exists (`proposal_id <= LATEST_PROPOSAL_NUM` and present in the store); not
//! expired (`now < expiration_time`); not canceled. When `is_add_approval`, the
//! owner must not already be in `approvals` (no double-approve); when clearing,
//! the owner must currently be in `approvals`.
//!
//! **Approve — execute** — add or remove the owner address in `approvals`.
//!
//! **Delete — validate** — owner valid + exists; the proposal exists; the owner
//! equals `proposer_address`; not expired; not canceled.
//!
//! **Delete — execute** — set `state = CANCELED`.
//!
//! Deviations from java-tron (differences are data-only, documented here):
//! - Parameter validation checks only key membership in [`ALLOWED_PARAM_IDS`] (a
//!   representative subset of `ProposalUtil.ProposalType`); java-tron's
//!   `ProposalUtil.validator` additionally range-checks each value and applies
//!   fork/committee gates. Values are not range-checked here.
//! - Expiration uses a fixed `now + MAINTENANCE_TIME_INTERVAL_MS`; java-tron
//!   rounds up to the next maintenance boundary using `getNextMaintenanceTime`
//!   and `getProposalExpireTime`. Both yield a future PENDING window.
//! - The `supportUnfreezeDelay`-style committee gates and reward/vote
//!   bookkeeping are not modelled — these actuators touch only account/witness
//!   existence, the proposal store, and the id counter.

use crate::{ActuatorError, ExecutionResult};
use prost::Message;
use tron_proto::protocol::proposal::State;
use tron_proto::protocol::{
    Proposal, ProposalApproveContract, ProposalCreateContract, ProposalDeleteContract,
};
use tron_state::{cf, props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// java-tron `DynamicPropertiesStore` LATEST_PROPOSAL_NUM — monotonic id counter.
pub const LATEST_PROPOSAL_NUM: &str = "LATEST_PROPOSAL_NUM";

/// Maintenance interval in ms (java-tron default: 6 hours). Used as the PENDING
/// window for a newly created proposal.
pub const MAINTENANCE_TIME_INTERVAL_MS: i64 = 6 * 3600 * 1000;

/// Representative subset of java-tron `ProposalUtil.ProposalType` codes accepted
/// as chain parameters. See the module deviation note.
pub const ALLOWED_PARAM_IDS: &[i64] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26,
];

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("Invalid address".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("Invalid address".into()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn storage_err(e: tron_storage::StorageError) -> ActuatorError {
    ActuatorError::State(e.to_string())
}

/// 8-byte big-endian proposal-store key (java-tron `ByteArray.fromLong`).
fn proposal_key(id: i64) -> [u8; 8] {
    id.to_be_bytes()
}

fn get_proposal<S: KvStore>(
    state: &WorldState<S>,
    id: i64,
) -> Result<Option<Proposal>, ActuatorError> {
    match state.db.get(cf::PROPOSAL, &proposal_key(id)).map_err(storage_err)? {
        Some(bytes) => Ok(Some(
            Proposal::decode(bytes.as_slice()).map_err(|e| ActuatorError::State(e.to_string()))?,
        )),
        None => Ok(None),
    }
}

fn put_proposal<S: KvStore>(
    state: &mut WorldState<S>,
    proposal: &Proposal,
) -> Result<(), ActuatorError> {
    state
        .db
        .put(cf::PROPOSAL, &proposal_key(proposal.proposal_id), &proposal.encode_to_vec())
        .map_err(storage_err)
}

fn is_witness<S: KvStore>(state: &WorldState<S>, addr: &Address) -> Result<bool, ActuatorError> {
    state.db.exists(cf::WITNESS, addr.as_bytes()).map_err(storage_err)
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

pub struct ProposalCreateActuator<'a> {
    contract: &'a ProposalCreateContract,
}

impl<'a> ProposalCreateActuator<'a> {
    pub fn new(contract: &'a ProposalCreateContract) -> Self {
        Self { contract }
    }

    /// java-tron `ProposalCreateActuator.validate`. Returns the fee (always 0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] not exists",
                hex(owner.as_bytes())
            )));
        }
        if !is_witness(state, &owner)? {
            return Err(ActuatorError::Validate(format!(
                "Witness[{}] not exists",
                hex(owner.as_bytes())
            )));
        }

        if self.contract.parameters.is_empty() {
            return Err(ActuatorError::Validate("This proposal has no parameter.".into()));
        }

        for key in self.contract.parameters.keys() {
            if !ALLOWED_PARAM_IDS.contains(key) {
                return Err(ActuatorError::Validate(format!(
                    "Bad chain parameter id [{key}]"
                )));
            }
        }

        Ok(0)
    }

    /// java-tron `ProposalCreateActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let id = state.get_prop_i64(LATEST_PROPOSAL_NUM)? + 1;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        let proposal = Proposal {
            proposal_id: id,
            proposer_address: owner.as_bytes().to_vec(),
            parameters: self.contract.parameters.clone(),
            expiration_time: now
                .checked_add(MAINTENANCE_TIME_INTERVAL_MS)
                .ok_or_else(|| ActuatorError::Execute("long overflow".into()))?,
            create_time: now,
            approvals: Vec::new(),
            state: State::Pending as i32,
        };

        put_proposal(state, &proposal)?;
        state.put_prop_i64(LATEST_PROPOSAL_NUM, id)?;

        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// Approve
// ---------------------------------------------------------------------------

pub struct ProposalApproveActuator<'a> {
    contract: &'a ProposalApproveContract,
}

impl<'a> ProposalApproveActuator<'a> {
    pub fn new(contract: &'a ProposalApproveContract) -> Self {
        Self { contract }
    }

    /// java-tron `ProposalApproveActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] not exists",
                hex(owner.as_bytes())
            )));
        }
        if !is_witness(state, &owner)? {
            return Err(ActuatorError::Validate(format!(
                "Witness[{}] not exists",
                hex(owner.as_bytes())
            )));
        }

        let id = self.contract.proposal_id;
        let latest = state.get_prop_i64(LATEST_PROPOSAL_NUM)?;
        if id > latest {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] not exists")));
        }
        let proposal = get_proposal(state, id)?
            .ok_or_else(|| ActuatorError::Validate(format!("Proposal[{id}] not exists")))?;

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        if now >= proposal.expiration_time {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] expired")));
        }
        if proposal.state == State::Canceled as i32 {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] canceled")));
        }

        let already = proposal.approvals.iter().any(|a| a.as_slice() == owner.as_bytes());
        if self.contract.is_add_approval {
            if already {
                return Err(ActuatorError::Validate(format!(
                    "Witness[{}]has approved proposal[{id}] before",
                    hex(owner.as_bytes())
                )));
            }
        } else if !already {
            return Err(ActuatorError::Validate(format!(
                "Witness[{}]has not approved proposal[{id}] before",
                hex(owner.as_bytes())
            )));
        }

        Ok(0)
    }

    /// java-tron `ProposalApproveActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let id = self.contract.proposal_id;

        let mut proposal = get_proposal(state, id)?
            .ok_or_else(|| ActuatorError::Execute(format!("Proposal[{id}] not exists")))?;

        if self.contract.is_add_approval {
            proposal.approvals.push(owner.as_bytes().to_vec());
        } else {
            proposal.approvals.retain(|a| a.as_slice() != owner.as_bytes());
        }

        put_proposal(state, &proposal)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

pub struct ProposalDeleteActuator<'a> {
    contract: &'a ProposalDeleteContract,
}

impl<'a> ProposalDeleteActuator<'a> {
    pub fn new(contract: &'a ProposalDeleteContract) -> Self {
        Self { contract }
    }

    /// java-tron `ProposalDeleteActuator.validate`. Returns the fee (0).
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;

        if !state.account_exists(&owner)? {
            return Err(ActuatorError::Validate(format!(
                "Account[{}] not exists",
                hex(owner.as_bytes())
            )));
        }

        let id = self.contract.proposal_id;
        let latest = state.get_prop_i64(LATEST_PROPOSAL_NUM)?;
        if id > latest {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] not exists")));
        }
        let proposal = get_proposal(state, id)?
            .ok_or_else(|| ActuatorError::Validate(format!("Proposal[{id}] not exists")))?;

        if proposal.proposer_address.as_slice() != owner.as_bytes() {
            return Err(ActuatorError::Validate(format!(
                "Proposal[{id}] is not proposed by {}",
                hex(owner.as_bytes())
            )));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        if now >= proposal.expiration_time {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] expired")));
        }
        if proposal.state == State::Canceled as i32 {
            return Err(ActuatorError::Validate(format!("Proposal[{id}] canceled")));
        }

        Ok(0)
    }

    /// java-tron `ProposalDeleteActuator.execute`. Call after `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let id = self.contract.proposal_id;
        let mut proposal = get_proposal(state, id)?
            .ok_or_else(|| ActuatorError::Execute(format!("Proposal[{id}] not exists")))?;
        proposal.state = State::Canceled as i32;
        put_proposal(state, &proposal)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// A fresh state with `owner` as an existing account and (optionally) witness.
    fn seeded_state(owner: &Address, is_witness: bool) -> WorldState<MemoryStore> {
        let mut ws = WorldState::new(MemoryStore::new());
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            balance: 1_000_000_000,
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        if is_witness {
            ws.db.put(cf::WITNESS, owner.as_bytes(), &[1]).unwrap();
        }
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, 1_700_000_000_000).unwrap();
        ws
    }

    fn params(pairs: &[(i64, i64)]) -> HashMap<i64, i64> {
        pairs.iter().copied().collect()
    }

    fn create_contract(owner: &Address, pairs: &[(i64, i64)]) -> ProposalCreateContract {
        ProposalCreateContract {
            owner_address: owner.as_bytes().to_vec(),
            parameters: params(pairs),
        }
    }

    fn approve_contract(owner: &Address, id: i64, add: bool) -> ProposalApproveContract {
        ProposalApproveContract {
            owner_address: owner.as_bytes().to_vec(),
            proposal_id: id,
            is_add_approval: add,
        }
    }

    fn delete_contract(owner: &Address, id: i64) -> ProposalDeleteContract {
        ProposalDeleteContract {
            owner_address: owner.as_bytes().to_vec(),
            proposal_id: id,
        }
    }

    fn now_of(ws: &WorldState<MemoryStore>) -> i64 {
        ws.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP).unwrap()
    }

    // -- create ----------------------------------------------------------

    #[test]
    fn create_happy_path_assigns_id_stores_pending_sets_expiration() {
        let o = addr(1);
        let mut ws = seeded_state(&o, true);
        let now = now_of(&ws);
        let c = create_contract(&o, &[(0, 21_600_000), (3, 10)]);
        let a = ProposalCreateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        assert_eq!(ws.get_prop_i64(LATEST_PROPOSAL_NUM).unwrap(), 1);
        let p = get_proposal(&ws, 1).unwrap().unwrap();
        assert_eq!(p.proposal_id, 1);
        assert_eq!(p.proposer_address, o.as_bytes().to_vec());
        assert_eq!(p.state, State::Pending as i32);
        assert_eq!(p.create_time, now);
        assert_eq!(p.expiration_time, now + MAINTENANCE_TIME_INTERVAL_MS);
        assert!(p.approvals.is_empty());
        assert_eq!(p.parameters.get(&0), Some(&21_600_000));
        assert_eq!(p.parameters.get(&3), Some(&10));
    }

    #[test]
    fn create_second_proposal_increments_id() {
        let o = addr(1);
        let mut ws = seeded_state(&o, true);
        let c = create_contract(&o, &[(1, 9_999_000_000)]);
        ProposalCreateActuator::new(&c).execute(&mut ws).unwrap();
        let c2 = create_contract(&o, &[(2, 100_000)]);
        let a2 = ProposalCreateActuator::new(&c2);
        a2.validate(&ws).unwrap();
        a2.execute(&mut ws).unwrap();

        assert_eq!(ws.get_prop_i64(LATEST_PROPOSAL_NUM).unwrap(), 2);
        assert!(get_proposal(&ws, 1).unwrap().is_some());
        assert_eq!(get_proposal(&ws, 2).unwrap().unwrap().proposal_id, 2);
    }

    #[test]
    fn create_by_non_witness_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, false); // account exists, not a witness
        let c = create_contract(&o, &[(0, 21_600_000)]);
        assert!(matches!(
            ProposalCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Witness[") && m.contains("not exists")
        ));
    }

    #[test]
    fn create_empty_parameters_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, true);
        let c = create_contract(&o, &[]);
        assert!(matches!(
            ProposalCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("no parameter")
        ));
    }

    #[test]
    fn create_unsupported_param_id_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, true);
        let c = create_contract(&o, &[(999, 1)]);
        assert!(matches!(
            ProposalCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Bad chain parameter id")
        ));
    }

    #[test]
    fn create_missing_owner_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = create_contract(&addr(1), &[(0, 21_600_000)]);
        assert!(matches!(
            ProposalCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Account[") && m.contains("not exists")
        ));
    }

    #[test]
    fn create_malformed_address_rejected() {
        let ws = WorldState::new(MemoryStore::new());
        let c = ProposalCreateContract {
            owner_address: vec![0x41; 20],
            parameters: params(&[(0, 1)]),
        };
        assert!(matches!(
            ProposalCreateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    // -- approve ---------------------------------------------------------

    /// Create one PENDING proposal by witness `o`, returning its id (1).
    fn with_proposal(o: &Address) -> WorldState<MemoryStore> {
        let mut ws = seeded_state(o, true);
        let c = create_contract(o, &[(0, 21_600_000)]);
        ProposalCreateActuator::new(&c).execute(&mut ws).unwrap();
        ws
    }

    #[test]
    fn approve_happy_path_adds_approval() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        let c = approve_contract(&o, 1, true);
        let a = ProposalApproveActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let p = get_proposal(&ws, 1).unwrap().unwrap();
        assert_eq!(p.approvals.len(), 1);
        assert_eq!(p.approvals[0], o.as_bytes().to_vec());
    }

    #[test]
    fn approve_double_approve_rejected() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        let c = approve_contract(&o, 1, true);
        ProposalApproveActuator::new(&c).execute(&mut ws).unwrap();
        // Second add-approval must be rejected in validate.
        assert!(matches!(
            ProposalApproveActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("has approved proposal")
        ));
    }

    #[test]
    fn approve_unapprove_removes() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        let add = approve_contract(&o, 1, true);
        ProposalApproveActuator::new(&add).execute(&mut ws).unwrap();

        let remove = approve_contract(&o, 1, false);
        let a = ProposalApproveActuator::new(&remove);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        assert!(get_proposal(&ws, 1).unwrap().unwrap().approvals.is_empty());
    }

    #[test]
    fn approve_unapprove_when_absent_rejected() {
        let o = addr(1);
        let ws = with_proposal(&o);
        let remove = approve_contract(&o, 1, false);
        assert!(matches!(
            ProposalApproveActuator::new(&remove).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("has not approved proposal")
        ));
    }

    #[test]
    fn approve_nonexistent_proposal_rejected() {
        let o = addr(1);
        let ws = with_proposal(&o); // only proposal id 1 exists
        let c = approve_contract(&o, 2, true);
        assert!(matches!(
            ProposalApproveActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Proposal[2] not exists")
        ));
    }

    #[test]
    fn approve_expired_rejected() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        // Advance the clock past the proposal's expiration.
        let expire = get_proposal(&ws, 1).unwrap().unwrap().expiration_time;
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, expire + 1).unwrap();
        let c = approve_contract(&o, 1, true);
        assert!(matches!(
            ProposalApproveActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Proposal[1] expired")
        ));
    }

    #[test]
    fn approve_by_non_witness_rejected() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        // A second account that exists but is not a witness.
        let other = addr(2);
        let acct = protocol::Account {
            address: other.as_bytes().to_vec(),
            ..Default::default()
        };
        ws.put_account(&other, &acct).unwrap();
        let c = approve_contract(&other, 1, true);
        assert!(matches!(
            ProposalApproveActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Witness[") && m.contains("not exists")
        ));
    }

    // -- delete ----------------------------------------------------------

    #[test]
    fn delete_by_proposer_cancels() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        let c = delete_contract(&o, 1);
        let a = ProposalDeleteActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        assert_eq!(get_proposal(&ws, 1).unwrap().unwrap().state, State::Canceled as i32);
    }

    #[test]
    fn delete_by_other_rejected() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        // Another account (must exist to pass the account check).
        let other = addr(2);
        let acct = protocol::Account {
            address: other.as_bytes().to_vec(),
            ..Default::default()
        };
        ws.put_account(&other, &acct).unwrap();
        let c = delete_contract(&other, 1);
        assert!(matches!(
            ProposalDeleteActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("is not proposed by")
        ));
    }

    #[test]
    fn delete_non_pending_rejected() {
        let o = addr(1);
        let mut ws = with_proposal(&o);
        // First delete cancels it.
        let c = delete_contract(&o, 1);
        ProposalDeleteActuator::new(&c).execute(&mut ws).unwrap();
        // A second delete must be rejected as already canceled.
        assert!(matches!(
            ProposalDeleteActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Proposal[1] canceled")
        ));
    }

    #[test]
    fn delete_nonexistent_rejected() {
        let o = addr(1);
        let ws = seeded_state(&o, true); // no proposals created
        let c = delete_contract(&o, 1);
        assert!(matches!(
            ProposalDeleteActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Proposal[1] not exists")
        ));
    }
}
