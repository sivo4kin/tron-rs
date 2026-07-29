//! Byte-addressed EVM memory, shared by the unified execution engine ([`crate::frame`])
//! and the single-contract entry ([`crate::interp`]).
//!
//! Memory grows in 32-byte words; [`Memory::expand_to`] resizes and returns the
//! expansion energy to charge (java-tron/EVM quadratic memory cost, via
//! [`crate::energy::memory_expansion_cost`]).

use primitive_types::U256;

#[derive(Default)]
pub(crate) struct Memory {
    pub data: Vec<u8>,
}

impl Memory {
    fn words(&self) -> u64 {
        (self.data.len() as u64 + 31) / 32
    }

    /// Ensure at least `end` bytes exist; returns the expansion energy to charge.
    pub fn expand_to(&mut self, end: usize) -> u64 {
        if end <= self.data.len() {
            return 0;
        }
        let cur = self.words();
        let new_words = (end as u64 + 31) / 32;
        self.data.resize((new_words * 32) as usize, 0);
        crate::energy::memory_expansion_cost(cur, new_words)
    }

    pub fn load(&self, off: usize) -> U256 {
        let mut buf = [0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.data.get(off + i).copied().unwrap_or(0);
        }
        U256::from_big_endian(&buf)
    }

    pub fn store(&mut self, off: usize, val: U256) {
        let bytes = val.to_big_endian();
        self.data[off..off + 32].copy_from_slice(&bytes);
    }
}
