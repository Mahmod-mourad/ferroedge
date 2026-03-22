pub mod least_loaded;
pub mod round_robin;

#[allow(unused_imports)]
pub use round_robin::RoundRobinScheduler;

use std::collections::HashMap;

use common::{errors::EdgeError, types::NodeInfo};

#[allow(dead_code)]
pub trait Scheduler: Send + Sync {
    /// Select the next healthy node. Returns a clone of the selected NodeInfo.
    fn next_node(
        &self,
        nodes: &mut HashMap<String, NodeInfo>,
        index: &mut usize,
    ) -> Result<NodeInfo, EdgeError>;
}
