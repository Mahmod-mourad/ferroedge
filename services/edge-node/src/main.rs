use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use tokio::sync::{RwLock, Semaphore};
use tonic::transport::Server;
use tracing::{error, info};

use proto::edge::edge_service_server::EdgeServiceServer;
use wasm_runtime::{CompiledModuleCache, WasmEngine, WasmSandbox};

use edge_node::{
    config::Config,
    downloader::ModuleDownloader,
    metrics::spawn_metrics_server,
    server::EdgeGrpcServer,
    state::NodeState,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::from_env()?;

    let mode = telemetry::TracingMode::from_env();
    telemetry::init_tracing("edge-node", mode);
    telemetry::metrics::init();

    info!(
        node_id = %config.node_id,
        grpc_port = config.grpc_port,
        metrics_port = config.metrics_port,
        control_plane = %config.control_plane_url,
        cache_max_size = config.cache_max_size,
        cache_ttl_secs = config.cache_ttl_secs,
        "edge-node starting"
    );

    // ── WASM engine + sandbox ─────────────────────────────────────────────────
    let engine = Arc::new(WasmEngine::new()?);
    let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));

    // ── Compiled module cache ─────────────────────────────────────────────────
    let module_cache = Arc::new(CompiledModuleCache::new(
        Arc::clone(&engine),
        config.cache_max_size,
        config.cache_ttl_secs,
    ));

    // ── Module downloader ─────────────────────────────────────────────────────
    let downloader = Arc::new(ModuleDownloader::new(&config.control_plane_url));

    // ── Shared state ──────────────────────────────────────────────────────────
    let shared_state = Arc::new(RwLock::new(NodeState::new(&config.node_id)));

    // ── Concurrency limiter ───────────────────────────────────────────────────
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_tasks));

    // ── Metrics HTTP server (port 9090) ───────────────────────────────────────
    let _metrics_handle = spawn_metrics_server(
        config.metrics_port,
        Arc::clone(&module_cache),
        Arc::clone(&shared_state),
    );

    // ── gRPC service ──────────────────────────────────────────────────────────
    let grpc_svc = EdgeGrpcServer {
        node_id: config.node_id.clone(),
        sandbox,
        module_cache,
        downloader,
        state: Arc::clone(&shared_state),
        semaphore,
    };

    // ── Register with control-plane (best-effort, non-blocking) ──────────────
    let registration_url = format!("{}/nodes/register", config.control_plane_url);
    let host = local_hostname();
    let own_grpc_addr = format!("http://{}:{}", host, config.grpc_port);
    let own_http_addr = format!("http://{}:{}", host, config.metrics_port);
    let node_id_clone = config.node_id.clone();
    tokio::spawn(async move {
        register_with_control_plane(
            &registration_url,
            &node_id_clone,
            &own_grpc_addr,
            &own_http_addr,
        )
        .await;
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
    info!(address = %addr, "gRPC server listening");

    Server::builder()
        .add_service(EdgeServiceServer::new(grpc_svc))
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    Ok(())
}

async fn register_with_control_plane(
    url: &str,
    node_id: &str,
    address: &str,
    http_address: &str,
) {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "node_id":      node_id,
        "address":      address,
        "http_address": http_address,
    });

    match client.post(url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(node_id = %node_id, "registered with control-plane");
        }
        Ok(resp) => {
            error!(status = %resp.status(), "control-plane rejected registration");
        }
        Err(e) => {
            error!(error = %e, "failed to reach control-plane for registration");
        }
    }
}

fn local_hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "127.0.0.1".to_string())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }

    info!("shutdown signal received");
}
