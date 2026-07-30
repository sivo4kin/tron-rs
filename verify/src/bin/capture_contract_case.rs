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

use anyhow::Context;
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

    let out_dir = format!("{}/{}/contracts", env!("CARGO_MANIFEST_DIR"), tron_verify::FIXTURE_DIR);
    std::fs::create_dir_all(&out_dir)?;

    // Offline: emit the committed hand-verified pre-state sample (no network needed).
    if args.iter().any(|a| a == "--emit-sample") {
        emit_controlled_sample(&out_dir)?;
        return Ok(());
    }

    let mut client = WalletClient::connect(endpoint.clone()).await?;

    // Archive pre-state capture for one tx id (T10 source #1).
    if let Some(i) = args.iter().position(|a| a == "--prestate") {
        let tx_hex = args.get(i + 1).cloned().unwrap_or_else(|| {
            eprintln!("usage: capture_contract_case --prestate <tx-id-hex>");
            std::process::exit(2);
        });
        let id = hex::decode(tx_hex.trim_start_matches("0x"))?;
        let jsonrpc = jsonrpc_url(&endpoint);
        capture_prestate(&mut client, &jsonrpc, &out_dir, network, id).await?;
        return Ok(());
    }

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

/// Map a gRPC endpoint to the sibling JSON-RPC URL (`eth_getStorageAt`).
fn jsonrpc_url(endpoint: &str) -> String {
    if endpoint.contains("nile") {
        "https://nile.trongrid.io/jsonrpc".to_string()
    } else if endpoint.contains("shasta") {
        "https://api.shasta.trongrid.io/jsonrpc".to_string()
    } else {
        "https://api.trongrid.io/jsonrpc".to_string()
    }
}

/// Capture the N-1 pre-state a TRC20 `transfer` read (source #1: archive node).
///
/// Locates the `_balances` mapping slot empirically — the candidate slot whose sender
/// value DROPPED by `amount` between N-1 and N is the balance slot — then records both
/// touched slots' N-1 values. Requires an ARCHIVE JSON-RPC that honors a historical
/// block number; public TronGrid nodes are latest-only (`eth_getStorageAt` rejects a
/// QUANTITY block), in which case this reports the limitation and writes nothing.
async fn capture_prestate(
    client: &mut Client,
    jsonrpc: &str,
    out_dir: &str,
    network: &str,
    id: Vec<u8>,
) -> anyhow::Result<()> {
    let info: TransactionInfo = client
        .get_transaction_info_by_id(BytesMessage { value: id.clone() })
        .await?
        .into_inner();
    let n = info.block_number;
    anyhow::ensure!(n > 0, "tx has no receipt");
    let tx = client
        .get_transaction_by_id(BytesMessage { value: id.clone() })
        .await?
        .into_inner();

    let raw = tx.raw_data.as_ref().context("tx raw_data")?;
    let any = raw.contract.first().and_then(|c| c.parameter.as_ref()).context("contract")?;
    let trigger = tron_proto::protocol::TriggerSmartContract::decode(any.value.as_slice())?;
    anyhow::ensure!(
        trigger.data.len() >= 68 && trigger.data[..4] == TRANSFER_SELECTOR,
        "not a TRC20 transfer(address,uint256)"
    );
    // owner body (20 bytes, strip 0x41), recipient from calldata, amount.
    let from20 = &trigger.owner_address[trigger.owner_address.len().saturating_sub(20)..];
    let to20 = &trigger.data[16..36];
    let amount = &trigger.data[36..68];
    let contract_hex = format!("0x{}", hex::encode(&info.contract_address[info.contract_address.len().saturating_sub(20)..]));

    let http = reqwest::Client::new();
    // Probe archive support once with an explicit historical block.
    if let Err(e) = get_storage_at(&http, jsonrpc, &contract_hex, &[0u8; 32], Some(n - 1)).await {
        println!(
            "archive pre-state UNAVAILABLE on {jsonrpc}: {e}\n\
             (public TronGrid is latest-only; N-1 needs an archive node or full-chain replay — \
             see T10 notes. Use `--emit-sample` for the committed offline sample.)"
        );
        return Ok(());
    }

    // Find the _balances mapping slot: the one whose sender value dropped by `amount`.
    for slot_idx in 0u64..=12 {
        let from_slot = tron_verify::mapping_slot(from20, slot_idx);
        let pre = get_storage_at(&http, jsonrpc, &contract_hex, &from_slot, Some(n - 1)).await?;
        let post = get_storage_at(&http, jsonrpc, &contract_hex, &from_slot, Some(n)).await?;
        if sub_u256(&pre, &post).as_deref() == Some(amount) {
            let to_slot = tron_verify::mapping_slot(to20, slot_idx);
            let to_pre = get_storage_at(&http, jsonrpc, &contract_hex, &to_slot, Some(n - 1)).await?;
            let prestate = tron_verify::PreState {
                contract: info.contract_address.clone(),
                block_number: n,
                storage: vec![(from_slot, pre), (to_slot, to_pre)],
                accounts: vec![],
                source: format!("archive:{jsonrpc}"),
            };
            let label = format!("{network}-{}", &hex::encode(&id)[..16]);
            std::fs::write(format!("{out_dir}/{label}.prestate.pb"), prestate.encode())?;
            println!("captured pre-state (mapping slot {slot_idx}) -> {out_dir}/{label}.prestate.pb");
            return Ok(());
        }
    }
    anyhow::bail!("could not locate the _balances mapping slot in slots 0..=12");
}

