//! The unified TVM execution loop: ONE dispatch with memory, the journaled
//! multi-account [`World`], inter-contract CALL, and SELFDESTRUCT.
//!
//! Merges what used to be two half-engines (a memory/opcode interpreter without
//! inter-contract CALL, and a journaled World without memory). Mirrors java-tron
//! `org.tron.core.vm.Program` + `VM.play`. The state model lives in [`crate::frame`];
//! [`crate::interp`] wraps this loop for the single-contract Host path.
//!
//! **Typed-unsupported (documented, never silent):** on the host-bound single-contract
//! path SELFDESTRUCT has no balance/account model, so it halts `BadOpcode(0xff)`.
//! VOTEWITNESS is priced (audit CS-JTRON-005) but its vote effect needs an actuator
//! witness store, so it pushes 0 (no vote recorded); the other Tron staking opcodes
//! decode to `BadOpcode` until a state model backs them.

use crate::energy::{base_cost, sstore_cost};
use crate::frame::{CallResult, Halt, World, MAX_CALL_DEPTH, STACK_LIMIT};
use crate::memory::Memory;
use crate::opcode::OpCode;
use crate::EnergyMeter;
use primitive_types::U256;
use tron_types::Address;

/// Full result of one engine frame (internal; wrappers project it onto their shapes).
pub(crate) struct Exec {
    pub halt: Halt,
    pub energy_used: u64,
    pub return_data: Vec<u8>,
    pub stack_top: Option<U256>,
}

pub(crate) fn halt_is_success(h: &Halt) -> bool {
    matches!(h, Halt::Stop | Halt::Return | Halt::SelfDestruct)
}

/// Execute the code stored for `address` against `world` (the public frame entry used
/// by the multi-account tests / callers).
pub fn execute(world: &mut World, address: &[u8], energy_limit: u64, depth: usize) -> CallResult {
    let code = world.get_code(address);
    let e = run_frame(world, address, &code, &[], energy_limit, depth);
    CallResult { success: halt_is_success(&e.halt), halt: e.halt, energy_used: e.energy_used }
}

