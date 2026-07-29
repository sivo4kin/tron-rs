//! Stake-2.0 resource delegation — `DelegateResourceContract` and
//! `UnDelegateResourceContract`.
//!
//! Semantics mirror java-tron's `DelegateResourceActuator` and
//! `UnDelegateResourceActuator` (the frozen_v2 / Stake-2.0 model).
//!
//! **Delegate — validate** — owner address valid 21-byte `0x41…`; owner account
//! exists; `balance >= 1 TRX`; resource is `BANDWIDTH` or `ENERGY`; the owner's
//! delegatable `frozen_v2` balance of that resource is `>= balance`; receiver address
//! valid and `!= owner`; receiver account exists and is not a contract; when `lock`,
//! the lock period is non-negative and (if a prior lock exists) not shorter than its
//! remaining time.
//!
//! **Delegate — execute** — move `balance` out of the owner's `frozen_v2` entry for the
//! resource into the owner's `delegated_frozen_v2_balance_for_*`, credit the receiver's
//! `acquired_delegated_frozen_v2_balance_for_*`, and record a
//! [`DelegatedResource`](protocol::DelegatedResource) keyed `owner‖receiver‖lock` with
//! the resource balance and (when locked) an `expire_time = now + lockPeriod × blockInterval`.
//!
//! **Undelegate — validate** — owner valid + exists; receiver valid + `!= owner`; a
//! delegation record (locked or unlocked) exists; `balance > 0`; and the *available*
//! delegated balance — the unlocked record plus any locked record already past its
//! expiry — is `>= balance`.
//!
//! **Undelegate — execute** — first fold any expired locked record into the unlocked
//! record; then debit the receiver's acquired balance (flooring at 0, mirroring the
//! java-tron contract-suicide guard), debit the unlocked record and the owner's
//! delegated balance, and return `balance` to the owner's `frozen_v2`. An emptied record
//! is deleted.
//!
//! ## Deviations from java-tron (data-only, documented here)
//! - **Committee gates.** `supportDR` / `supportUnfreezeDelay` / `supportMaxDelegateLockPeriod`
//!   are not modelled; delegation is treated as always enabled (same posture as
//!   `freeze_v2.rs`).
//! - **Usage accounting.** java-tron reduces the delegatable balance by the owner's
//!   *current resource usage* (`v2NetUsage` / `v2EnergyUsage`, via Bandwidth/Energy
//!   processors and the global weight/limit ratios) and, on undelegate, transfers a
//!   pro-rata slice of the receiver's usage back to the owner. None of that usage
//!   bookkeeping is modelled: the delegatable balance is taken to be the full
//!   `frozen_v2` amount of the resource, and no `net_usage` / `energy_usage` is moved.
//! - **Delegation store.** java-tron's `DelegatedResourceStore` +
//!   `DelegatedResourceAccountIndexStore` are collapsed into a single module-local
//!   column family [`DELEGATION_CF`], keyed `owner‖receiver‖lockByte`, value =
//!   prost-encoded `protocol.DelegatedResource`. The account index is not maintained.
//! - **Lock re-lock check.** The `validRemainTime` re-lock rule is modelled for the
//!   resource being delegated; the per-resource split within a single record follows
//!   the proto `DelegatedResource` fields.

use crate::{ActuatorError, ExecutionResult};
use prost::Message;
use tron_proto::protocol::account::FreezeV2;
use tron_proto::protocol::{
    Account, AccountType, DelegateResourceContract, DelegatedResource, ResourceCode,
    UnDelegateResourceContract,
};
use tron_state::{props, WorldState};
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

/// java-tron `Parameter.ChainConstant.TRX_PRECISION` — sun per TRX.
pub const TRX_PRECISION: i64 = 1_000_000;
/// java-tron `Parameter.ChainConstant.BLOCK_PRODUCED_INTERVAL` — ms per block.
pub const BLOCK_PRODUCED_INTERVAL_MS: i64 = 3_000;
/// java-tron `Parameter.ChainConstant.DELEGATE_PERIOD` — default lock window (ms), 3 days.
pub const DELEGATE_PERIOD_MS: i64 = 3 * 86_400_000;

