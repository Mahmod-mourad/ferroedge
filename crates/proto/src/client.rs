use tonic::transport::Channel;

use crate::edge::{
    edge_service_client::EdgeServiceClient, ExecuteTaskRequest, ExecuteTaskResponse, HealthRequest,
    HealthResponse,
};

/// Thin wrapper around the generated tonic client for an edge-node.
pub struct EdgeNodeClient {
    inner: EdgeServiceClient<Channel>,
}

impl EdgeNodeClient {
    /// Connect to an edge-node gRPC server at `addr`
    /// (e.g. `"http://127.0.0.1:50051"`).
    pub async fn connect(addr: &str) -> Result<Self, anyhow::Error> {
        let inner = EdgeServiceClient::connect(addr.to_string()).await?;
        Ok(Self { inner })
    }

    /// Execute a task; returns the raw `tonic::Status` on failure so callers
    /// can distinguish `ResourceExhausted` (node busy) from other errors.
    ///
    /// Accepts `impl tonic::IntoRequest<ExecuteTaskRequest>` so callers can
    /// pass either a plain `ExecuteTaskRequest` or a `tonic::Request` with
    /// metadata already populated (e.g. for OTEL traceparent injection).
    pub async fn execute_task(
        &mut self,
        req: impl tonic::IntoRequest<ExecuteTaskRequest>,
    ) -> Result<ExecuteTaskResponse, tonic::Status> {
        self.inner.execute_task(req).await.map(|r| r.into_inner())
    }

    pub async fn health_check(&mut self) -> Result<HealthResponse, anyhow::Error> {
        Ok(self.inner.health(HealthRequest {}).await?.into_inner())
    }
}