/// `eth_getStorageAt(address, slot, block)`. `block=None` => "latest"; `Some(n)` => the
/// historical block (archive-only). Returns the 32-byte value.
async fn get_storage_at(
    http: &reqwest::Client,
    url: &str,
    address_hex: &str,
    slot: &[u8; 32],
    block: Option<i64>,
) -> anyhow::Result<[u8; 32]> {
    let tag = match block {
        Some(n) => format!("0x{n:x}"),
        None => "latest".to_string(),
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_getStorageAt",
        "params": [address_hex, format!("0x{}", hex::encode(slot)), tag],
    });
    let resp: serde_json::Value = http.post(url).json(&body).send().await?.json().await?;
    if let Some(err) = resp.get("error") {
        anyhow::bail!("{}", err);
    }
    let hexval = resp.get("result").and_then(|v| v.as_str()).context("no result")?;
    let raw = hex::decode(hexval.trim_start_matches("0x"))?;
    let mut out = [0u8; 32];
    let n = raw.len().min(32);
    out[32 - n..].copy_from_slice(&raw[raw.len() - n..]);
    Ok(out)
}

/// `a - b` for two big-endian 32-byte values, or `None` on underflow.
fn sub_u256(a: &[u8; 32], b: &[u8; 32]) -> Option<Vec<u8>> {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = a[i] as i16 - b[i] as i16 - borrow;
        if d < 0 {
            out[i] = (d + 256) as u8;
            borrow = 1;
        } else {
            out[i] = d as u8;
            borrow = 0;
        }
    }
    (borrow == 0).then(|| out.to_vec())
}

/// Write the committed hand-verified pre-state sample for the controlled transfer-like
/// contract (T03): storage slot 1 (sender balance) = 1000, the pre-state a deterministic
/// `transfer(amount)` reads. This lets T11's absolute-parity gate run fully OFFLINE.
fn emit_controlled_sample(out_dir: &str) -> anyhow::Result<()> {
    use tron_types::Address;
    let contract = Address::from_body([0xcc; 20]);
    let mut slot1 = [0u8; 32];
    slot1[31] = 1; // storage slot index 1 (SLOT_FROM in the controlled contract)
    let mut val1000 = [0u8; 32];
    val1000[24..].copy_from_slice(&1000u64.to_be_bytes());

    let prestate = tron_verify::PreState {
        contract: contract.as_bytes().to_vec(),
        block_number: 0,
        storage: vec![(slot1, val1000)],
        accounts: vec![(contract.as_bytes().to_vec(), 0)],
        source: "hand-verified: controlled transfer-like contract (T03); real nile N-1 \
                 unavailable — public JSON-RPC is latest-only, full-chain replay of ~69.6M \
                 blocks infeasible offline"
            .to_string(),
    };
    let path = format!("{out_dir}/controlled-transfer.prestate.pb");
    std::fs::write(&path, prestate.encode())?;
    println!("wrote hand-verified sample pre-state -> {path}");
    Ok(())
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
