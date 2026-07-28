//! `AccountPermissionUpdateContract` — set an account's multisig permissions.
//!
//! Mirrors java-tron `AccountPermissionUpdateActuator` (core rules): the owner
//! account must exist; exactly one **owner** permission is required with a
//! positive threshold and 1..=5 keys whose weights sum to >= threshold; up to 8
//! **active** permissions with the same key/threshold rules; an optional witness
//! permission (only for SR accounts). Execute replaces the account's permissions.

use crate::{ActuatorError, ExecutionResult};
use tron_proto::protocol::{AccountPermissionUpdateContract, Permission};
use tron_state::WorldState;
use tron_storage::KvStore;
use tron_types::{Address, ADDRESS_LEN};

pub const MAX_ACTIVE_PERMISSIONS: usize = 8;
pub const MAX_KEYS: usize = 5;

fn parse_address(bytes: &[u8]) -> Result<Address, ActuatorError> {
    let arr: [u8; ADDRESS_LEN] = bytes
        .try_into()
        .map_err(|_| ActuatorError::Validate("invalid ownerAddress".into()))?;
    Address::from_bytes(arr).map_err(|_| ActuatorError::Validate("invalid ownerAddress".into()))
}

/// Validate a single permission's keys/threshold (java-tron `checkPermission`).
fn check_permission(p: &Permission) -> Result<(), ActuatorError> {
    if p.keys.is_empty() || p.keys.len() > MAX_KEYS {
        return Err(ActuatorError::Validate(format!(
            "number of keys in permission should not be greater than {MAX_KEYS}"
        )));
    }
    if p.threshold <= 0 {
        return Err(ActuatorError::Validate("permission's threshold should be greater than 0".into()));
    }
    let mut weight_sum: i64 = 0;
    for k in &p.keys {
        if k.weight <= 0 {
            return Err(ActuatorError::Validate("key's weight should be greater than 0".into()));
        }
        if k.address.len() != ADDRESS_LEN || k.address.first() != Some(&tron_types::ADDRESS_PREFIX) {
            return Err(ActuatorError::Validate("key is not a validate address".into()));
        }
        weight_sum = weight_sum
            .checked_add(k.weight)
            .ok_or_else(|| ActuatorError::Validate("long overflow".into()))?;
    }
    if weight_sum < p.threshold {
        return Err(ActuatorError::Validate(
            "sum of all key's weight should not be less than threshold".into(),
        ));
    }
    Ok(())
}

pub struct AccountPermissionUpdateActuator<'a> {
    contract: &'a AccountPermissionUpdateContract,
}

impl<'a> AccountPermissionUpdateActuator<'a> {
    pub fn new(contract: &'a AccountPermissionUpdateContract) -> Self {
        Self { contract }
    }

    pub fn validate<S: KvStore>(&self, state: &WorldState<S>) -> Result<i64, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        if state.get_account(&owner)?.is_none() {
            return Err(ActuatorError::Validate("ownerAddress account does not exist".into()));
        }
        let owner_perm = self
            .contract
            .owner
            .as_ref()
            .ok_or_else(|| ActuatorError::Validate("owner permission is missing".into()))?;
        check_permission(owner_perm)?;

