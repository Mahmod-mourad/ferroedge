use std::{sync::Arc, time::Instant};

use axum::{
    extract::Request,
    http::HeaderValue,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::state::AppState;

use super::handlers;

// ── TraceId extension ──────────────────────────────────────────────────────

/// Extracted or generated trace ID, stored in Axum request extensions so that
/// every handler can pull it without an extra extractor parameter.
#[derive(Clone, Debug)]
pub struct TraceId(pub String);

// ── Middleware: trace-ID propagation ───────────────────────────────────────

/// Extract `X-Trace-Id` from the request header (or generate a fresh UUID),
/// attach it to request extensions, create a tracing span so all child spans
/// inherit the field, and echo it back on the response.
pub async fn trace_id_middleware(mut req: Request, next: Next) -> Response {
    let trace_id = req
        .headers()
        .get("X-Trace-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    req.extensions_mut().insert(TraceId(trace_id.clone()));

    // The span is entered here so every child span (handlers, gRPC calls, etc.)
    // inherits the trace_id field in structured logs.
    let span = tracing::info_span!("request", trace_id = %trace_id);
    let _guard = span.enter();

    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let mut response = next.run(req).await;

    let duration_ms = start.elapsed().as_millis();
    let status = response.status().as_u16();

    tracing::info!(
        event       = "http_request",
        method      = %method,
        path        = %path,
        status,
        duration_ms,
        trace_id    = %trace_id,
        "request completed"
    );

    if let Ok(val) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert("X-Trace-Id", val);
    }

    response
}

// ── Router ─────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<RwLock<AppState>>) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/metrics", get(handlers::metrics))
        // ── task endpoints ────────────────────────────────────────────────────
        .route("/tasks", post(handlers::submit_task))
        .route("/tasks/batch", post(handlers::submit_tasks_batch))
        .route("/tasks/:id", get(handlers::get_task))
        // ── node management ───────────────────────────────────────────────────
        .route("/nodes/register", post(handlers::register_node))
        .route("/nodes", get(handlers::list_nodes))
        // ── module registry ───────────────────────────────────────────────────
        .route("/modules", post(handlers::upload_module))
        .route("/modules", get(handlers::list_modules))
        .route("/modules/:name/:version", get(handlers::get_module_bytes))
        .layer(middleware::from_fn(trace_id_middleware))
        .with_state(state)
}
