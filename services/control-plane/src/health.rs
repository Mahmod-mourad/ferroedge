//! Background health-check task for registered edge-nodes.
//!
//! Rules:
//! - Each node is probed via `EdgeService::Health` with a **2-second timeout**.
//! - Success  → mark healthy, refresh `active_tasks` count.
//! - Timeout / error → mark unhealthy; increment `NODE_HEALTH_CHECK_FAILURES` counter.
//! - The `RwLock` is **never** held across an `await` point.

use std::{sync::Arc, time::Duration};

use tokio::{sync::RwLock, time};
use tracing::{info, warn};

use proto::client::EdgeNodeClient;

use crate::state::AppState;

/// Perform one health-check round over every registered node.
pub async fn check_all_nodes(state: Arc<RwLock<AppState>>) {
    // Snapshot (id, address) pairs — release read lock before any I/O.
    let nodes: Vec<(String, String)> = {
        let st = state.read().await;
        st.nodes
            .values()
            .map(|n| (n.id.clone(), n.address.clone()))
            .collect()
    };

    let mut healthy_count = 0usize;

    for (node_id, address) in &nodes {
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let mut client = EdgeNodeClient::connect(address).await?;
            client.health_check().await
        })
        .await;

        match result {
            Ok(Ok(resp)) => {
                {
                    let mut st = state.write().await;
                    if let Some(node) = st.nodes.get_mut(node_id) {
                        node.healthy = true;
                        node.active_tasks = resp.active_tasks as usize;
                    }
                }
                healthy_count += 1;
                info!(
                    node_id      = %node_id,
                    active_tasks = resp.active_tasks,
                    "health-check OK"
                );
            }
            Ok(Err(e)) => {
                {
                    let mut st = state.write().await;
                    if let Some(node) = st.nodes.get_mut(node_id) {
                        node.healthy = false;
                    }
                }
                telemetry::metrics::NODE_HEALTH_CHECK_FAILURES
                    .with_label_values(&[node_id])
                    .inc();
                warn!(node_id = %node_id, error = %e, "health-check FAILED");
            }
            Err(_elapsed) => {
                {
                    let mut st = state.write().await;
                    if let Some(node) = st.nodes.get_mut(node_id) {
                        node.healthy = false;
                    }
                }
                telemetry::metrics::NODE_HEALTH_CHECK_FAILURES
                    .with_label_values(&[node_id])
                    .inc();
                warn!(node_id = %node_id, "health-check TIMEOUT (>2s)");
            }
        }
    }

    // Update the healthy-nodes gauge after each full round.
    telemetry::metrics::HEALTHY_NODES_GAUGE.set(healthy_count as f64);
}

/// Long-running background task: calls `check_all_nodes` every `interval`.
pub async fn health_check_loop(state: Arc<RwLock<AppState>>, interval: Duration) {
    let mut ticker = time::interval(interval);
    loop {
        ticker.tick().await;
        check_all_nodes(Arc::clone(&state)).await;
    }
}
