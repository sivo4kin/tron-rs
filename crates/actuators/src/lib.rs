//! Transaction execution — one **actuator** per system-contract type (P1).
//!
//! Mirrors java-tron's `actuator` module: transfer, TRC10, freeze/stake, vote,
//! witness, proposal, exchange, plus the smart-contract path that enters the TVM.
//! Each actuator validates then applies, metering bandwidth/energy.

use thiserror::Error;
use tron_state::WorldState;
use tron_storage::KvStore;

#[derive(Debug, Error)]
pub enum ActuatorError {
    #[error("validation failed: {0}")]
    Validate(String),
    #[error("execution failed: {0}")]
    Execute(String),
}

/// A system-contract executor. Implemented per contract type in P1.
pub trait Actuator<S: KvStore> {
    /// Static/stateful validation before execution.
    fn validate(&self, state: &WorldState<S>) -> Result<(), ActuatorError>;
    /// Apply the state transition.
    fn execute(&self, state: &mut WorldState<S>) -> Result<(), ActuatorError>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        // P1 fills in concrete actuators; this asserts the trait/crate compiles.
        assert!(true);
    }
}
