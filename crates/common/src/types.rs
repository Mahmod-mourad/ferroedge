use serde::{Deserialize, Serialize};

/// Task submitted by user to control-plane
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub wasm_bytes: Vec<u8>,
    pub function_name: String,
    pub input: Vec<u8>,
    pub timeout_ms: u64,
    /// UUID generated at the API boundary; propagated through all services
    /// for log and span correlation.
    #[serde(default)]
    pub trace_id: String,
}

/// Result returned from edge-node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: String,
    pub node_id: String,
    pub success: bool,
    pub output: Vec<u8>,
    pub execution_time_ms: u64,
    pub error: Option<String>,
}

/// Node registered in control-plane
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    /// gRPC address (e.g. `http://127.0.0.1:50051`)
    pub address: String,
    /// HTTP admin/metrics address (e.g. `http://127.0.0.1:9090`).
    /// Used by the control-plane for cache pre-warming.
    #[serde(default)]
    pub http_address: String,
    pub active_tasks: usize,
    pub healthy: bool,
    /// Maximum concurrent tasks this node accepts (used by least-loaded scheduler).
    #[serde(default = "default_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
}

fn default_max_concurrent_tasks() -> usize {
    10
}
