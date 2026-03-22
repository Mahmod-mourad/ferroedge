#[derive(Debug)]
pub struct NodeState {
    pub node_id: String,
    /// Number of tasks currently executing.
    pub active_tasks: usize,
    pub total_executed: u64,
    pub total_failed: u64,
    /// Sum of execution times for completed tasks (used to compute avg).
    pub total_execution_time_ms: u64,
}

impl NodeState {
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            active_tasks: 0,
            total_executed: 0,
            total_failed: 0,
            total_execution_time_ms: 0,
        }
    }
}
