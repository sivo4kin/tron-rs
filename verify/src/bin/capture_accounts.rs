//! Capture real account states from a live java-tron node over gRPC.
//!
//! Writes prost-encoded `protocol.Account` fixtures for the state-diff layer of
//! the differential harness (SPEC section 7 item 3): real accounts carry the full
//! richness our state layer must handle (frozen_v2, votes, asset maps, permissions).
//!
//! Usage: capture_accounts [--endpoint URL] <base58-or-hex-address> [...]

use prost::Message;
use tron_proto::protocol::wallet_client::WalletClient;
use tron_proto::protocol::Account;
use tron_types::Address;

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
        anyhow::bail!("usage: capture_accounts [--endpoint URL] <address> [...]");
    }

    let mut client = WalletClient::connect(endpoint.clone()).await?;
    let out_dir = format!(
        "{}/{}/accounts",
        env!("CARGO_MANIFEST_DIR"),
        tron_verify::FIXTURE_DIR
    );
    std::fs::create_dir_all(&out_dir)?;

    for arg in &args {
        let addr = Address::from_base58check(arg)
            .or_else(|_| Address::from_hex(arg))
            .map_err(|e| anyhow::anyhow!("bad address {arg}: {e}"))?;
        let query = Account {
            address: addr.as_bytes().to_vec(),
            ..Default::default()
        };
        let account = client.get_account(query).await?.into_inner();
        if account.address.is_empty() {
            anyhow::bail!("{arg}: account not found on {endpoint}");
        }
        let path = format!("{out_dir}/{}.pb", addr.to_base58check());
        std::fs::write(&path, account.encode_to_vec())?;
        println!(
            "captured {} balance={} frozen_v2={} votes={} -> {path}",
            addr.to_base58check(),
            account.balance,
            account.frozen_v2.len(),
            account.votes.len()
        );
    }
    Ok(())
}
