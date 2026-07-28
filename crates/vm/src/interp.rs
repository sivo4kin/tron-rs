//! A minimal stack-based interpreter over a subset of TVM opcodes.
//!
//! This is the P2 spike (SPEC section 5.2): it validates the execution + energy
//! model end to end — a 256-bit stack machine metering each step against the
//! [`crate::energy`] table — independent of the eventual revm-vs-clean-room
//! decision. Arithmetic uses wrapping 256-bit words (via u128 pairs kept simple
//! here as U256 over four u64 limbs would be premature); the subset is enough to
//! prove metering, control flow, and Tron opcode dispatch.

use crate::energy::base_cost;
use crate::opcode::OpCode;
use crate::EnergyMeter;

/// 256-bit word as big-endian bytes (kept opaque; arithmetic is on the low 128 bits
/// for the spike subset — full U256 lands with the revm/clean-room decision).
pub type Word = u128;

#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    Stop,
    Return,
    Revert,
    OutOfEnergy,
    StackUnderflow,
    BadOpcode(u8),
    BadJump,
}

/// Execution outcome: the value left on top of the stack (if any) and how it halted.
#[derive(Debug)]
pub struct Outcome {
    pub halt: Halt,
    pub stack_top: Option<Word>,
    pub energy_used: u64,
}

/// Run `code` with an energy `limit`. Pure/self-contained: no state access (SLOAD/
/// SSTORE and the Tron context opcodes are out of scope for the spike).
pub fn run(code: &[u8], limit: u64) -> Outcome {
    let mut meter = EnergyMeter::new(limit);
    let mut stack: Vec<Word> = Vec::new();
    let mut pc = 0usize;

    macro_rules! pop {
        () => {
            match stack.pop() {
                Some(v) => v,
                None => return done(Halt::StackUnderflow, &stack, &meter),
            }
        };
    }

    while pc < code.len() {
        let byte = code[pc];
        let op = match OpCode::from_u8(byte) {
            Some(o) => o,
            None => return done(Halt::BadOpcode(byte), &stack, &meter),
        };
        if !meter.charge(base_cost(op)) {
            return done(Halt::OutOfEnergy, &stack, &meter);
        }

        match op {
            OpCode::Stop => return done(Halt::Stop, &stack, &meter),
            OpCode::Add => {
                let (a, b) = (pop!(), pop!());
                stack.push(a.wrapping_add(b));
            }
            OpCode::Mul => {
                let (a, b) = (pop!(), pop!());
                stack.push(a.wrapping_mul(b));
            }
            OpCode::Sub => {
                let (a, b) = (pop!(), pop!());
                stack.push(a.wrapping_sub(b));
            }
            OpCode::Div => {
                let (a, b) = (pop!(), pop!());
                stack.push(if b == 0 { 0 } else { a / b });
            }
            OpCode::IsZero => {
                let a = pop!();
                stack.push(u128::from(a == 0));
            }
            OpCode::Lt => {
                let (a, b) = (pop!(), pop!());
                stack.push(u128::from(a < b));
            }
            OpCode::Gt => {
                let (a, b) = (pop!(), pop!());
                stack.push(u128::from(a > b));
            }
            OpCode::Eq => {
                let (a, b) = (pop!(), pop!());
                stack.push(u128::from(a == b));
            }
            OpCode::Pop => {
                let _ = pop!();
            }
            OpCode::Dup1 => {
                let a = *stack.last().unwrap_or(&0);
                if stack.is_empty() {
                    return done(Halt::StackUnderflow, &stack, &meter);
                }
                stack.push(a);
            }
            OpCode::Swap1 => {
                let n = stack.len();
                if n < 2 {
                    return done(Halt::StackUnderflow, &stack, &meter);
                }
                stack.swap(n - 1, n - 2);
            }
            OpCode::Push1 => {
                pc += 1;
                let v = *code.get(pc).unwrap_or(&0);
                stack.push(v as u128);
            }
            OpCode::Jump => {
                let dest = pop!() as usize;
                if code.get(dest) != Some(&(OpCode::Jumpdest as u8)) {
                    return done(Halt::BadJump, &stack, &meter);
                }
                pc = dest;
                continue;
            }
            OpCode::Jumpi => {
                let dest = pop!() as usize;
                let cond = pop!();
                if cond != 0 {
                    if code.get(dest) != Some(&(OpCode::Jumpdest as u8)) {
                        return done(Halt::BadJump, &stack, &meter);
                    }
                    pc = dest;
                    continue;
                }
            }
            OpCode::Jumpdest => {}
            OpCode::Return => return done(Halt::Return, &stack, &meter),
            OpCode::Revert => return done(Halt::Revert, &stack, &meter),
            other => return done(Halt::BadOpcode(other as u8), &stack, &meter),
        }
        pc += 1;
    }
    done(Halt::Stop, &stack, &meter)
}

fn done(halt: Halt, stack: &[Word], meter: &EnergyMeter) -> Outcome {
    Outcome { halt, stack_top: stack.last().copied(), energy_used: meter.used }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::energy::base_cost;
    use crate::opcode::OpCode::*;

    #[test]
    fn push_add_computes_and_meters() {
        // PUSH1 2, PUSH1 3, ADD, STOP -> 5
        let code = [Push1 as u8, 2, Push1 as u8, 3, Add as u8, Stop as u8];
        let out = run(&code, 1000);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(5));
        // energy = 3 (push) + 3 (push) + 3 (add) + 0 (stop)
        assert_eq!(out.energy_used, base_cost(Push1) * 2 + base_cost(Add));
    }

    #[test]
    fn out_of_energy_halts() {
        let code = [Push1 as u8, 2, Push1 as u8, 3, Add as u8];
        let out = run(&code, 5); // only enough for one push
        assert_eq!(out.halt, Halt::OutOfEnergy);
    }

    #[test]
    fn conditional_jump_taken() {
        // PUSH1 1 (cond), PUSH1 7 (dest), JUMPI, PUSH1 0xff, STOP, JUMPDEST@7, PUSH1 42, STOP
        let code = [
            Push1 as u8, 1, Push1 as u8, 7, Jumpi as u8, Push1 as u8, 0xff,
            Jumpdest as u8, Push1 as u8, 42, Stop as u8,
        ];
        let out = run(&code, 1000);
        assert_eq!(out.halt, Halt::Stop);
        assert_eq!(out.stack_top, Some(42)); // jumped past the 0xff push
    }

    #[test]
    fn bad_jump_destination_rejected() {
        // jump to a non-JUMPDEST byte
        let code = [Push1 as u8, 3, Jump as u8, Stop as u8];
        assert_eq!(run(&code, 1000).halt, Halt::BadJump);
    }

    #[test]
    fn stack_underflow_detected() {
        assert_eq!(run(&[Add as u8], 1000).halt, Halt::StackUnderflow);
    }

    #[test]
    fn div_by_zero_is_zero() {
        // PUSH1 0, PUSH1 5, DIV  -> 5 / 0 = 0  (EVM semantics)
        let code = [Push1 as u8, 0, Push1 as u8, 5, Div as u8, Stop as u8];
        assert_eq!(run(&code, 1000).stack_top, Some(0));
    }
}
