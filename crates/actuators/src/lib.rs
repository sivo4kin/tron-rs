//! Transaction execution — one **actuator** per system-contract type (P1).
//!
//! Mirrors java-tron's `actuator` module. Implemented so far:
//! - [`transfer::TransferActuator`] — plain TRX transfer (`TransferContract`),
//!   semantics byte-matched to `org.tron.core.actuator.TransferActuator`.

use thiserror::Error;
use tron_state::StateError;

pub mod account;
pub mod asset_issue;
pub mod brokerage;
pub mod cancel_unfreeze;
pub mod clear_abi;
pub mod delegate;
pub mod exchange;
pub mod freeze_v1;
pub mod maintenance;
pub mod market;
pub mod asset_transfer;
pub mod participate_asset_issue;
pub mod executor;
pub mod freeze_v2;
pub mod permission;
pub mod proposal;
pub mod smart_contract;
pub mod transfer;
pub mod unfreeze_asset;
pub mod update_asset;
pub mod update_energy_limit;
pub mod update_setting;
pub mod vm_host;
pub mod vote;
pub mod withdraw;
pub mod withdraw_unfreeze;
pub mod witness;

#[derive(Debug, Error, PartialEq)]
pub enum ActuatorError {
    /// Rejected by validation (java-tron `ContractValidateException`).
    #[error("validation failed: {0}")]
    Validate(String),
    /// Failed during execution (java-tron `ContractExeException`).
    #[error("execution failed: {0}")]
    Execute(String),
    #[error("state error: {0}")]
    State(String),
}

impl From<StateError> for ActuatorError {
    fn from(e: StateError) -> Self {
        ActuatorError::State(e.to_string())
    }
}

/// Result of a successful execution (java-tron `TransactionResultCapsule` subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutionResult {
    /// Fee charged, in sun.
    pub fee: i64,
}

/// Require a committee feature gate to be active, else reject with `err_msg`.
///
/// Mirrors java-tron actuators' opening guard
/// `if (!dynamicStore.getAllowX()) throw new ContractValidateException(...)`.
/// `key` is an `ALLOW_*` dynamic-property key (see
/// [`tron_state::features::flags`]); the feature is on iff the property equals 1.
pub fn require_feature<S: tron_storage::KvStore>(
    state: &tron_state::WorldState<S>,
    key: &str,
    err_msg: &str,
) -> Result<(), ActuatorError> {
    if state.feature_enabled(key) {
        Ok(())
    } else {
        Err(ActuatorError::Validate(err_msg.into()))
    }
}
