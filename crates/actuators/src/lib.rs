//! Transaction execution — one **actuator** per system-contract type (P1).
//!
//! Mirrors java-tron's `actuator` module. Implemented so far:
//! - [`transfer::TransferActuator`] — plain TRX transfer (`TransferContract`),
//!   semantics byte-matched to `org.tron.core.actuator.TransferActuator`.

use thiserror::Error;
use tron_state::StateError;

pub mod account;
pub mod delegate;
pub mod exchange;
pub mod maintenance;
pub mod asset_transfer;
pub mod executor;
pub mod freeze_v2;
pub mod proposal;
pub mod smart_contract;
pub mod transfer;
pub mod vm_host;
pub mod vote;
pub mod withdraw;
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
