use std::{sync::Arc, time::Instant};

use tokio::sync::{RwLock, Semaphore};
use tonic::{Request, Response, Status};
use tracing::{error, info};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use proto::edge::{
    edge_service_server::EdgeService, execute_task_request::WasmSource, ExecuteTaskRequest,
    ExecuteTaskResponse, HealthRequest, HealthResponse,
};
use wasm_runtime::{CompiledModuleCache, ExecutionConfig, WasmSandbox};

use crate::{downloader::ModuleDownloader, state::NodeState};

// ── W3C TraceContext carrier for tonic gRPC metadata ─────────────────────

/// Extract the W3C `traceparent` (and `tracestate`) headers from incoming
/// tonic metadata so this service can continue the distributed trace.
struct MetadataExtractor<'a>(&'a tonic::metadata::MetadataMap);

impl<'a> opentelemetry::propagation::Extractor for MetadataExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        // The W3C propagator only calls get() for specific keys; keys() is used
        // for baggage which we don't implement.
        vec![]
    }
}

// ── gRPC server ────────────────────────────────────────────────────────────

pub struct EdgeGrpcServer {
    pub node_id: String,
    pub sandbox: Arc<WasmSandbox>,
    /// Cache of compiled wasmtime::Module objects, keyed by "name:version".
    pub module_cache: Arc<CompiledModuleCache>,
    /// Downloads WASM bytes from the control-plane on a cache miss.
    pub downloader: Arc<ModuleDownloader>,
    pub state: Arc<RwLock<NodeState>>,
    /// Limits the number of simultaneously executing WASM tasks.
    pub semaphore: Arc<Semaphore>,
}