/// Module-local column family holding delegation records (see module deviations).
pub const DELEGATION_CF: &str = tron_state::cf::DELEGATION;

const BANDWIDTH: i32 = ResourceCode::Bandwidth as i32;
const ENERGY: i32 = ResourceCode::Energy as i32;

fn parse_address(bytes: &[u8], msg: &str) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate(msg.to_string()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate(msg.to_string()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Default lock period, in blocks (java-tron `DELEGATE_PERIOD / BLOCK_PRODUCED_INTERVAL`).
fn default_lock_period_blocks() -> i64 {
    DELEGATE_PERIOD_MS / BLOCK_PRODUCED_INTERVAL_MS
}

// -- owner frozen_v2 (delegatable) balance --------------------------------

/// Total `frozen_v2` amount of `resource` held by `account` (delegatable balance).
fn frozen_v2_amount(account: &Account, resource: i32) -> i64 {
    account
        .frozen_v2
        .iter()
        .filter(|f| f.r#type == resource)
        .map(|f| f.amount)
        .fold(0i64, |acc, a| acc.saturating_add(a))
}

/// Add `delta` (may be negative) to the account's `frozen_v2` entry for `resource`,
/// creating the entry when absent.
fn add_frozen_v2(account: &mut Account, resource: i32, delta: i64) {
    if let Some(f) = account.frozen_v2.iter_mut().find(|f| f.r#type == resource) {
        f.amount += delta;
    } else {
        account.frozen_v2.push(FreezeV2 {
            r#type: resource,
            amount: delta,
        });
    }
}

// -- owner delegated-out / receiver acquired-in balances ------------------

#[cfg_attr(not(test), allow(dead_code))]
fn owner_delegated(account: &Account, resource: i32) -> i64 {
    if resource == BANDWIDTH {
        account.delegated_frozen_v2_balance_for_bandwidth
    } else {
        account
            .account_resource
            .as_ref()
            .map(|r| r.delegated_frozen_v2_balance_for_energy)
            .unwrap_or(0)
    }
}

fn add_owner_delegated(account: &mut Account, resource: i32, delta: i64) {
    if resource == BANDWIDTH {
        account.delegated_frozen_v2_balance_for_bandwidth += delta;
    } else {
        account
            .account_resource
            .get_or_insert_with(Default::default)
            .delegated_frozen_v2_balance_for_energy += delta;
    }
}

fn receiver_acquired(account: &Account, resource: i32) -> i64 {
    if resource == BANDWIDTH {
        account.acquired_delegated_frozen_v2_balance_for_bandwidth
    } else {
        account
            .account_resource
            .as_ref()
            .map(|r| r.acquired_delegated_frozen_v2_balance_for_energy)
            .unwrap_or(0)
    }
}

/// Add `delta` to the receiver's acquired balance, flooring the result at 0 when the
/// existing amount is smaller than a negative `delta` (java-tron's contract-suicide guard).
fn add_receiver_acquired(account: &mut Account, resource: i32, delta: i64) {
    let cur = receiver_acquired(account, resource);
    let next = if delta < 0 && cur < -delta { 0 } else { cur + delta };
    if resource == BANDWIDTH {
        account.acquired_delegated_frozen_v2_balance_for_bandwidth = next;
    } else {
        account
            .account_resource
            .get_or_insert_with(Default::default)
            .acquired_delegated_frozen_v2_balance_for_energy = next;
    }
}

// -- delegation records ---------------------------------------------------

fn delegation_key(owner: &Address, receiver: &Address, lock: bool) -> Vec<u8> {
    let mut key = Vec::with_capacity(ADDRESS_LEN * 2 + 1);
    key.extend_from_slice(owner.as_bytes());
    key.extend_from_slice(receiver.as_bytes());
    key.push(lock as u8);
    key
}

fn get_delegation<S: KvStore>(
    state: &WorldState<S>,
    owner: &Address,
    receiver: &Address,
    lock: bool,
) -> Result<Option<DelegatedResource>, ActuatorError> {
    match state
        .db
        .get(DELEGATION_CF, &delegation_key(owner, receiver, lock))
        .map_err(|e| ActuatorError::State(e.to_string()))?
    {
        Some(bytes) => DelegatedResource::decode(bytes.as_slice())
            .map(Some)
            .map_err(|e| ActuatorError::State(e.to_string())),
        None => Ok(None),
    }
}

fn put_delegation<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &Address,
    receiver: &Address,
    lock: bool,
    rec: &DelegatedResource,
) -> Result<(), ActuatorError> {
    state
        .db
        .put(
            DELEGATION_CF,
            &delegation_key(owner, receiver, lock),
            &rec.encode_to_vec(),
        )
        .map_err(|e| ActuatorError::State(e.to_string()))
}

fn delete_delegation<S: KvStore>(
    state: &mut WorldState<S>,
    owner: &Address,
    receiver: &Address,
    lock: bool,
) -> Result<(), ActuatorError> {
    state
        .db
        .delete(DELEGATION_CF, &delegation_key(owner, receiver, lock))
        .map_err(|e| ActuatorError::State(e.to_string()))
}

fn rec_frozen(rec: &DelegatedResource, resource: i32) -> i64 {
    if resource == BANDWIDTH {
        rec.frozen_balance_for_bandwidth
    } else {
        rec.frozen_balance_for_energy
    }
}

fn rec_set_frozen(rec: &mut DelegatedResource, resource: i32, value: i64) {
    if resource == BANDWIDTH {
        rec.frozen_balance_for_bandwidth = value;
    } else {
        rec.frozen_balance_for_energy = value;
    }
}

fn rec_expire(rec: &DelegatedResource, resource: i32) -> i64 {
    if resource == BANDWIDTH {
        rec.expire_time_for_bandwidth
    } else {
        rec.expire_time_for_energy
    }
}

fn rec_set_expire(rec: &mut DelegatedResource, resource: i32, value: i64) {
    if resource == BANDWIDTH {
        rec.expire_time_for_bandwidth = value;
    } else {
        rec.expire_time_for_energy = value;
    }
}

fn rec_is_empty(rec: &DelegatedResource) -> bool {
    rec.frozen_balance_for_bandwidth == 0 && rec.frozen_balance_for_energy == 0
}

fn resource_valid(resource: i32) -> bool {
    resource == BANDWIDTH || resource == ENERGY
}

// -- DelegateResource -----------------------------------------------------

pub struct DelegateResourceActuator<'a> {
    contract: &'a DelegateResourceContract,
}

impl<'a> DelegateResourceActuator<'a> {
    pub fn new(contract: &'a DelegateResourceContract) -> Self {
        Self { contract }
    }

    /// java-tron `DelegateResourceActuator.validate`. Delegation is free; returns `0`.
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "Invalid address")?;

        let owner_account = state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                hex(&self.contract.owner_address)
            ))
        })?;

        let balance = self.contract.balance;
        if balance < TRX_PRECISION {
            return Err(ActuatorError::Validate(
                "delegateBalance must be greater than or equal to 1 TRX".into(),
            ));
        }

        let resource = self.contract.resource;
        if !resource_valid(resource) {
            return Err(ActuatorError::Validate(
                "ResourceCode error, valid ResourceCode[BANDWIDTH, ENERGY]".into(),
            ));
        }
        // Deviation: available balance is the full frozen_v2 amount (no usage subtracted).
        if frozen_v2_amount(&owner_account, resource) < balance {
            let msg = if resource == BANDWIDTH {
                "delegateBalance must be less than or equal to available FreezeBandwidthV2 balance"
            } else {
                "delegateBalance must be less than or equal to available FreezeEnergyV2 balance"
            };
            return Err(ActuatorError::Validate(msg.into()));
        }

        let receiver = parse_address(&self.contract.receiver_address, "Invalid receiverAddress")?;
        if receiver == owner {
            return Err(ActuatorError::Validate(
                "receiverAddress must not be the same as ownerAddress".into(),
            ));
        }
        let receiver_account = state.get_account(&receiver)?.ok_or_else(|| {
            ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                hex(&self.contract.receiver_address)
            ))
        })?;

        if self.contract.lock {
            let lock_period = self.lock_period_blocks();
            if lock_period < 0 {
                return Err(ActuatorError::Validate(
                    "The lock period of delegate resource cannot be less than 0".into(),
                ));
            }
            // validRemainTime: a new lock cannot be shorter than the remaining time of a
            // prior lock for the same resource.
            if let Some(existing) = get_delegation(state, &owner, &receiver, true)? {
                let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
                let remain = rec_expire(&existing, resource) - now;
                if lock_period * BLOCK_PRODUCED_INTERVAL_MS < remain {
                    return Err(ActuatorError::Validate(
                        "The lock period this time cannot be less than the remaining time of the \
                         last lock period"
                            .into(),
                    ));
                }
            }
        }

        if receiver_account.r#type == AccountType::Contract as i32 {
            return Err(ActuatorError::Validate(
                "Do not allow delegate resources to contract addresses".into(),
            ));
        }

        Ok(0)
    }

    /// java-tron `DelegateResourceActuator.execute`. Call after a successful `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "Invalid address")?;
        let receiver = parse_address(&self.contract.receiver_address, "Invalid receiverAddress")?;
        let resource = self.contract.resource;
        let balance = self.contract.balance;
        let lock = self.contract.lock;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        // owner: frozen_v2 -> delegated
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        add_frozen_v2(&mut owner_account, resource, -balance);
        add_owner_delegated(&mut owner_account, resource, balance);
        state.put_account(&owner, &owner_account)?;

        // receiver: acquired += balance
        let mut receiver_account = state
            .get_account(&receiver)?
            .ok_or_else(|| ActuatorError::Execute("receiver account missing".into()))?;
        add_receiver_acquired(&mut receiver_account, resource, balance);
        state.put_account(&receiver, &receiver_account)?;

        // delegation record (owner‖receiver‖lock)
        let expire = if lock {
            now + self.lock_period_blocks() * BLOCK_PRODUCED_INTERVAL_MS
        } else {
            0
        };
        let mut rec = get_delegation(state, &owner, &receiver, lock)?.unwrap_or(DelegatedResource {
            from: owner.as_bytes().to_vec(),
            to: receiver.as_bytes().to_vec(),
            ..Default::default()
        });
        let new_frozen = rec_frozen(&rec, resource) + balance;
        rec_set_frozen(&mut rec, resource, new_frozen);
        rec_set_expire(&mut rec, resource, expire);
        put_delegation(state, &owner, &receiver, lock, &rec)?;

        Ok(ExecutionResult { fee: 0 })
    }

    fn lock_period_blocks(&self) -> i64 {
        if self.contract.lock_period == 0 {
            default_lock_period_blocks()
        } else {
            self.contract.lock_period
        }
    }
}

