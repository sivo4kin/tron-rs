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
