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
