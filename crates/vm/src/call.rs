//! Unified CALL dispatch: route a call to a precompile or to contract bytecode.
//!
//! Mirrors java-tron's call path: a target address in `0x01..=0x04` (and Tron's
//! extended precompiles) runs a precompile charged its fixed/dynamic energy;
//! otherwise the target's bytecode runs on the interpreter against its storage.
//! This is the entry point contract-to-contract CALL and external invocation use.

use crate::interp::{run, Halt, Host, Outcome};
use crate::precompile;

/// Result of a call: output bytes, energy consumed, and success flag.
#[derive(Debug)]
pub struct CallResult {
    pub success: bool,
    pub output: Vec<u8>,
    pub energy_used: u64,
}

/// Dispatch a call to `address` with `input`, bounded by `energy_limit`.
///
/// - Precompile addresses (low byte 0x01..=0x04) run the precompile: charged its
///   energy; fails (out of energy) if the limit is too low.
/// - Otherwise `code` runs on the interpreter against `host`; a Return/Stop halt
///   is success, Revert/errors are failure. (Bytecode calls ignore `input` here —
///   calldata opcodes land with the full CALL frame model.)
pub fn call(
    address_low_byte: u8,
    input: &[u8],
    code: &[u8],
    energy_limit: u64,
    host: &mut dyn Host,
) -> CallResult {
    if let Some(cost) = precompile::energy_for(address_low_byte, input) {
        if cost > energy_limit {
            return CallResult { success: false, output: vec![], energy_used: energy_limit };
        }
        let output = precompile::execute(address_low_byte, input).unwrap_or_default();
        return CallResult { success: true, output, energy_used: cost };
    }

    // Not a precompile: execute bytecode.
    let Outcome { halt, energy_used, .. } = run(code, energy_limit, host);
    let success = matches!(halt, Halt::Stop | Halt::Return);
    CallResult { success, output: vec![], energy_used }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interp::MemoryHost;
    use crate::opcode::OpCode::*;

    #[test]
    fn call_to_identity_precompile() {
        let mut host = MemoryHost::default();
        let r = call(0x04, b"hello", &[], 1000, &mut host);
        assert!(r.success);
        assert_eq!(r.output, b"hello");
        assert_eq!(r.energy_used, 15 + 3); // identity, one word
    }

    #[test]
    fn call_to_sha256_precompile() {
        let mut host = MemoryHost::default();
        let r = call(0x02, b"abc", &[], 1000, &mut host);
        assert!(r.success);
        assert_eq!(r.output, tron_crypto::sha256(b"abc").to_vec());
    }

    #[test]
    fn precompile_out_of_energy_fails() {
        let mut host = MemoryHost::default();
        // ecrecover needs 3000; give it 100
        let r = call(0x01, &[0u8; 128], &[], 100, &mut host);
        assert!(!r.success);
    }

    #[test]
    fn call_to_bytecode_runs_and_persists() {
        let mut host = MemoryHost::default();
        // SSTORE 7 = 1, STOP
        let code = [Push1 as u8, 1, Push1 as u8, 7, Sstore as u8, Stop as u8];
        let r = call(0x99, &[], &code, 100_000, &mut host); // 0x99 not a precompile
        assert!(r.success);
        assert!(r.energy_used >= 20_000); // SSTORE set dominates
        use primitive_types::U256;
        assert_eq!(host.sload(U256::from(7)), U256::from(1));
    }

    #[test]
    fn call_to_reverting_bytecode_fails() {
        let mut host = MemoryHost::default();
        let code = [crate::opcode::OpCode::Revert as u8];
        let r = call(0x99, &[], &code, 1000, &mut host);
        assert!(!r.success);
    }
}
