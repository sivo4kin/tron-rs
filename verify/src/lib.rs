//! Differential-verification support: fixture loading shared by tests and tools.

use prost::Message;
use tron_proto::protocol;

/// Directory (relative to this crate) holding captured block fixtures.
pub const FIXTURE_DIR: &str = "fixtures";

/// Load a captured block fixture (`fixtures/<name>.pb`, raw `protocol.Block` bytes).
pub fn load_block(name: &str) -> anyhow::Result<protocol::Block> {
    let path = format!("{}/{}/{}.pb", env!("CARGO_MANIFEST_DIR"), FIXTURE_DIR, name);
    let bytes = std::fs::read(&path)?;
    Ok(protocol::Block::decode(bytes.as_slice())?)
}

/// List all committed block fixtures by name (sorted).
pub fn fixture_names() -> anyhow::Result<Vec<String>> {
    let dir = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), FIXTURE_DIR);
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            n.strip_suffix(".pb").map(str::to_string)
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Subdirectory holding captured contract-call cases (`capture_contract_case`).
pub const CONTRACT_FIXTURE_SUBDIR: &str = "fixtures/contracts";

/// A captured real `TriggerSmartContract` call and its ground-truth receipt/code.
pub struct ContractCase {
    pub label: String,
    /// The real transaction (its `Any`-packed `TriggerSmartContract`).
    pub tx: protocol::Transaction,
    /// `GetTransactionInfoById` receipt: `receipt.energy_usage_total`, `result`, …
    pub info: protocol::TransactionInfo,
    /// The contract's runtime bytecode (`GetContract`).
    pub code: Vec<u8>,
}

/// Load every committed contract-call case (sorted). Returns an empty vec if none
/// are committed yet (the parity test then relies on its controlled case only).
pub fn contract_cases() -> anyhow::Result<Vec<ContractCase>> {
    let dir = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), CONTRACT_FIXTURE_SUBDIR);
    let mut cases = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(cases);
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().into_string().unwrap_or_default();
        let Some(label) = name.strip_suffix(".info.pb") else { continue };
        let base = format!("{dir}/{label}");
        let info = protocol::TransactionInfo::decode(std::fs::read(format!("{base}.info.pb"))?.as_slice())?;
        let tx = protocol::Transaction::decode(std::fs::read(format!("{base}.tx.pb"))?.as_slice())?;
        let code = std::fs::read(format!("{base}.code.bin"))?;
        cases.push(ContractCase { label: label.to_string(), tx, info, code });
    }
    cases.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(cases)
}

// ---------------------------------------------------------------------------
// T10: contract-call pre-state fixtures
// ---------------------------------------------------------------------------

/// The exact state a `TriggerSmartContract` call read at block `N-1` (its pre-state):
/// the contract's touched storage slots and any involved account balances. Absolute
/// energy/storage parity (T11) seeds this, executes the real tx, and asserts the
/// receipt's `energy_usage_total` and post-slots exactly.
///
/// **Fixture format** (`<label>.prestate.pb`, all integers big-endian):
/// ```text
///   magic          "TPS1"                (4 bytes)
///   contract_len   u8  ; contract        (21-byte Tron address)
///   block_number   i64                    (the tx block N; slots are N-1)
///   source_len     u16 ; source           (utf8 provenance tag)
///   storage_count  u32 ; then count × (slot[32] ‖ value[32])
///   account_count  u32 ; then count × (addr_len u8 ‖ addr ‖ balance i64)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreState {
    /// 21-byte Tron contract address the slots belong to.
    pub contract: Vec<u8>,
    /// The transaction's block number `N`; the captured slot/account values are the
    /// state as of `N-1`.
    pub block_number: i64,
    /// Touched storage: 32-byte slot key → 32-byte value.
    pub storage: Vec<([u8; 32], [u8; 32])>,
    /// Involved accounts: 21-byte address → TRX balance (sun).
    pub accounts: Vec<(Vec<u8>, i64)>,
    /// How this pre-state was obtained: `"archive"`, `"replay"`, or
    /// `"hand-verified"` (with a note).
    pub source: String,
}

const PRESTATE_MAGIC: &[u8; 4] = b"TPS1";

impl PreState {
    /// Serialize to the documented `<label>.prestate.pb` byte format.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(PRESTATE_MAGIC);
        b.push(self.contract.len() as u8);
        b.extend_from_slice(&self.contract);
        b.extend_from_slice(&self.block_number.to_be_bytes());
        b.extend_from_slice(&(self.source.len() as u16).to_be_bytes());
        b.extend_from_slice(self.source.as_bytes());
        b.extend_from_slice(&(self.storage.len() as u32).to_be_bytes());
        for (slot, value) in &self.storage {
            b.extend_from_slice(slot);
            b.extend_from_slice(value);
        }
        b.extend_from_slice(&(self.accounts.len() as u32).to_be_bytes());
        for (addr, bal) in &self.accounts {
            b.push(addr.len() as u8);
            b.extend_from_slice(addr);
            b.extend_from_slice(&bal.to_be_bytes());
        }
        b
    }

    /// Parse the documented byte format.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<PreState> {
        let mut c = Cursor { b: bytes, pos: 0 };
        anyhow::ensure!(c.take(4)? == PRESTATE_MAGIC, "bad prestate magic");
        let clen = c.u8()? as usize;
        let contract = c.take(clen)?.to_vec();
        let block_number = i64::from_be_bytes(c.take(8)?.try_into().unwrap());
        let slen = u16::from_be_bytes(c.take(2)?.try_into().unwrap()) as usize;
        let source = String::from_utf8(c.take(slen)?.to_vec())?;
        let scount = u32::from_be_bytes(c.take(4)?.try_into().unwrap()) as usize;
        let mut storage = Vec::with_capacity(scount);
        for _ in 0..scount {
            let slot: [u8; 32] = c.take(32)?.try_into().unwrap();
            let value: [u8; 32] = c.take(32)?.try_into().unwrap();
            storage.push((slot, value));
        }
        let acount = u32::from_be_bytes(c.take(4)?.try_into().unwrap()) as usize;
        let mut accounts = Vec::with_capacity(acount);
        for _ in 0..acount {
            let alen = c.u8()? as usize;
            let addr = c.take(alen)?.to_vec();
            let bal = i64::from_be_bytes(c.take(8)?.try_into().unwrap());
            accounts.push((addr, bal));
        }
        Ok(PreState { contract, block_number, storage, accounts, source })
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(self.pos + n <= self.b.len(), "prestate truncated");
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }
}

/// Load a committed pre-state fixture (`fixtures/contracts/<label>.prestate.pb`).
pub fn load_prestate(label: &str) -> anyhow::Result<PreState> {
    let path = format!(
        "{}/{}/{}.prestate.pb",
        env!("CARGO_MANIFEST_DIR"),
        CONTRACT_FIXTURE_SUBDIR,
        label
    );
    PreState::decode(&std::fs::read(path)?)
}

/// EVM storage slot of `mapping(address => …) m` at declaration slot `p`, for `key`
/// (`keccak256(pad32(key) ‖ pad32(p))`). `key` is the 20-byte EVM address body. Used to
/// locate a TRC20 `_balances[holder]` slot for archive capture.
pub fn mapping_slot(key20: &[u8], slot_index: u64) -> [u8; 32] {
    let mut buf = [0u8; 64];
    let n = key20.len().min(20);
    buf[32 - n..32].copy_from_slice(&key20[..n]); // left-pad the address key
    buf[56..64].copy_from_slice(&slot_index.to_be_bytes()); // right-aligned slot index
    tron_crypto::keccak256(&buf)
}
