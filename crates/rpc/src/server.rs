//! HTTP server binding (P4): serve the JSON handlers over a real socket (axum).
//!
//! Routes the java-tron FullNode HTTP paths (contracted by the tron-openapi spec)
//! to the pure handlers in [`crate::http`], over a shared read-only [`WorldState`].

use crate::http;
use axum::{extract::State, routing::post, Json, Router};
use serde_json::Value;
use std::sync::Arc;
use tron_state::WorldState;
use tron_storage::KvStore;

/// Shared, read-only application state.
pub type AppState<S> = Arc<WorldState<S>>;

/// Build the router serving the currently-implemented endpoints.
pub fn router<S: KvStore + 'static>(state: AppState<S>) -> Router {
    Router::new()
        .route("/wallet/getaccount", post(get_account::<S>))
        .route("/wallet/getnowblock", post(get_now_block::<S>))
        .route("/wallet/getblockbynum", post(get_block_by_num::<S>))
        .with_state(state)
}

async fn get_account<S: KvStore>(
    State(state): State<AppState<S>>,
    Json(req): Json<Value>,
) -> Json<Value> {
    Json(http::get_account(&state, &req))
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

/// Serve on `addr` until the process ends (blocks). Used by the node binary.
pub async fn serve<S: KvStore + 'static>(
    addr: std::net::SocketAddr,
    state: AppState<S>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(state)).await
}
