//! HTTP server binding (P4): serve the JSON handlers over a real socket (axum).
//!
//! Routes the java-tron FullNode HTTP paths (contracted by the tron-openapi spec)
//! to the pure handlers in [`crate::http`], over a shared read-only [`WorldState`].

use crate::http;
use axum::extract::FromRef;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tron_consensus::mempool::Mempool;
use tron_state::WorldState;
use tron_storage::KvStore;

/// Read-only world-state handle (most handlers use only this).
pub type AppState<S> = Arc<WorldState<S>>;

/// Shared node state: the world plus the pending-transaction mempool. Handlers
/// extract whichever piece they need via [`FromRef`].
pub struct NodeState<S: KvStore> {
    pub world: Arc<WorldState<S>>,
    pub mempool: Arc<Mutex<Mempool>>,
}

impl<S: KvStore> Clone for NodeState<S> {
    fn clone(&self) -> Self {
        Self { world: self.world.clone(), mempool: self.mempool.clone() }
    }
}

impl<S: KvStore> NodeState<S> {
    pub fn new(world: Arc<WorldState<S>>) -> Self {
        Self { world, mempool: Arc::new(Mutex::new(Mempool::default())) }
    }
}

impl<S: KvStore + 'static> FromRef<NodeState<S>> for Arc<WorldState<S>> {
    fn from_ref(st: &NodeState<S>) -> Self {
        st.world.clone()
    }
}

impl<S: KvStore + 'static> FromRef<NodeState<S>> for Arc<Mutex<Mempool>> {
    fn from_ref(st: &NodeState<S>) -> Self {
        st.mempool.clone()
    }
}

/// Build the router. Accepts either a bare world handle or a full [`NodeState`].
pub fn router<S: KvStore + 'static>(world: AppState<S>) -> Router {
    router_with_state(NodeState::new(world))
}

/// Build the router over an explicit [`NodeState`].
pub fn router_with_state<S: KvStore + 'static>(state: NodeState<S>) -> Router {
    Router::new()
        .route("/wallet/getaccount", post(get_account::<S>))
        .route("/wallet/getaccountresource", post(get_account_resource::<S>))
        .route("/wallet/getReward", post(get_reward::<S>))
        .route("/wallet/getBrokerage", post(get_brokerage::<S>))
        .route("/wallet/getenergyprices", post(get_energy_prices::<S>))
        .route("/wallet/getbandwidthprices", post(get_bandwidth_prices::<S>))
        .route("/wallet/getmemofee", post(get_memo_fee::<S>))
        .route("/wallet/listexchanges", post(list_exchanges))
        .route("/wallet/listproposals", post(list_proposals))
        .route("/wallet/getassetissuelist", post(get_asset_issue_list))
        .route("/wallet/getnowblock", post(get_now_block::<S>))
        .route("/wallet/getblockbynum", post(get_block_by_num::<S>))
        .route("/wallet/getblockbylatestnum", post(get_block_by_latest_num::<S>))
        .route("/wallet/getblockbyid", post(get_block_by_id::<S>))
        .route("/wallet/gettransactioncountbyblocknum", post(get_tx_count_by_block::<S>))
        .route("/wallet/getblockbylimitnext", post(get_block_by_limit_next::<S>))
        .route("/wallet/getcontract", post(get_contract::<S>))
        .route("/wallet/gettransactionbyid", post(get_transaction_by_id::<S>))
        .route("/wallet/getchainparameters", post(get_chain_parameters::<S>))
        .route("/wallet/getnodeinfo", post(get_node_info))
        .route("/wallet/listnodes", post(list_nodes))
        .route("/wallet/validateaddress", post(validate_address))
        .route("/wallet/getburntrx", post(get_burn_trx::<S>))
        .route("/wallet/getnextmaintenancetime", post(get_next_maintenance_time::<S>))
        .route("/wallet/totaltransaction", post(total_transaction::<S>))
        .route("/wallet/broadcasthex", post(broadcast_hex))
        .with_state(state)
}

