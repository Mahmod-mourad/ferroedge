//! gRPC connection pool for edge-node clients.
//!
//! # Problem
//! Each `EdgeNodeClient::connect(addr).await` dials a new TCP connection and
//! negotiates TLS/HTTP-2 from scratch.  Under load this per-request dial
//! becomes a measurable bottleneck.
//!
//! # Solution
//! `NodeConnectionPool` stores one `tonic::transport::Channel` per node.
//! A `Channel` wraps a lazily-connected, multiplexed HTTP-2 transport that
//! can serve many concurrent RPCs.  Cloning a `Channel` is O(1) (ref-count
//! bump) and shares the underlying transport, so we pay the connection cost
//! at most once per node across the entire lifetime of the process.

use std::collections::HashMap;

use proto::edge::edge_service_client::EdgeServiceClient;
use tonic::transport::Channel;

/// Connection pool: one persistent `Channel` per edge-node.
///
/// Wrap in `Arc<tokio::sync::Mutex<_>>` for shared, async-safe access.
pub struct NodeConnectionPool {
    connections: HashMap<String, Channel>,
}

impl NodeConnectionPool {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Return an `EdgeServiceClient` for `node_id`, reusing an existing
    /// channel if one is already open, or creating a lazy one otherwise.
    ///
    /// `connect_lazy()` records the endpoint without performing any I/O;
    /// the first actual RPC triggers the connection.  This keeps the method
    /// non-async and means the pool lock is held for only a hashmap lookup.
    #[allow(clippy::result_large_err)]
    pub fn get_or_create(
        &mut self,
        node_id: &str,
        addr: &str,
    ) -> Result<EdgeServiceClient<Channel>, tonic::Status> {
        let channel = if let Some(ch) = self.connections.get(node_id) {
            ch.clone()
        } else {
            let ch = Channel::from_shared(addr.to_string())
                .map_err(|e| tonic::Status::internal(format!("invalid address '{addr}': {e}")))?
                .connect_lazy();
            self.connections.insert(node_id.to_string(), ch.clone());
            ch
        };
        Ok(EdgeServiceClient::new(channel))
    }

    /// Remove the channel for a node that has been deregistered or found
    /// permanently unhealthy.  The next call to `get_or_create` will open a
    /// fresh connection.
    pub fn evict(&mut self, node_id: &str) {
        self.connections.remove(node_id);
    }
}

impl Default for NodeConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NodeConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeConnectionPool")
            .field("node_count", &self.connections.len())
            .finish()
    }
}
