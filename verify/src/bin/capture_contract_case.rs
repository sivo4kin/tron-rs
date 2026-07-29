//! Capture a real `TriggerSmartContract` call as a differential-parity fixture (T03).
//!
//! For a given tx id (or auto-discovered from recent blocks), fetches the ground truth
//! from a live java-tron node and writes it under `fixtures/contracts/<label>.*`:
//! - `<label>.tx.pb`   — the raw `protocol.Transaction` (real `Any`-packed contract),
//! - `<label>.info.pb` — its `protocol.TransactionInfo` receipt
//!   (`receipt.energy_usage_total`, `result`, `contract_result`),
//! - `<label>.code.bin`— the contract's runtime `bytecode` (`GetContract`).
//!
//! The offline parity test (`tests/contract_call_parity.rs`) reads these. Note: the
//! historical *pre-state storage* the call read is NOT captured — the standard gRPC
//! surface exposes no archival `getStorageAt`, so absolute energy parity for an
//! SSTORE-heavy call needs an archive node / full-chain replay (SPEC §7). The committed
//! test therefore asserts the deterministic energy + storage delta on a controlled
//! contract, and asserts the well-formedness of + reports against the captured receipt.
//!
//! Usage:
//!   capture_contract_case [--endpoint URL] <tx-id-hex> [...]
//!   capture_contract_case [--endpoint URL] --discover [depth]   # scan recent blocks

use prost::Message;
use tron_proto::protocol::transaction::contract::ContractType;
use tron_proto::protocol::wallet_client::WalletClient;
use tron_proto::protocol::{BytesMessage, EmptyMessage, NumberMessage, TransactionInfo};

/// TRC20 `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

type Client = WalletClient<tonic::transport::Channel>;

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
    let network = if endpoint.contains("nile") {
        "nile"
    } else if endpoint.contains("shasta") {
        "shasta"
    } else {
        "mainnet"
    };

    let mut client = WalletClient::connect(endpoint.clone()).await?;
    let out_dir = format!("{}/{}/contracts", env!("CARGO_MANIFEST_DIR"), tron_verify::FIXTURE_DIR);
    std::fs::create_dir_all(&out_dir)?;

    let tx_ids: Vec<Vec<u8>> = if let Some(i) = args.iter().position(|a| a == "--discover") {
        let depth: i64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(200);
        match discover_transfer(&mut client, depth).await? {
            Some(id) => vec![id],
            None => anyhow::bail!("no TRC20 transfer TriggerSmartContract found in last {depth} blocks"),
        }
    } else if args.is_empty() {
        anyhow::bail!("usage: capture_contract_case [--endpoint URL] (<tx-id-hex> ... | --discover [depth])");
    } else {
        args.iter().map(|h| hex::decode(h.trim_start_matches("0x"))).collect::<Result<_, _>>()?
    };

    for id in tx_ids {
        capture_one(&mut client, network, &out_dir, id).await?;
    }
    println!("done: fixtures in {out_dir}");
    Ok(())
}

/// Scan back `depth` blocks from head for the first successful TRC20-`transfer`
/// `TriggerSmartContract` that actually consumed energy, returning its tx id.
async fn discover_transfer(client: &mut Client, depth: i64) -> anyhow::Result<Option<Vec<u8>>> {
    let head = client.get_now_block(EmptyMessage {}).await?.into_inner();
    let head_num = head
        .block_header
        .and_then(|h| h.raw_data)
        .map(|r| r.number)
        .unwrap_or(0);
    println!("head block {head_num}; scanning back {depth}");

    for num in (head_num - depth..head_num).rev() {
        let infos = client
            .get_transaction_info_by_block_num(NumberMessage { num })
            .await?
            .into_inner()
            .transaction_info;
        for info in infos {
            let energy = info.receipt.as_ref().map(|r| r.energy_usage_total).unwrap_or(0);
            if energy == 0 || info.result != 0 {
                continue; // not a successful, energy-consuming contract call
            }
            let tx = client
                .get_transaction_by_id(BytesMessage { value: info.id.clone() })
                .await?
                .into_inner();
            if is_trc20_transfer(&tx) {
                println!("found transfer tx {} in block {num} (energy {energy})", hex::encode(&info.id));
                return Ok(Some(info.id));
            }
        }
    }
    Ok(None)
}

fn is_trc20_transfer(tx: &tron_proto::protocol::Transaction) -> bool {
    let Some(raw) = tx.raw_data.as_ref() else { return false };
    let Some(contract) = raw.contract.first() else { return false };
    if contract.r#type() != ContractType::TriggerSmartContract {
        return false;
    }
    let Some(any) = contract.parameter.as_ref() else { return false };
    let Ok(trigger) = tron_proto::protocol::TriggerSmartContract::decode(any.value.as_slice())
    else {
        return false;
    };
    trigger.data.len() >= 4 && trigger.data[..4] == TRANSFER_SELECTOR
}

async fn capture_one(
    client: &mut Client,
    network: &str,
    out_dir: &str,
    id: Vec<u8>,
) -> anyhow::Result<()> {
    let info: TransactionInfo = client
        .get_transaction_info_by_id(BytesMessage { value: id.clone() })
        .await?
        .into_inner();
    if info.block_number == 0 {
        anyhow::bail!("tx {}: no receipt (unconfirmed/unknown)", hex::encode(&id));
    }
    let tx = client
        .get_transaction_by_id(BytesMessage { value: id.clone() })
        .await?
        .into_inner();
    let contract = client
        .get_contract(BytesMessage { value: info.contract_address.clone() })
        .await?
        .into_inner();

    let label = format!("{network}-{}", &hex::encode(&id)[..16]);
    std::fs::write(format!("{out_dir}/{label}.tx.pb"), tx.encode_to_vec())?;
    std::fs::write(format!("{out_dir}/{label}.info.pb"), info.encode_to_vec())?;
    std::fs::write(format!("{out_dir}/{label}.code.bin"), &contract.bytecode)?;

    let energy = info.receipt.as_ref().map(|r| r.energy_usage_total).unwrap_or(0);
    println!(
        "captured {label}: energy_usage_total={energy} result={} code={} bytes -> {out_dir}",
        info.result,
        contract.bytecode.len()
    );
    Ok(())
}
