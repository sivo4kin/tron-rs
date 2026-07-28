//! The TVM — Tron's EVM-derived virtual machine (P2).
//!
//! Decision (SPEC §5.2): adapt modern `revm` to Tron semantics, using opentron's
//! `tvm` crate as the authoritative reference for the Tron opcode set (`0xd0–0xdb`:
//! CALLTOKEN, STAKE, UNSTAKE, WITHDRAWREWARD, ISWITNESS, …), precompile energy
//! costs, and java-tron quirk-parity flags (e.g. `has_buggy_origin`). An early P2
//! spike validates that revm's gas model can host Tron's energy meter; the fallback
//! is a clean-room Sputnik-style VM. This crate holds the energy meter today.

/// Energy is Tron's gas. Default price is 100 sun/energy (governance-adjustable).
pub const DEFAULT_ENERGY_FEE_SUN: i64 = 100;

/// Meters energy consumption for contract execution (P2).
#[derive(Debug, Clone, Copy)]
pub struct EnergyMeter {
    pub limit: u64,
    pub used: u64,
}

impl EnergyMeter {
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    /// Charge `amount` energy; returns false if it would exceed the limit (OOG).
    pub fn charge(&mut self, amount: u64) -> bool {
        match self.used.checked_add(amount) {
            Some(u) if u <= self.limit => {
                self.used = u;
                true
            }
            _ => false,
        }
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_meter_charges_and_ooges() {
        let mut m = EnergyMeter::new(100);
        assert!(m.charge(60));
        assert_eq!(m.remaining(), 40);
        assert!(!m.charge(50)); // would exceed limit -> out of energy
        assert_eq!(m.used, 60); // rejected charge does not apply
    }
}
