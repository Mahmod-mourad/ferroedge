//! Least-loaded scheduling strategy.
//!
//! Selects the healthy node with the fewest active tasks that still has
//! remaining capacity.  When multiple nodes tie on `active_tasks`, one is
//! chosen pseudo-randomly using the current wall-clock sub-second nanos as
//! a seed (no external `rand` dependency required).

use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use common::types::NodeInfo;

/// Pick the least-loaded node, subject to the following filters:
///
/// - `node.healthy == true`
/// - `node.active_tasks < node.max_concurrent_tasks` (node still has headroom)
/// - `node.id` not in `exclude` (already tried or circuit-breaker open)
///
/// Returns `None` when all candidate nodes are at capacity or excluded.
pub fn select_least_loaded<'a>(
    nodes: &'a HashMap<String, NodeInfo>,
    exclude: &HashSet<String>,
) -> Option<&'a NodeInfo> {
    let available: Vec<&NodeInfo> = nodes
        .values()
        .filter(|n| {
            n.healthy && n.active_tasks < n.max_concurrent_tasks && !exclude.contains(&n.id)
        })
        .collect();

    if available.is_empty() {
        return None;
    }

    let min_tasks = available.iter().map(|n| n.active_tasks).min()?;

    let tied: Vec<&NodeInfo> = available
        .into_iter()
        .filter(|n| n.active_tasks == min_tasks)
        .collect();

    if tied.len() == 1 {
        Some(tied[0])
    } else {
        // Tie-break: use sub-second wall-clock nanos as a cheap pseudo-random seed.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as usize;
        Some(tied[seed % tied.len()])
    }
}
