//! Capture block fixtures from a live java-tron node over gRPC.
//!
//! Uses our own generated `Wallet` client (`tron-proto`), so a successful capture
//! also exercises the gRPC surface end-to-end against real java-tron.
//!
//! Usage: capture [--endpoint http://grpc.nile.trongrid.io:50051] <block-num> [...]

use prost::Message;
use tron_proto::protocol::wallet_client::WalletClient;
use tron_proto::protocol::NumberMessage;

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
        anyhow::bail!("usage: capture [--endpoint URL] <block-num> [<block-num> ...]");
    }

    let mut client = WalletClient::connect(endpoint.clone()).await?;
    let out_dir = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), tron_verify::FIXTURE_DIR);
    std::fs::create_dir_all(&out_dir)?;

    for arg in &args {
        let num: i64 = arg.parse()?;
        let block = client
            .get_block_by_num(NumberMessage { num })
            .await?
            .into_inner();
        let raw = block.block_header.as_ref().and_then(|h| h.raw_data.as_ref());
        let (height, tx_count) = match raw {
            Some(r) => (r.number, block.transactions.len()),
            None => anyhow::bail!("block {num}: empty response (no header)"),
        };
        let path = format!("{out_dir}/nile-{height}.pb");
        std::fs::write(&path, block.encode_to_vec())?;
        println!("captured block {height} ({tx_count} txs) -> {path}");
    }
    println!("done: {} fixture(s) from {endpoint}", args.len());
    Ok(())
}
