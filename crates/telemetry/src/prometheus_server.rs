//! Shared Prometheus metrics HTTP server.
//!
//! Spawns a background Axum server on `port` that serves the default
//! Prometheus registry at `GET /metrics` in the standard text format.
//! Both control-plane and edge-node call this at startup.

use axum::{routing::get, Router};
use tokio::net::TcpListener;

async fn metrics_handler() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&prometheus::gather(), &mut buf)
        .unwrap_or_default();
    String::from_utf8_lossy(&buf).into_owned()
}

/// Spawn the Prometheus metrics server on `port` (typically 9090).
///
/// Returns the [`tokio::task::JoinHandle`]; the server runs until the process
/// exits.  Bind errors are logged but **never** panic.
pub fn spawn_prometheus_server(port: u16) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let app = Router::new().route("/metrics", get(metrics_handler));
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!(address = %addr, "Prometheus metrics server listening");
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!(error = %e, "Prometheus metrics server error");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, port, "failed to bind Prometheus metrics server");
            }
        }
    })
}
