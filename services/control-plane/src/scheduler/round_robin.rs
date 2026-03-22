use std::collections::{HashMap, HashSet};

use common::{errors::EdgeError, types::NodeInfo};

use super::Scheduler;

pub struct RoundRobinScheduler;

impl Scheduler for RoundRobinScheduler {
    fn next_node<'a>(
        &self,
        nodes: &'a mut HashMap<String, NodeInfo>,
        index: &mut usize,
    ) -> Result<NodeInfo, EdgeError> {
        let healthy: Vec<&NodeInfo> = nodes.values().filter(|n| n.healthy).collect();

        if healthy.is_empty() {
            return Err(EdgeError::NodeUnavailable(
                "no healthy nodes registered".to_string(),
            ));
        }

        let selected = healthy[*index % healthy.len()].clone();
        *index = index.wrapping_add(1);
        Ok(selected)
    }
}

/// Stateless helper used by handlers to pick a node without the trait object.
/// Skips unhealthy nodes; advances `index` on every call.
pub fn select_node(nodes: &HashMap<String, NodeInfo>, index: &mut usize) -> Option<NodeInfo> {
    let healthy: Vec<&NodeInfo> = nodes.values().filter(|n| n.healthy).collect();
    if healthy.is_empty() {
        return None;
    }
    let selected = healthy[*index % healthy.len()].clone();
    *index = index.wrapping_add(1);
    Some(selected)
}

/// Like `select_node` but also skips nodes whose IDs are in `exclude`.
///
/// Used during task retry to avoid re-dispatching to nodes that already failed.
pub fn select_node_excluded(
    nodes: &HashMap<String, NodeInfo>,
    index: &mut usize,
    exclude: &HashSet<String>,
) -> Option<NodeInfo> {
    let candidates: Vec<&NodeInfo> = nodes
        .values()
        .filter(|n| n.healthy && !exclude.contains(&n.id))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let selected = candidates[*index % candidates.len()].clone();
    *index = index.wrapping_add(1);
    Some(selected)
}