/// The one execution loop: memory + stack + journaled World storage + CALL + SELFDESTRUCT.
pub(crate) fn run_frame(
    world: &mut World,
    address: &[u8],
    code: &[u8],
    calldata: &[u8],
    energy_limit: u64,
    depth: usize,
) -> Exec {
    let mut meter = EnergyMeter::new(energy_limit);
    if depth > MAX_CALL_DEPTH {
        return Exec { halt: Halt::DepthLimit, energy_used: 0, return_data: Vec::new(), stack_top: None };
    }
    let mut stack: Vec<U256> = Vec::new();
    let mut mem = Memory::default();
    let mut last_return_data: Vec<u8> = Vec::new();
    let mut pc = 0usize;

    macro_rules! stop {
        ($h:expr) => {
            return Exec { halt: $h, energy_used: meter.used, return_data: Vec::new(), stack_top: stack.last().copied() }
        };
    }
    macro_rules! stop_out {
        ($h:expr, $o:expr) => {
            return Exec { halt: $h, energy_used: meter.used, return_data: $o, stack_top: stack.last().copied() }
        };
    }
    macro_rules! pop {
        () => {
            match stack.pop() { Some(v) => v, None => stop!(Halt::StackUnderflow) }
        };
    }
    macro_rules! push {
        ($v:expr) => {{
            if stack.len() >= STACK_LIMIT { stop!(Halt::StackOverflow); }
            stack.push($v);
        }};
    }
    macro_rules! charge {
        ($amt:expr) => {
            if !meter.charge($amt) { stop!(Halt::OutOfEnergy); }
        };
    }

    while pc < code.len() {
        let byte = code[pc];
        let op = match OpCode::from_u8(byte) {
            Some(o) => o,
            None => stop!(Halt::BadOpcode(byte)),
        };
        // Static per-op base cost; SSTORE is charged dynamically below.
        if op != OpCode::Sstore {
            charge!(base_cost(op));
        }

        match op {
            OpCode::Stop => stop!(Halt::Stop),
            OpCode::Add => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_add(b).0); }
            OpCode::Sub => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_sub(b).0); }
            OpCode::Mul => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_mul(b).0); }
            OpCode::Div => { let (a,b)=(pop!(),pop!()); push!(if b.is_zero(){U256::zero()}else{a/b}); }
            OpCode::Mod => { let (a,b)=(pop!(),pop!()); push!(if b.is_zero(){U256::zero()}else{a%b}); }
            OpCode::Exp => { let (a,b)=(pop!(),pop!()); push!(a.overflowing_pow(b).0); }
            OpCode::Lt => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a<b))); }
            OpCode::Gt => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a>b))); }
            OpCode::Eq => { let (a,b)=(pop!(),pop!()); push!(U256::from(u8::from(a==b))); }
            OpCode::IsZero => { let a=pop!(); push!(U256::from(u8::from(a.is_zero()))); }
            OpCode::And => { let (a,b)=(pop!(),pop!()); push!(a & b); }
            OpCode::Or => { let (a,b)=(pop!(),pop!()); push!(a | b); }
            OpCode::Xor => { let (a,b)=(pop!(),pop!()); push!(a ^ b); }
            OpCode::Pop => { let _=pop!(); }
            OpCode::Dup1 => {
                let a = match stack.last() { Some(v)=>*v, None=> stop!(Halt::StackUnderflow) };
                push!(a);
            }
            OpCode::Swap1 => {
                let n = stack.len();
                if n < 2 { stop!(Halt::StackUnderflow); }
                stack.swap(n-1, n-2);
            }
            OpCode::Push1 => { pc += 1; push!(U256::from(*code.get(pc).unwrap_or(&0))); }
            OpCode::Push2 => {
                let hi = *code.get(pc + 1).unwrap_or(&0) as u16;
                let lo = *code.get(pc + 2).unwrap_or(&0) as u16;
                pc += 2;
                push!(U256::from((hi << 8) | lo));
            }
            OpCode::CallDataLoad => {
                let off = pop!().low_u64() as usize;
                let mut buf = [0u8; 32];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = calldata.get(off + i).copied().unwrap_or(0);
                }
                push!(U256::from_big_endian(&buf));
            }
            OpCode::CallDataSize => push!(U256::from(calldata.len())),
            OpCode::ReturnDataSize => push!(U256::from(last_return_data.len())),
            OpCode::ReturnDataCopy => {
                let dest = pop!().low_u64() as usize;
                let off = pop!().low_u64() as usize;
                let len = pop!().low_u64() as usize;
                charge!(mem.expand_to(dest.saturating_add(len)));
                for i in 0..len {
                    mem.data[dest + i] = last_return_data.get(off + i).copied().unwrap_or(0);
                }
            }
            OpCode::CallDataCopy => {
                let dest = pop!().low_u64() as usize;
                let off = pop!().low_u64() as usize;
                let len = pop!().low_u64() as usize;
                charge!(mem.expand_to(dest.saturating_add(len)));
                for i in 0..len {
                    mem.data[dest + i] = calldata.get(off + i).copied().unwrap_or(0);
                }
            }
            OpCode::Mload => {
                let off = pop!().low_u64() as usize;
                charge!(mem.expand_to(off.saturating_add(32)));
                push!(mem.load(off));
            }
            OpCode::Mstore => {
                let off = pop!().low_u64() as usize;
                let val = pop!();
                charge!(mem.expand_to(off.saturating_add(32)));
                mem.store(off, val);
            }
            OpCode::Mstore8 => {
                let off = pop!().low_u64() as usize;
                let val = pop!();
                charge!(mem.expand_to(off.saturating_add(1)));
                mem.data[off] = (val.low_u64() & 0xff) as u8;
            }
            OpCode::Sload => { let key = pop!(); push!(world.sload(address, key)); }
            OpCode::Sstore => {
                let key = pop!();
                let value = pop!();
                let current = world.sload(address, key);
                charge!(sstore_cost(current.is_zero(), value.is_zero()));
                world.sstore(address, key, value);
            }
            OpCode::Jump => {
                let dest = pop!();
                let d = dest.low_u64() as usize;
                if dest > U256::from(code.len()) || code.get(d) != Some(&(OpCode::Jumpdest as u8)) {
                    stop!(Halt::BadJump);
                }
                pc = d; continue;
            }
            OpCode::Jumpi => {
                let dest = pop!();
                let cond = pop!();
                if !cond.is_zero() {
                    let d = dest.low_u64() as usize;
                    if dest > U256::from(code.len()) || code.get(d) != Some(&(OpCode::Jumpdest as u8)) {
                        stop!(Halt::BadJump);
                    }
                    pc = d; continue;
                }
            }
            OpCode::Jumpdest => {}
            OpCode::Call => {
                // EVM CALL: gas, addr, value, argsOffset, argsLen, retOffset, retLen.
                let requested_gas = pop!();
                let addr_word = pop!();
                let _value = pop!();
                let args_off = pop!().low_u64() as usize;
                let args_len = pop!().low_u64() as usize;
                let ret_off = pop!().low_u64() as usize;
                let ret_len = pop!().low_u64() as usize;

                charge!(mem.expand_to(args_off.saturating_add(args_len)));
                let input = mem.data.get(args_off..args_off + args_len).unwrap_or(&[]).to_vec();

                let low = (addr_word.low_u64() & 0xff) as u8;
                let is_precompile = addr_word.leading_zeros() >= 248
                    && crate::precompile::energy_for(low, &input).is_some();

                let (success, output) = if is_precompile {
                    let cost = crate::precompile::energy_for(low, &input).unwrap();
                    charge!(cost);
                    (true, crate::precompile::execute(low, &input).unwrap_or_default())
                } else {
                    // Inter-contract CALL: callee = low 20 bytes of the address word.
                    let buf = addr_word.to_big_endian();
                    let callee = buf[12..].to_vec();
                    let callee_code = world.get_code(&callee);
                    if callee_code.is_empty() {
                        // No code (or host-bound path with no other contracts) -> 0.
                        (false, Vec::new())
                    } else {
                        // EIP-150 all-but-64th: keep >= 1/64 for the caller.
                        let remaining = meter.remaining();
                        let max_forward = remaining - remaining / 64;
                        let forwarded = requested_gas.low_u64().min(max_forward);
                        let cp = world.checkpoint();
                        let sub = run_frame(world, &callee, &callee_code, &input, forwarded, depth + 1);
                        let _ = meter.charge(sub.energy_used);
                        let ok = halt_is_success(&sub.halt);
                        if ok { world.commit(cp); } else { world.revert_to(cp); }
                        (ok, sub.return_data)
                    }
                };

                last_return_data = output.clone();
                if success && ret_len > 0 {
                    charge!(mem.expand_to(ret_off.saturating_add(ret_len)));
                    let n = ret_len.min(output.len());
                    mem.data[ret_off..ret_off + n].copy_from_slice(&output[..n]);
                }
                push!(U256::from(u8::from(success)));
            }
            OpCode::Return | OpCode::Revert => {
                let off = pop!().low_u64() as usize;
                let len = pop!().low_u64() as usize;
                charge!(mem.expand_to(off.saturating_add(len)));
                let output = mem.data.get(off..off + len).unwrap_or(&[]).to_vec();
                let halt = if op == OpCode::Return { Halt::Return } else { Halt::Revert };
                stop_out!(halt, output);
            }
            OpCode::VoteWitness => {
                // Stack (java-tron getVoteWitnessCost2), top-first: amountLen, amountOff,
                // witnessLen, witnessOff. Priced (CS-JTRON-005); vote effect unsupported
                // here (needs an actuator witness store) -> push 0, no vote recorded.
                let amount_len = pop!().low_u64();
                let amount_off = pop!().low_u64();
                let witness_len = pop!().low_u64();
                let witness_off = pop!().low_u64();
                charge!(crate::energy::VOTE_WITNESS);
                let needed = crate::energy::vote_witness_mem_needed(witness_off, witness_len, amount_off, amount_len);
                charge!(mem.expand_to(needed as usize));
                push!(U256::zero());
            }
            OpCode::SelfDestruct => {
                // No balance/account model on the host-bound single-contract path.
                if world.host_backed() {
                    stop!(Halt::BadOpcode(byte));
                }
                let word = pop!();
                let wb = word.to_big_endian();
                let benef_body = <[u8; 20]>::try_from(&wb[12..32]).unwrap();
                let beneficiary = Address::from_body(benef_body);
                let self_addr = match <[u8; 20]>::try_from(address) {
                    Ok(body) => Address::from_body(body),
                    Err(_) => stop!(Halt::BadOpcode(byte)),
                };
                // getSuicideCost2: surcharge when the beneficiary account is absent.
                charge!(crate::energy::suicide_cost(world.account_exists(&beneficiary)));
                world.suicide(&self_addr, &beneficiary);
                stop!(Halt::SelfDestruct);
            }
            other => stop!(Halt::BadOpcode(other as u8)),
        }
        pc += 1;
    }
    stop!(Halt::Stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcode::OpCode::*;

    fn addr(b: u8) -> Vec<u8> {
        vec![b; 20]
    }
    /// 20-byte address whose only set byte is `lo` (byte 19).
    fn addr_lo(lo: u8) -> Vec<u8> {
        let mut v = vec![0u8; 20];
        v[19] = lo;
        v
    }
    fn benef_of(lo: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = lo;
        Address::from_body(b)
    }
    /// 7-arg CALL to a one-byte address `lo` with `gas` (value/args/ret all zero).
    fn call_bytes(lo: u8, gas_hi: u8, gas_lo: u8) -> Vec<u8> {
        vec![
            Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0, Push1 as u8, 0,
            Push1 as u8, lo, Push2 as u8, gas_hi, gas_lo, Call as u8,
        ]
    }

    #[test]
    fn contract_to_contract_call_persists_callee_storage() {
        let mut world = World::new();
        let a = addr(0xaa);
        let b = addr_lo(0x07);
        world.set_code(&b, vec![Push1 as u8, 9, Push1 as u8, 1, Sstore as u8, Stop as u8]);
        let mut acode = call_bytes(0x07, 0xff, 0xff);
        acode.push(Stop as u8);
        world.set_code(&a, acode);
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success);
        assert_eq!(world.sload(&b, U256::from(1)), U256::from(9));
    }

    #[test]
    fn reverting_subcall_rolls_back_only_its_writes() {
        let mut world = World::new();
        let a = addr(0xaa);
        let cp0 = world.checkpoint();
        world.sstore(&a, U256::from(5), U256::from(3));
        world.commit(cp0);
        world.set_code(&a, vec![Push1 as u8, 99, Push1 as u8, 5, Sstore as u8, Push1 as u8, 0, Push1 as u8, 0, Revert as u8]);
        let cp = world.checkpoint();
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(!r.success);
        assert_eq!(r.halt, Halt::Revert);
        world.revert_to(cp);
        assert_eq!(world.sload(&a, U256::from(5)), U256::from(3));
    }

    #[test]
    fn call_reserves_one_sixtyfourth_of_gas() {
        let mut world = World::new();
        let a = addr(0xaa);
        let callee = addr_lo(0x09);
        world.set_code(&callee, vec![
            Push1 as u8, 1, Push1 as u8, 1, Sstore as u8,
            Push1 as u8, 1, Push1 as u8, 2, Sstore as u8, Stop as u8,
        ]);
        let mut acode = call_bytes(0x09, 0x00, 0x64);
        acode.push(Stop as u8);
        world.set_code(&a, acode);
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success, "caller must survive a callee OOG");
        assert_eq!(world.sload(&callee, U256::from(1)), U256::zero());
    }

    #[test]
    fn depth_limit_enforced() {
        let mut world = World::new();
        let a = addr(0xaa);
        world.set_code(&a, vec![Stop as u8]);
        let r = execute(&mut world, &a, 1000, MAX_CALL_DEPTH + 1);
        assert_eq!(r.halt, Halt::DepthLimit);
    }

    // -- Unified engine: memory + journaled nested CALL (T01 acceptance) ----

    #[test]
    fn unified_engine_memory_and_journaled_nested_call_revert() {
        // ONE run: A uses MEMORY (MSTORE/MLOAD) and makes a nested inter-contract CALL
        // into B, which writes storage then REVERTs; the journal rolls B's write back.
        let mut world = World::new();
        let a = addr(0xaa);
        let b = addr_lo(0xbb);
        world.set_code(&b, vec![Push1 as u8, 7, Push1 as u8, 3, Sstore as u8, Push1 as u8, 0, Push1 as u8, 0, Revert as u8]);
        let mut acode = vec![
            Push1 as u8, 42, Push1 as u8, 0, Mstore as u8,
            Push1 as u8, 0, Mload as u8, Pop as u8,
        ];
        acode.extend(call_bytes(0xbb, 0xff, 0xff));
        acode.push(Stop as u8);
        world.set_code(&a, acode);

        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success, "caller survives a reverting callee");
        assert_eq!(r.halt, Halt::Stop);
        assert_eq!(world.sload(&b, U256::from(3)), U256::zero());
    }

    #[test]
    fn unified_engine_committed_nested_call_persists() {
        let mut world = World::new();
        let a = addr(0xaa);
        let b = addr_lo(0xbb);
        world.set_code(&b, vec![Push1 as u8, 7, Push1 as u8, 3, Sstore as u8, Stop as u8]);
        let mut acode = call_bytes(0xbb, 0xff, 0xff);
        acode.push(Stop as u8);
        world.set_code(&a, acode);
        let r = execute(&mut world, &a, 1_000_000, 0);
        assert!(r.success);
        assert_eq!(world.sload(&b, U256::from(3)), U256::from(7));
    }

    // -- SELFDESTRUCT via the engine (H03) --------------------------------

    fn run_suicide(lo: u8, bal: i64, benef_exists: bool) -> (u64, World<'static>, Address) {
        let mut world = World::new();
        let cbody = [0xaa; 20];
        let contract = Address::from_body(cbody);
        world.set_balance(&contract, bal);
        if benef_exists {
            world.create_account(&benef_of(lo));
        }
        world.set_code(&cbody, vec![Push1 as u8, lo, SelfDestruct as u8]);
        let r = execute(&mut world, &cbody, 1_000_000, 0);
        assert!(r.success);
        assert_eq!(r.halt, Halt::SelfDestruct);
        (r.energy_used, world, contract)
    }

    #[test]
    fn suicide_to_absent_beneficiary_charges_new_account_energy() {
        let (energy, world, contract) = run_suicide(0x07, 1_000, false);
        assert!(energy >= crate::energy::NEW_ACCT_CALL, "energy {energy} must include NEW_ACCT_CALL");
        assert_eq!(world.balance(&benef_of(0x07)), 1_000);
        assert_eq!(world.balance(&contract), 0);
        assert!(world.account_exists(&benef_of(0x07)));
        assert!(world.is_suicided(&contract));
        assert_eq!(world.burned(), 0);
    }

    #[test]
    fn suicide_new_account_surcharge_is_exactly_new_acct_call() {
        let (absent, _, _) = run_suicide(0x07, 10, false);
        let (present, _, _) = run_suicide(0x07, 10, true);
        assert_eq!(absent - present, crate::energy::NEW_ACCT_CALL);
    }
}
