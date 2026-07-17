//! Minimal localhost-only HTTP API for the miner sidecar.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{Error, MinerSidecar};

/// Shared router state.
#[derive(Clone)]
pub struct AppState {
    pub sidecar: MinerSidecar,
}

/// Optional router knobs (reserved for future use).
#[derive(Clone, Debug, Default)]
pub struct RouterConfig;

/// Default bind: loopback only.
#[must_use]
pub fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Whether `ip` is loopback (IPv4 127.0.0.0/8 or IPv6 ::1).
#[must_use]
pub fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Build the axum router. Always intended for 127.0.0.1 listeners.
pub fn router(state: AppState, _config: RouterConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tx", post(post_tx).get(get_inventory))
        .route("/inventory", get(get_inventory).delete(delete_inventory))
        .route("/mine", post(post_mine))
        .route("/submit", post(post_mine))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct TxBody {
    hex: String,
}

#[derive(Debug, Serialize)]
struct TxResponse {
    txid: String,
}

#[derive(Debug, Serialize)]
struct InventoryResponse {
    txids: Vec<String>,
    count: usize,
}

#[derive(Debug, Serialize)]
struct MineResponse {
    block_hash: String,
    height: u32,
    tx_count: usize,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn post_tx(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let hex = match parse_tx_body(&body) {
        Ok(h) => h,
        Err(msg) => {
            return (StatusCode::BAD_REQUEST, Json(ErrorBody { error: msg })).into_response();
        }
    };
    match state.sidecar.add_tx_hex(&hex) {
        Ok(txid) => (
            StatusCode::OK,
            Json(TxResponse {
                txid: txid.to_string(),
            }),
        )
            .into_response(),
        Err(err) => error_response(err),
    }
}

fn parse_tx_body(body: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| "body is not utf-8".to_owned())?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty body".to_owned());
    }
    // Prefer JSON `{"hex":"..."}`; fall back to raw hex.
    if trimmed.starts_with('{') {
        let parsed: TxBody =
            serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON body: {e}"))?;
        if parsed.hex.trim().is_empty() {
            return Err("missing hex field".to_owned());
        }
        Ok(parsed.hex)
    } else {
        Ok(trimmed.to_owned())
    }
}

async fn get_inventory(State(state): State<AppState>) -> impl IntoResponse {
    let txids = state.sidecar.list_txids();
    let count = txids.len();
    Json(InventoryResponse {
        txids: txids.into_iter().map(|t| t.to_string()).collect(),
        count,
    })
}

async fn delete_inventory(State(state): State<AppState>) -> impl IntoResponse {
    state.sidecar.clear();
    StatusCode::NO_CONTENT
}

async fn post_mine(State(state): State<AppState>) -> Response {
    match state.sidecar.mine().await {
        Ok(result) => (
            StatusCode::OK,
            Json(MineResponse {
                block_hash: result.block_hash.to_string(),
                height: result.height,
                tx_count: result.tx_count,
            }),
        )
            .into_response(),
        Err(err) => error_response(err),
    }
}

fn error_response(err: Error) -> Response {
    let status = match &err {
        // Client / input
        e if e.is_client_error() => StatusCode::BAD_REQUEST,
        // Block not accepted by Core
        Error::BlockRejected(_) => StatusCode::CONFLICT,
        // Assembly / witness / coinbase template issues → treat as 500 (local bug / node state)
        Error::MissingCoinbaseValue
        | Error::WitnessRoot
        | Error::TxMerkleRoot
        | Error::Rpc { .. }
        | Error::CreateClient(_)
        | Error::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        // Fallback (should be covered above)
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorBody {
            error: err.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn client_errors_map_to_400_class() {
        assert!(Error::EmptyInventory.is_client_error());
        assert!(Error::InvalidTxHex("x".into()).is_client_error());
        use bitcoin::hashes::Hash as _;
        assert!(Error::DuplicateTxid(bitcoin::Txid::all_zeros()).is_client_error());
        assert!(Error::InventoryTxCap { have: 1, max: 1 }.is_client_error());
        assert!(!Error::WitnessRoot.is_client_error());
        assert!(!Error::BlockRejected("x".into()).is_client_error());
    }

    #[test]
    fn bind_addr_is_loopback() {
        let a = bind_addr(18450);
        assert!(is_loopback_ip(a.ip()));
        assert_eq!(a.port(), 18450);
    }
}
