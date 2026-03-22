//! HTTP admin server for the edge-node (port 9090 by default).
//!
//! Routes:
//!   GET  /metrics       — Prometheus text-format metrics scrape endpoint
//!   GET  /debug/memory  — Live memory usage snapshot (JSON)
//!   POST /cache/warm    — Pre-warm the module cache with provided WASM bytes

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::RwLock};

use wasm_runtime::CompiledModuleCache;

use crate::state::NodeState;

// ─── Shared state for HTTP handlers ──────────────────────────────────────────

#[derive(Clone)]
struct HttpState {
    cache: Arc<CompiledModuleCache>,
    node_state: Arc<RwLock<NodeState>>,
}

// ─── GET /metrics ─────────────────────────────────────────────────────────────

async fn metrics_handler() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&prometheus::gather(), &mut buf)
        .unwrap_or_default();
    String::from_utf8_lossy(&buf).into_owned()
}

// ─── GET /debug/memory ────────────────────────────────────────────────────────

async fn memory_handler(State(state): State<HttpState>) -> impl IntoResponse {
    let node_st = state.node_state.read().await;
    let cache_stats = state.cache.stats().await;

    // active_stores: each active task holds exactly one wasmtime::Store
    let active_stores = node_st.active_tasks;

    // wasm_memory_bytes: conservatively 64 MB per active store (default limit)
    let wasm_memory_bytes: u64 = active_stores as u64 * 64 * 1024 * 1024;

    // cache_memory_estimate_bytes: raw WASM bytes in cache (compiled artifacts
    // are roughly 2-5× larger, but we report source bytes as the baseline)
    let cache_memory_estimate_bytes = cache_stats.total_wasm_bytes;

    // heap_allocated_bytes: RSS from /proc/self/status on Linux; 0 elsewhere
    let heap_allocated_bytes = read_rss_bytes();

    Json(serde_json::json!({
        "heap_allocated_bytes":         heap_allocated_bytes,
        "wasm_memory_bytes":            wasm_memory_bytes,
        "cache_memory_estimate_bytes":  cache_memory_estimate_bytes,
        "active_stores":                active_stores,
    }))
}

fn read_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
            for line in content.lines() {
                if line.starts_with("VmRSS:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<u64>() {
                            return kb * 1024;
                        }
                    }
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

// ─── POST /cache/warm ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WarmCacheRequest {
    name: String,
    version: String,
    wasm_bytes_base64: String,
}

async fn warm_cache_handler(
    State(state): State<HttpState>,
    Json(body): Json<WarmCacheRequest>,
) -> Response {
    let wasm_bytes = match B64.decode(&body.wasm_bytes_base64) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid base64: {e}")})),
            )
                .into_response()
        }
    };

    let key = format!("{}:{}", body.name, body.version);

    match state.cache.get_or_compile(&key, &wasm_bytes).await {
        Ok(_) => {
            let stats = state.cache.stats().await;
            telemetry::metrics::CACHE_SIZE_GAUGE.set(stats.size as f64);
            tracing::info!(key = %key, "cache pre-warmed");
            (
                StatusCode::OK,
                Json(serde_json::json!({"warmed": true, "key": key})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ─── Server spawn ─────────────────────────────────────────────────────────────

/// Spawn the edge-node HTTP admin server on `port`.
///
/// Exposes Prometheus metrics, a memory snapshot endpoint, and a cache
/// pre-warming endpoint that the control-plane calls after module uploads.
pub fn spawn_metrics_server(
    port: u16,
    cache: Arc<CompiledModuleCache>,
    node_state: Arc<RwLock<NodeState>>,
) -> tokio::task::JoinHandle<()> {
    // Sync cache-size gauge once at startup.
    {
        let cache_clone = Arc::clone(&cache);
        tokio::spawn(async move {
            let stats = cache_clone.stats().await;
            telemetry::metrics::CACHE_SIZE_GAUGE.set(stats.size as f64);
        });
    }

    let state = HttpState { cache, node_state };

    tokio::spawn(async move {
        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/debug/memory", get(memory_handler))
            .route("/cache/warm", post(warm_cache_handler))
            .with_state(state);

        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!(address = %addr, "edge-node HTTP admin server listening");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!(error = %e, "HTTP admin server error");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, port, "failed to bind HTTP admin server");
            }
        }
    })
}
