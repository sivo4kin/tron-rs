//! Capture a transaction's block context by tx id, for building future full-replay /
//! state-diff parity fixtures (V02).
//!
//! Given a tx id, resolves its block via `GetTransactionInfoById`, then captures block
//! `N` (the post-state block that contains the tx) and block `N-1` (the pre-state
//! block) as ordinary block fixtures — the same `.pb` format `load_block` reads.
//!
//! Note: deriving each account's absolute pre/post state from these two blocks needs a
//! whole-chain replay up to `N-1` (or an archive node with historical `getAccount`,
//! which the standard gRPC surface does not expose). Until that replay harness exists,
//! the committed offline parity test (`tests/transfer_parity.rs`) asserts DIFFERENTIAL
//! deltas against a seeded pre-state rather than absolute post-state.
//!
//! Usage: `capture_txcase [--endpoint http://grpc.nile.trongrid.io:50051] <tx-id-hex> [...]`

use prost::Message;
use tron_proto::protocol::wallet_client::WalletClient;
use tron_proto::protocol::{BytesMessage, NumberMessage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = match args.iter().position(|a| a == "--endpoint") {
        Some(i) => {
            args.remove(i);
            args.remove(i)
        }
        None => "http://grpc.nile.trongrid.io:50051".to_string(),
    };
    if args.is_empty() {
        anyhow::bail!("usage: capture_txcase [--endpoint URL] <tx-id-hex> [<tx-id-hex> ...]");
    }

    let network = if endpoint.contains("nile") {
        "nile"
    } else if endpoint.contains("shasta") {
        "shasta"
    } else {
        "mainnet"
    };

    let mut client = WalletClient::connect(endpoint.clone()).await?;
    let out_dir = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), tron_verify::FIXTURE_DIR);
    std::fs::create_dir_all(&out_dir)?;

    for tx_id_hex in &args {
        let id = hex::decode(tx_id_hex.trim_start_matches("0x"))?;
        let info = client
            .get_transaction_info_by_id(BytesMessage { value: id })
            .await?
            .into_inner();
        let n = info.block_number;
        if n <= 0 {
            anyhow::bail!("tx {tx_id_hex}: no block number (unconfirmed or unknown)");
        }

        // Capture the pre-state block (N-1) and the post-state block (N).
        for height in [n - 1, n] {
            let block = client
                .get_block_by_num(NumberMessage { num: height })
                .await?
                .into_inner();
            let path = format!("{out_dir}/{network}-{height}.pb");
            std::fs::write(&path, block.encode_to_vec())?;
            println!("captured block {height} ({} txs) -> {path}", block.transactions.len());
        }
        println!("tx {tx_id_hex} is in block {n} (ts {})", info.block_time_stamp);
    }
    println!("done: {} tx-case(s) from {endpoint}", args.len());
    Ok(())
}