async fn get_account<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_account(&state, &req))
}

async fn list_exchanges() -> Json<Value> { Json(http::list_exchanges()) }
async fn list_proposals() -> Json<Value> { Json(http::list_proposals()) }
async fn get_asset_issue_list() -> Json<Value> { Json(http::get_asset_issue_list()) }

async fn get_energy_prices<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_energy_prices(&state))
}

async fn get_bandwidth_prices<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_bandwidth_prices(&state))
}

async fn get_memo_fee<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_memo_fee(&state))
}

async fn get_reward<S: KvStore>(State(state): State<AppState<S>>, Json(req): Json<Value>) -> Json<Value> {
    Json(http::get_reward(&state, &req))
}

async fn get_brokerage<S: KvStore>(State(state): State<AppState<S>>, Json(req): Json<Value>) -> Json<Value> {
    Json(http::get_brokerage(&state, &req))
}

async fn get_account_resource<S: KvStore>(State(state): State<AppState<S>>, Json(req): Json<Value>) -> Json<Value> {
    Json(http::get_account_resource(&state, &req))
}

async fn get_now_block<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_now_block(&state))
}

async fn get_block_by_num<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_block_by_num(&state, &req))
}

async fn get_block_by_latest_num<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_block_by_latest_num(&state, &req))
}

async fn get_tx_count_by_block<S: KvStore>(State(state): State<AppState<S>>, Json(req): Json<Value>) -> Json<Value> {
    Json(http::get_transaction_count_by_block_num(&state, &req))
}

async fn get_block_by_limit_next<S: KvStore>(State(state): State<AppState<S>>, Json(req): Json<Value>) -> Json<Value> {
    Json(http::get_block_by_limit_next(&state, &req))
}

async fn get_block_by_id<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_block_by_id(&state, &req))
}

async fn get_contract<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_contract(&state, &req))
}

async fn get_transaction_by_id<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_transaction_by_id(&state, &req))
}

async fn get_chain_parameters<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_chain_parameters(&state))
}

async fn get_node_info<S: KvStore>(State(_state): State<AppState<S>>) -> Json<Value> {
    // Config context isn't threaded into the router yet; report defaults.
    Json(http::get_node_info("nile", tron_p2p_port()))
}

fn tron_p2p_port() -> u16 {
    18888
}

async fn list_nodes<S: KvStore>(State(_state): State<AppState<S>>) -> Json<Value> {
    Json(http::list_nodes())
}

async fn validate_address(Json(req): Json<Value>) -> Json<Value> {
    Json(http::validate_address(&req))
}

async fn get_burn_trx<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_burn_trx(&state))
}

async fn get_next_maintenance_time<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::get_next_maintenance_time(&state))
}

async fn total_transaction<S: KvStore>(State(state): State<AppState<S>>) -> Json<Value> {
    Json(http::total_transaction(&state))
}

async fn broadcast_hex(
    State(mempool): State<Arc<Mutex<Mempool>>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    let result = http::broadcast_hex(&req);
    // On a structurally-valid tx, admit it to the mempool.
    if result.get("result").and_then(Value::as_bool) == Some(true) {
        if let Some(hex_str) = req.get("transaction").and_then(Value::as_str) {
            if let Ok(bytes) = hex::decode(hex_str.trim_start_matches("0x")) {
                use prost::Message;
                if let Ok(tx) = tron_proto::protocol::Transaction::decode(bytes.as_slice()) {
                    if let Ok(mut pool) = mempool.lock() {
                        pool.add(tx);
                    }
                }
            }
        }
    }
    Json(result)
}

/// Serve on `addr` until the process ends (blocks). Used by the node binary.
pub async fn serve<S: KvStore + 'static>(
    addr: std::net::SocketAddr,
    state: AppState<S>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await
}