#[tonic::async_trait]
impl EdgeService for EdgeGrpcServer {
    async fn execute_task(
        &self,
        request: Request<ExecuteTaskRequest>,
    ) -> Result<Response<ExecuteTaskResponse>, Status> {
        // ── Extract the distributed trace context from tonic metadata ──────────
        let parent_cx = opentelemetry::global::get_text_map_propagator(|p| {
            p.extract(&MetadataExtractor(request.metadata()))
        });

        let req      = request.into_inner();
        let task_id  = req.task_id.clone();
        let trace_id = req.trace_id.clone();
        let node_id  = self.node_id.clone();

        // ── Root span for this RPC — child of the control-plane span ──────────
        let span = tracing::info_span!(
            "grpc.execute_task",
            task_id  = %task_id,
            trace_id = %trace_id,
            node_id  = %node_id,
        );
        span.set_parent(parent_cx);
        let _enter = span.enter();

        // ── 1. Acquire semaphore permit (non-blocking) ─────────────────────────
        let _permit = self.semaphore.try_acquire().map_err(|_| {
            Status::resource_exhausted("node is at maximum concurrent task capacity")
        })?;

        // ── 2. Increment active_tasks + gauge ──────────────────────────────────
        self.state.write().await.active_tasks += 1;
        telemetry::metrics::ACTIVE_TASKS_GAUGE
            .with_label_values(&[&node_id])
            .inc();

        let config = ExecutionConfig {
            memory_limit_bytes: req.memory_limit_mb.max(1) * 1024 * 1024,
            timeout_ms:         if req.timeout_ms == 0 { 5_000 } else { req.timeout_ms },
            function_name:      req.function_name.clone(),
        };

        let exec_start = Instant::now();

        // ── 3. Resolve WASM source ─────────────────────────────────────────────
        let result = match req.wasm_source {
            // ── 3a. Direct bytes ─────────────────────────────────────────────
            Some(WasmSource::WasmBytes(bytes)) => {
                let _wasm_span = tracing::info_span!(
                    "wasm.compile",
                    task_id  = %task_id,
                    trace_id = %trace_id,
                );
                let _we = _wasm_span.enter();
                telemetry::metrics::WASM_CACHE_MISSES_TOTAL.inc();
                self.sandbox.execute(&bytes, config, &req.input).await
            }

            // ── 3b. Module reference (cached module mode) ──────────────────
            Some(WasmSource::ModuleRef(module_ref)) => {
                let key = format!("{}:{}", module_ref.name, module_ref.version);

                // Cache lookup span
                let module = {
                    let _cache_span = tracing::info_span!(
                        "cache.lookup",
                        key      = %key,
                        task_id  = %task_id,
                        trace_id = %trace_id,
                    );
                    let _ce = _cache_span.enter();

                    if let Some(cached) = self.module_cache.get_cached(&key).await {
                        telemetry::metrics::WASM_CACHE_HITS_TOTAL.inc();
                        info!(
                            key      = %key,
                            task_id  = %task_id,
                            trace_id = %trace_id,
                            "module cache hit"
                        );
                        cached
                    } else {
                        telemetry::metrics::WASM_CACHE_MISSES_TOTAL.inc();
                        info!(
                            key       = %key,
                            fetch_url = %module_ref.fetch_url,
                            task_id   = %task_id,
                            trace_id  = %trace_id,
                            "module cache miss — downloading"
                        );

                        let (name, version) = (&module_ref.name, &module_ref.version);
                        let wasm = self.downloader.download(name, version).await.map_err(|e| {
                            Status::internal(format!("module download failed: {e}"))
                        })?;

                        if !module_ref.hash.is_empty()
                            && !ModuleDownloader::verify_hash(&wasm, &module_ref.hash)
                        {
                            error!(
                                key      = %key,
                                task_id  = %task_id,
                                trace_id = %trace_id,
                                "hash mismatch after download"
                            );
                            let mut st = self.state.write().await;
                            st.active_tasks = st.active_tasks.saturating_sub(1);
                            st.total_failed += 1;
                            telemetry::metrics::ACTIVE_TASKS_GAUGE
                                .with_label_values(&[&node_id])
                                .dec();
                            return Err(Status::data_loss(
                                "module hash mismatch — possible tampering",
                            ));
                        }

                        // Compile span (only on cache miss)
                        let _compile_span = tracing::info_span!(
                            "wasm.compile",
                            key      = %key,
                            task_id  = %task_id,
                            trace_id = %trace_id,
                        );
                        let _cpe = _compile_span.enter();

                        let m = self
                            .module_cache
                            .get_or_compile(&key, &wasm)
                            .await
                            .map_err(|e| Status::internal(format!("compilation failed: {e}")))?;

                        // Update cache-size gauge.
                        let stats = self.module_cache.stats().await;
                        telemetry::metrics::CACHE_SIZE_GAUGE.set(stats.size as f64);

                        m
                    }
                };

                // WASM execution span
                let _exec_span = tracing::info_span!(
                    "wasm.execute",
                    task_id  = %task_id,
                    trace_id = %trace_id,
                );
                let _ee = _exec_span.enter();
                self.sandbox.execute_compiled(module, config, &req.input).await
            }

            None => {
                let mut st = self.state.write().await;
                st.active_tasks = st.active_tasks.saturating_sub(1);
                st.total_failed += 1;
                telemetry::metrics::ACTIVE_TASKS_GAUGE
                    .with_label_values(&[&node_id])
                    .dec();
                return Err(Status::invalid_argument(
                    "wasm_source must be set (wasm_bytes or module_ref)",
                ));
            }
        };

        // ── 4. Update state counters + Prometheus metrics ──────────────────────
        let exec_ms = exec_start.elapsed().as_millis() as f64;
        {
            let mut st = self.state.write().await;
            st.active_tasks = st.active_tasks.saturating_sub(1);
            match &result {
                Ok(r) => {
                    st.total_executed += 1;
                    st.total_execution_time_ms += r.execution_time_ms;
                    telemetry::metrics::TASK_EXECUTION_DURATION_MS
                        .observe(r.execution_time_ms as f64);
                }
                Err(_) => st.total_failed += 1,
            }
        }
        telemetry::metrics::ACTIVE_TASKS_GAUGE
            .with_label_values(&[&node_id])
            .dec();

        // ── 5. Map result → response ───────────────────────────────────────────
        let response = match result {
            Ok(exec) => {
                info!(
                    task_id          = %task_id,
                    trace_id         = %trace_id,
                    node_id          = %node_id,
                    execution_time_ms = exec.execution_time_ms,
                    duration_ms       = exec_ms as u64,
                    "task succeeded"
                );
                ExecuteTaskResponse {
                    task_id,
                    success:           true,
                    output:            exec.output,
                    execution_time_ms: exec.execution_time_ms,
                    error:             String::new(),
                }
            }
            Err(e) => {
                error!(
                    task_id  = %task_id,
                    trace_id = %trace_id,
                    node_id  = %node_id,
                    error    = %e,
                    "task failed"
                );
                ExecuteTaskResponse {
                    task_id,
                    success:           false,
                    output:            Vec::new(),
                    execution_time_ms: 0,
                    error:             e.to_string(),
                }
            }
        };

        Ok(Response::new(response))
    }

    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let st = self.state.read().await;
        Ok(Response::new(HealthResponse {
            node_id:      self.node_id.clone(),
            active_tasks: st.active_tasks as u32,
            ready:        true,
        }))
    }
}

