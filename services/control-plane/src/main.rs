use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{net::TcpListener, sync::RwLock};
use tracing::info;

use control_plane::{
    api::build_router, config::Config, health::health_check_loop, state::AppState,
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let config = Config::from_env()?;

    let mode = telemetry::TracingMode::from_env();
    telemetry::init_tracing("control-plane", mode);
    telemetry::metrics::init();

    info!(
        http_port    = config.http_port,
        metrics_port = config.metrics_port,
        strategy     = %config.scheduling_strategy,
        "control-plane starting"
    );

    let shared_state = Arc::new(RwLock::new(AppState {
        self_url: config.self_url.clone(),
        scheduling_strategy: config.scheduling_strategy.clone(),
        ..AppState::default()
    }));

    // ── Prometheus metrics server (port 9090, scraped by Prometheus) ──────────
    let _prometheus = telemetry::spawn_prometheus_server(config.metrics_port);

    // ── Health-check background task (non-blocking, runs every 10 s) ─────────
    {
        let state_for_hc = Arc::clone(&shared_state);
        tokio::spawn(async move {
            health_check_loop(state_for_hc, Duration::from_secs(10)).await;
        });
    }

    let router = build_router(Arc::clone(&shared_state));

    let addr = SocketAddr::from(([0, 0, 0, 0], config.http_port));
    let listener = TcpListener::bind(addr).await?;
    info!(address = %addr, "listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
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