// -- UnDelegateResource ---------------------------------------------------

pub struct UnDelegateResourceActuator<'a> {
    contract: &'a UnDelegateResourceContract,
}

impl<'a> UnDelegateResourceActuator<'a> {
    pub fn new(contract: &'a UnDelegateResourceContract) -> Self {
        Self { contract }
    }

    /// java-tron `UnDelegateResourceActuator.validate`. Free; returns `0` on success.
    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "Invalid address")?;
        state.get_account(&owner)?.ok_or_else(|| {
            ActuatorError::Validate(format!(
                "Account[{}] does not exist",
                hex(&self.contract.owner_address)
            ))
        })?;

        let receiver = parse_address(&self.contract.receiver_address, "Invalid receiverAddress")?;
        if receiver == owner {
            return Err(ActuatorError::Validate(
                "receiverAddress must not be the same as ownerAddress".into(),
            ));
        }

        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;
        let unlock = get_delegation(state, &owner, &receiver, false)?;
        let lock = get_delegation(state, &owner, &receiver, true)?;
        if unlock.is_none() && lock.is_none() {
            return Err(ActuatorError::Validate("delegated Resource does not exist".into()));
        }

        let balance = self.contract.balance;
        if balance <= 0 {
            return Err(ActuatorError::Validate(
                "unDelegateBalance must be more than 0 TRX".into(),
            ));
        }

        let resource = self.contract.resource;
        if !resource_valid(resource) {
            return Err(ActuatorError::Validate(
                "ResourceCode error, valid ResourceCode[BANDWIDTH, ENERGY]".into(),
            ));
        }

        // Available = unlocked balance + any locked balance already past expiry.
        let mut available = 0i64;
        if let Some(u) = &unlock {
            available += rec_frozen(u, resource);
        }
        if let Some(l) = &lock {
            if rec_expire(l, resource) < now {
                available += rec_frozen(l, resource);
            }
        }
        if available < balance {
            let name = if resource == BANDWIDTH { "BANDWIDTH" } else { "Energy" };
            return Err(ActuatorError::Validate(format!(
                "insufficient delegatedFrozenBalance({name}), request={balance}, unlock_balance={available}"
            )));
        }

        Ok(0)
    }

    /// java-tron `UnDelegateResourceActuator.execute`. Call after a successful `validate`.
    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address, "Invalid address")?;
        let receiver = parse_address(&self.contract.receiver_address, "Invalid receiverAddress")?;
        let resource = self.contract.resource;
        let balance = self.contract.balance;
        let now = state.get_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP)?;

        // Fold an expired locked record into the unlocked record (unLockExpireResource).
        self.merge_expired_lock(state, &owner, &receiver, resource, now)?;

        // receiver: acquired -= balance (floored at 0)
        if let Some(mut receiver_account) = state.get_account(&receiver)? {
            add_receiver_acquired(&mut receiver_account, resource, -balance);
            state.put_account(&receiver, &receiver_account)?;
        }

        // unlocked record: frozen -= balance
        let mut unlock = get_delegation(state, &owner, &receiver, false)?
            .ok_or_else(|| ActuatorError::Execute("unlocked delegation missing".into()))?;
        let remaining = rec_frozen(&unlock, resource) - balance;
        rec_set_frozen(&mut unlock, resource, remaining);
        if rec_is_empty(&unlock) {
            delete_delegation(state, &owner, &receiver, false)?;
        } else {
            put_delegation(state, &owner, &receiver, false, &unlock)?;
        }

        // owner: delegated -= balance, frozen_v2 += balance
        let mut owner_account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        add_owner_delegated(&mut owner_account, resource, -balance);
        add_frozen_v2(&mut owner_account, resource, balance);
        state.put_account(&owner, &owner_account)?;

        Ok(ExecutionResult { fee: 0 })
    }

    /// Move an expired locked record's `resource` balance into the unlocked record,
    /// mirroring java-tron `DelegatedResourceStore.unLockExpireResource`.
    fn merge_expired_lock<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
        owner: &Address,
        receiver: &Address,
        resource: i32,
        now: i64,
    ) -> Result<(), ActuatorError> {
        let Some(mut lock) = get_delegation(state, owner, receiver, true)? else {
            return Ok(());
        };
        let expire = rec_expire(&lock, resource);
        let locked = rec_frozen(&lock, resource);
        if locked == 0 || expire == 0 || expire >= now {
            return Ok(()); // nothing matured for this resource
        }

        // move locked -> unlocked
        let mut unlock =
            get_delegation(state, owner, receiver, false)?.unwrap_or(DelegatedResource {
                from: owner.as_bytes().to_vec(),
                to: receiver.as_bytes().to_vec(),
                ..Default::default()
            });
        let merged = rec_frozen(&unlock, resource) + locked;
        rec_set_frozen(&mut unlock, resource, merged);
        put_delegation(state, owner, receiver, false, &unlock)?;

        rec_set_frozen(&mut lock, resource, 0);
        rec_set_expire(&mut lock, resource, 0);
        if rec_is_empty(&lock) {
            delete_delegation(state, owner, receiver, true)?;
        } else {
            put_delegation(state, owner, receiver, true, &lock)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    const NOW: i64 = 1_000_000;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    /// World state with `now` set and `owner` seeded with `frozen` sun of `resource`.
    fn state_with_owner(owner: &Address, resource: i32, frozen: i64) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        let account = protocol::Account {
            address: owner.as_bytes().to_vec(),
            frozen_v2: vec![FreezeV2 { r#type: resource, amount: frozen }],
            ..Default::default()
        };
        ws.put_account(owner, &account).unwrap();
        ws
    }

    fn seed_account(ws: &mut WorldState<MemoryStore>, a: &Address, kind: AccountType) {
        let account = protocol::Account {
            address: a.as_bytes().to_vec(),
            r#type: kind as i32,
            ..Default::default()
        };
        ws.put_account(a, &account).unwrap();
    }

    fn del_contract(
        owner: &Address,
        receiver: &Address,
        resource: i32,
        balance: i64,
        lock: bool,
        lock_period: i64,
    ) -> DelegateResourceContract {
        DelegateResourceContract {
            owner_address: owner.as_bytes().to_vec(),
            resource,
            balance,
            receiver_address: receiver.as_bytes().to_vec(),
            lock,
            lock_period,
        }
    }

    fn undel_contract(
        owner: &Address,
        receiver: &Address,
        resource: i32,
        balance: i64,
    ) -> UnDelegateResourceContract {
        UnDelegateResourceContract {
            owner_address: owner.as_bytes().to_vec(),
            resource,
            balance,
            receiver_address: receiver.as_bytes().to_vec(),
        }
    }

    // ---- Delegate ----

    #[test]
    fn delegate_happy_path_moves_frozen_to_delegated() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);

        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, false, 0);
        let a = DelegateResourceActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let owner = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(frozen_v2_amount(&owner, BANDWIDTH), 5_000_000);
        assert_eq!(owner_delegated(&owner, BANDWIDTH), 5_000_000);
        let recv = ws.get_account(&r).unwrap().unwrap();
        assert_eq!(receiver_acquired(&recv, BANDWIDTH), 5_000_000);
        let rec = get_delegation(&ws, &o, &r, false).unwrap().unwrap();
        assert_eq!(rec_frozen(&rec, BANDWIDTH), 5_000_000);
        assert_eq!(rec_expire(&rec, BANDWIDTH), 0);
    }

    #[test]
    fn delegate_energy_happy_path() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, ENERGY, 3_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        let c = del_contract(&o, &r, ENERGY, 3_000_000, false, 0);
        let a = DelegateResourceActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();
        let owner = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(frozen_v2_amount(&owner, ENERGY), 0);
        assert_eq!(owner_delegated(&owner, ENERGY), 3_000_000);
        let recv = ws.get_account(&r).unwrap().unwrap();
        assert_eq!(receiver_acquired(&recv, ENERGY), 3_000_000);
    }

    #[test]
    fn delegate_self_rejected() {
        let o = addr(1);
        let ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        let c = del_contract(&o, &o, BANDWIDTH, 5_000_000, false, 0);
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("must not be the same as ownerAddress")
        ));
    }

    #[test]
    fn delegate_insufficient_frozen_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 3_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, false, 0);
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("available FreezeBandwidthV2 balance")
        ));
    }

    #[test]
    fn delegate_below_one_trx_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        for bal in [0, -1, 999_999] {
            let c = del_contract(&o, &r, BANDWIDTH, bal, false, 0);
            assert!(
                matches!(
                    DelegateResourceActuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("greater than or equal to 1 TRX")
                ),
                "balance {bal} must be rejected"
            );
        }
    }

    #[test]
    fn delegate_missing_owner_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        seed_account(&mut ws, &r, AccountType::Normal);
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, false, 0);
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn delegate_missing_receiver_rejected() {
        let (o, r) = (addr(1), addr(2));
        let ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        // receiver r not seeded
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, false, 0);
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn delegate_to_contract_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        seed_account(&mut ws, &r, AccountType::Contract);
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, false, 0);
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("contract addresses")
        ));
    }

    #[test]
    fn delegate_malformed_owner_address_rejected() {
        let r = addr(2);
        let mut ws = WorldState::new(MemoryStore::new());
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW).unwrap();
        seed_account(&mut ws, &r, AccountType::Normal);
        let c = DelegateResourceContract {
            owner_address: vec![0x41; 20], // wrong length
            resource: BANDWIDTH,
            balance: 5_000_000,
            receiver_address: r.as_bytes().to_vec(),
            lock: false,
            lock_period: 0,
        };
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid address")
        ));
    }

    #[test]
    fn delegate_malformed_receiver_address_rejected() {
        let o = addr(1);
        let ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        let mut bad = [0u8; ADDRESS_LEN];
        bad[0] = 0x42; // wrong prefix
        let c = DelegateResourceContract {
            owner_address: o.as_bytes().to_vec(),
            resource: BANDWIDTH,
            balance: 5_000_000,
            receiver_address: bad.to_vec(),
            lock: false,
            lock_period: 0,
        };
        assert!(matches!(
            DelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid receiverAddress")
        ));
    }

    // ---- Undelegate ----

    /// Seed a completed unlocked delegation of `balance` bandwidth from `o` to `r`.
    fn setup_delegated(o: &Address, r: &Address, balance: i64) -> WorldState<MemoryStore> {
        let mut ws = state_with_owner(o, BANDWIDTH, balance);
        seed_account(&mut ws, r, AccountType::Normal);
        let c = del_contract(o, r, BANDWIDTH, balance, false, 0);
        DelegateResourceActuator::new(&c).execute(&mut ws).unwrap();
        ws
    }

    #[test]
    fn undelegate_happy_path_restores_frozen() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = setup_delegated(&o, &r, 5_000_000);

        let c = undel_contract(&o, &r, BANDWIDTH, 5_000_000);
        let a = UnDelegateResourceActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let owner = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(frozen_v2_amount(&owner, BANDWIDTH), 5_000_000);
        assert_eq!(owner_delegated(&owner, BANDWIDTH), 0);
        let recv = ws.get_account(&r).unwrap().unwrap();
        assert_eq!(receiver_acquired(&recv, BANDWIDTH), 0);
        // emptied record deleted
        assert!(get_delegation(&ws, &o, &r, false).unwrap().is_none());
    }

    #[test]
    fn undelegate_nonexistent_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        // no delegation performed
        let c = undel_contract(&o, &r, BANDWIDTH, 5_000_000);
        assert!(matches!(
            UnDelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("delegated Resource does not exist")
        ));
    }

    #[test]
    fn undelegate_non_positive_rejected() {
        let (o, r) = (addr(1), addr(2));
        let ws = setup_delegated(&o, &r, 5_000_000);
        for bal in [0, -1] {
            let c = undel_contract(&o, &r, BANDWIDTH, bal);
            assert!(
                matches!(
                    UnDelegateResourceActuator::new(&c).validate(&ws),
                    Err(ActuatorError::Validate(m)) if m.contains("must be more than 0 TRX")
                ),
                "balance {bal} must be rejected"
            );
        }
    }

    #[test]
    fn undelegate_before_lock_rejected() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 5_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        // locked delegation, expires at NOW + 10 blocks * 3000ms (in the future)
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, true, 10);
        DelegateResourceActuator::new(&c).execute(&mut ws).unwrap();

        let u = undel_contract(&o, &r, BANDWIDTH, 5_000_000);
        assert!(matches!(
            UnDelegateResourceActuator::new(&u).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("insufficient delegatedFrozenBalance(BANDWIDTH)")
        ));
    }

    #[test]
    fn undelegate_after_lock_accepted() {
        let (o, r) = (addr(1), addr(2));
        let mut ws = state_with_owner(&o, BANDWIDTH, 5_000_000);
        seed_account(&mut ws, &r, AccountType::Normal);
        let c = del_contract(&o, &r, BANDWIDTH, 5_000_000, true, 10);
        DelegateResourceActuator::new(&c).execute(&mut ws).unwrap();

        // advance past the lock expiry (NOW + 10*3000 = NOW + 30_000)
        ws.put_prop_i64(props::LATEST_BLOCK_HEADER_TIMESTAMP, NOW + 30_001).unwrap();

        let u = undel_contract(&o, &r, BANDWIDTH, 5_000_000);
        let a = UnDelegateResourceActuator::new(&u);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();

        let owner = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(frozen_v2_amount(&owner, BANDWIDTH), 5_000_000);
        assert_eq!(owner_delegated(&owner, BANDWIDTH), 0);
        // both records gone
        assert!(get_delegation(&ws, &o, &r, false).unwrap().is_none());
        assert!(get_delegation(&ws, &o, &r, true).unwrap().is_none());
    }

    #[test]
    fn undelegate_self_rejected() {
        let o = addr(1);
        let ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        let c = undel_contract(&o, &o, BANDWIDTH, 5_000_000);
        assert!(matches!(
            UnDelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("must not be the same as ownerAddress")
        ));
    }

    #[test]
    fn undelegate_malformed_receiver_rejected() {
        let o = addr(1);
        let ws = state_with_owner(&o, BANDWIDTH, 10_000_000);
        let mut bad = [0u8; ADDRESS_LEN];
        bad[0] = 0x42;
        let c = UnDelegateResourceContract {
            owner_address: o.as_bytes().to_vec(),
            resource: BANDWIDTH,
            balance: 5_000_000,
            receiver_address: bad.to_vec(),
        };
        assert!(matches!(
            UnDelegateResourceActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("Invalid receiverAddress")
        ));
    }

    #[test]
    fn partial_undelegate_conserves_balance() {
        // owner frozen + owner delegated == original; delegated == acquired == record.
        let (o, r) = (addr(1), addr(2));
        let mut ws = setup_delegated(&o, &r, 5_000_000);
        // owner started with 5_000_000 frozen, all delegated. Undelegate 2_000_000.
        let c = undel_contract(&o, &r, BANDWIDTH, 2_000_000);
        let a = UnDelegateResourceActuator::new(&c);
        a.validate(&ws).unwrap();
        a.execute(&mut ws).unwrap();

        let owner = ws.get_account(&o).unwrap().unwrap();
        let recv = ws.get_account(&r).unwrap().unwrap();
        let frozen = frozen_v2_amount(&owner, BANDWIDTH);
        let delegated = owner_delegated(&owner, BANDWIDTH);
        let acquired = receiver_acquired(&recv, BANDWIDTH);
        let rec = get_delegation(&ws, &o, &r, false).unwrap().unwrap();
        assert_eq!(frozen, 2_000_000);
        assert_eq!(delegated, 3_000_000);
        assert_eq!(acquired, 3_000_000);
        assert_eq!(rec_frozen(&rec, BANDWIDTH), 3_000_000);
        // conservation: frozen + delegated == original 5_000_000
        assert_eq!(frozen + delegated, 5_000_000);
    }
}
