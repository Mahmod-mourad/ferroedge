use std::collections::HashMap;
use std::sync::Arc;

use common::types::{NodeInfo, TaskResult};
use tokio::sync::Mutex;

use crate::circuit_breaker::CircuitBreaker;
use crate::connection_pool::NodeConnectionPool;
use crate::registry::ModuleRegistry;

#[derive(Debug)]
pub struct AppState {
    /// Registered edge-nodes keyed by node ID.
    pub nodes: HashMap<String, NodeInfo>,
    /// Completed task results keyed by task ID.
    pub tasks: HashMap<String, TaskResult>,
    /// Cursor for the round-robin scheduler.
    pub scheduler_index: usize,
    /// Registered WASM modules served to edge-nodes.
    pub module_registry: ModuleRegistry,
    /// Public base URL of this control-plane (used to construct ModuleRef fetch_url).
    pub self_url: String,

    // ── Fault-tolerance ───────────────────────────────────────────────────────
    /// Per-node circuit breakers.  Keyed by node ID.
    pub circuit_breakers: HashMap<String, CircuitBreaker>,
    /// Active scheduling strategy: `"round_robin"` or `"least_loaded"`.
    pub scheduling_strategy: String,

    // ── Connection pool (shared Arc so handlers can clone it without write lock) ─
    /// Reusable gRPC channels to edge-nodes, keyed by node ID.
    pub connection_pool: Arc<Mutex<NodeConnectionPool>>,

    // ── Metrics counters ──────────────────────────────────────────────────────
    pub total_submitted: u64,
    pub total_success: u64,
    pub total_failed: u64,
    /// Tasks that were retried at least once.
    pub total_retried: u64,
    /// Tasks currently queued waiting for node capacity.
    pub queued: usize,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            tasks: HashMap::new(),
            scheduler_index: 0,
            module_registry: ModuleRegistry::default(),
            self_url: String::new(),
            circuit_breakers: HashMap::new(),
            scheduling_strategy: "round_robin".to_string(),
            connection_pool: Arc::new(Mutex::new(NodeConnectionPool::new())),
            total_submitted: 0,
            total_success: 0,
            total_failed: 0,
            total_retried: 0,
            queued: 0,
        }
    }
}