        if self.contract.actives.len() > MAX_ACTIVE_PERMISSIONS {
            return Err(ActuatorError::Validate(format!(
                "number of active permissions should not be greater than {MAX_ACTIVE_PERMISSIONS}"
            )));
        }
        if self.contract.actives.is_empty() {
            return Err(ActuatorError::Validate("active permission is missing".into()));
        }
        for a in &self.contract.actives {
            check_permission(a)?;
        }
        Ok(0)
    }

    pub fn execute<S: KvStore>(
        &self,
        state: &mut WorldState<S>,
    ) -> Result<ExecutionResult, ActuatorError> {
        let owner = parse_address(&self.contract.owner_address)?;
        let mut account = state
            .get_account(&owner)?
            .ok_or_else(|| ActuatorError::Execute("owner account missing".into()))?;
        account.owner_permission = self.contract.owner.clone();
        account.witness_permission = self.contract.witness.clone();
        account.active_permission = self.contract.actives.clone();
        state.put_account(&owner, &account)?;
        Ok(ExecutionResult { fee: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tron_proto::protocol;
    use tron_storage::MemoryStore;

    fn addr(b: u8) -> Address {
        Address::from_body([b; 20])
    }

    fn key(a: &Address, weight: i64) -> protocol::Key {
        protocol::Key { address: a.as_bytes().to_vec(), weight }
    }

    fn perm(name: &str, threshold: i64, keys: Vec<protocol::Key>) -> Permission {
        Permission { permission_name: name.into(), threshold, keys, ..Default::default() }
    }

    fn seeded(owner: &Address) -> WorldState<MemoryStore> {
        let ws = WorldState::new(MemoryStore::new());
        ws.put_account(owner, &protocol::Account {
            address: owner.as_bytes().to_vec(), ..Default::default()
        }).unwrap();
        ws
    }

    fn contract(owner: &Address, o: Permission, actives: Vec<Permission>) -> AccountPermissionUpdateContract {
        AccountPermissionUpdateContract {
            owner_address: owner.as_bytes().to_vec(),
            owner: Some(o),
            witness: None,
            actives,
        }
    }

    #[test]
    fn happy_path_sets_permissions() {
        let (o, k) = (addr(1), addr(2));
        let mut ws = seeded(&o);
        let c = contract(&o,
            perm("owner", 1, vec![key(&k, 1)]),
            vec![perm("active", 2, vec![key(&k, 1), key(&addr(3), 1)])]);
        let a = AccountPermissionUpdateActuator::new(&c);
        assert_eq!(a.validate(&ws).unwrap(), 0);
        a.execute(&mut ws).unwrap();
        let acc = ws.get_account(&o).unwrap().unwrap();
        assert_eq!(acc.owner_permission.unwrap().threshold, 1);
        assert_eq!(acc.active_permission.len(), 1);
    }

    #[test]
    fn rejects_missing_owner_account() {
        let ws = WorldState::new(MemoryStore::new());
        let c = contract(&addr(1), perm("owner", 1, vec![key(&addr(2), 1)]), vec![perm("active", 1, vec![key(&addr(2), 1)])]);
        assert!(matches!(
            AccountPermissionUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("does not exist")
        ));
    }

    #[test]
    fn rejects_threshold_exceeding_weight_sum() {
        let o = addr(1);
        let ws = seeded(&o);
        // threshold 5 but only weight 1 available
        let c = contract(&o, perm("owner", 5, vec![key(&addr(2), 1)]), vec![perm("active", 1, vec![key(&addr(2), 1)])]);
        assert!(matches!(
            AccountPermissionUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("threshold")
        ));
    }

    #[test]
    fn rejects_too_many_keys() {
        let o = addr(1);
        let ws = seeded(&o);
        let keys: Vec<_> = (0..6).map(|i| key(&addr(10 + i), 1)).collect();
        let c = contract(&o, perm("owner", 1, keys), vec![perm("active", 1, vec![key(&addr(2), 1)])]);
        assert!(matches!(
            AccountPermissionUpdateActuator::new(&c).validate(&ws),
            Err(ActuatorError::Validate(m)) if m.contains("number of keys")
        ));
    }

    #[test]
    fn rejects_missing_owner_permission() {
        let o = addr(1);
        let ws = seeded(&o);
        let c = AccountPermissionUpdateContract {
            owner_address: o.as_bytes().to_vec(), owner: None, witness: None,
            actives: vec![perm("active", 1, vec![key(&addr(2), 1)])],
        };
        assert!(AccountPermissionUpdateActuator::new(&c).validate(&ws).is_err());
    }
}
