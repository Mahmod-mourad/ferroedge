use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{Extension, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::instrument;
use uuid::Uuid;

use common::{
    errors::EdgeError,
    types::{NodeInfo, TaskResult},
};
use proto::edge::{execute_task_request::WasmSource, ExecuteTaskRequest, ModuleRef};

use crate::{api::routes::TraceId, scheduler::least_loaded::select_least_loaded, state::AppState};

// ── W3C TraceContext carrier for tonic gRPC metadata ─────────────────────

/// Inject the current OTEL span context into tonic `MetadataMap` as a
/// `traceparent` header so the edge-node can continue the distributed trace.
struct MetadataInjector<'a>(&'a mut tonic::metadata::MetadataMap);

impl<'a> opentelemetry::propagation::Injector for MetadataInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        use tonic::metadata::{Ascii, MetadataKey, MetadataValue};
        if let (Ok(k), Ok(v)) = (
            key.parse::<MetadataKey<Ascii>>(),
            value.parse::<MetadataValue<Ascii>>(),
        ) {
            self.0.insert(k, v);
        }
    }
}

// ── Response envelope ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub data: Option<T>,
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Json<Self> {
        Json(ApiResponse {
            data: Some(data),
            error: None,
        })
    }
}

fn error_response(msg: impl Into<String>) -> Json<ApiResponse<serde_json::Value>> {
    Json(ApiResponse {
        data: None,
        error: Some(msg.into()),
    })
}

// ── AppError ───────────────────────────────────────────────────────────────

pub struct AppError(EdgeError);

impl From<EdgeError> for AppError {
    fn from(e: EdgeError) -> Self {
        AppError(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            EdgeError::TaskNotFound(_) => StatusCode::NOT_FOUND,
            EdgeError::NodeUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            EdgeError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            EdgeError::InvalidWasm(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, error_response(self.0.to_string())).into_response()
    }
}

// ── Request bodies ─────────────────────────────────────────────────────────

/// Submit a task with inline WASM bytes **or** a reference to a registered module.
#[derive(Deserialize, Clone)]
pub struct SubmitTaskRequest {
    pub wasm_base64: Option<String>,
    pub module_name: Option<String>,
    pub module_version: Option<String>,
    pub function_name: String,
    pub input_base64: String,
    pub timeout_ms: Option<u64>,
    pub memory_limit_mb: Option<u64>,
}

#[derive(Deserialize)]
pub struct RegisterNodeRequest {
    pub node_id: String,
    pub address: String,
    pub http_address: Option<String>,
    pub max_concurrent_tasks: Option<usize>,
}

#[derive(Deserialize)]
pub struct UploadModuleRequest {
    pub name: String,
    pub version: String,
    pub wasm_base64: String,
}

/// A single item in a batch task submission.
#[derive(Deserialize, Clone)]
pub struct BatchTaskItem {
    pub wasm_base64: Option<String>,
    pub module_name: Option<String>,
    pub module_version: Option<String>,
    pub function_name: String,
    pub input_base64: String,
    pub timeout_ms: Option<u64>,
    pub memory_limit_mb: Option<u64>,
}

#[derive(Deserialize)]
pub struct BatchSubmitRequest {
    /// Up to 100 tasks to execute concurrently.
    pub tasks: Vec<BatchTaskItem>,
}

// ── Retry configuration ────────────────────────────────────────────────────

pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_ms: 500,
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

async fn select_node_for_task(
    state: &Arc<RwLock<AppState>>,
    tried: &HashSet<String>,
) -> Option<NodeInfo> {
    let mut st = state.write().await;

    let mut excluded = tried.clone();
    for (node_id, cb) in st.circuit_breakers.iter_mut() {
        if !cb.allow_request() {
            excluded.insert(node_id.clone());
        }
    }

    let strategy = st.scheduling_strategy.clone();
    match strategy.as_str() {
        "least_loaded" => select_least_loaded(&st.nodes, &excluded).cloned(),
        _ => {
            let candidates: Vec<NodeInfo> = st
                .nodes
                .values()
                .filter(|n| n.healthy && !excluded.contains(&n.id))
                .cloned()
                .collect();
            if candidates.is_empty() {
                None
            } else {
                let idx = st.scheduler_index % candidates.len();
                st.scheduler_index = st.scheduler_index.wrapping_add(1);
                Some(candidates[idx].clone())
            }
        }
    }
}

/// Execute a task on the given node via gRPC, reusing a pooled channel.
///
/// The connection pool is locked briefly (hashmap lookup only — no I/O),
/// then released before the actual RPC, so concurrent tasks never block
/// each other waiting for the pool.
async fn grpc_execute(
    state: &Arc<RwLock<AppState>>,
    node_id: &str,
    address: &str,
    req: ExecuteTaskRequest,
) -> Result<proto::edge::ExecuteTaskResponse, tonic::Status> {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    // Grab (or lazily create) a channel — non-blocking, no network I/O.
    let pool_arc = state.read().await.connection_pool.clone();
    let mut client = pool_arc.lock().await.get_or_create(node_id, address)?;

    let mut tonic_req = tonic::Request::new(req);

    // Propagate the current W3C trace context so the edge-node can continue
    // the distributed trace as a child span.
    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|p| {
        p.inject_context(&cx, &mut MetadataInjector(tonic_req.metadata_mut()));
    });

    client.execute_task(tonic_req).await.map(|r| r.into_inner())
}

// ── Core task dispatch (shared by submit_task and submit_tasks_batch) ──────

/// Dispatch a single task through the retry/scheduler loop.
/// Returns the `task_id` string (even on failure — the task entry is stored).
async fn dispatch_task(
    state: Arc<RwLock<AppState>>,
    task_id: String,
    trace_id: String,
    input: Vec<u8>,
    wasm_source: WasmSource,
    function_name: String,
    timeout_ms: u64,
    memory_limit_mb: u64,
) -> String {
    telemetry::metrics::TASKS_SUBMITTED_TOTAL.inc();
    state.write().await.total_submitted += 1;

    let retry_cfg = RetryConfig::default();
    let mut tried_nodes: HashSet<String> = HashSet::new();
    #[allow(unused_assignments)]
    let mut last_error = String::new();
    let mut no_node_count = 0u32;
    let mut capacity_waits = 0u32;
    const MAX_CAPACITY_WAITS: u32 = 30;
    let mut error_retries = 0u32;
    let e2e_start = Instant::now();

    loop {
        // ── Schedule ─────────────────────────────────────────────────────────
        let node = loop {
            match select_node_for_task(&state, &tried_nodes).await {
                Some(n) => break n,
                None => {
                    no_node_count += 1;
                    if no_node_count > 3 {
                        tracing::warn!(
                            task_id  = %task_id,
                            trace_id = %trace_id,
                            "no nodes available after 3 queuing retries → giving up"
                        );
                        last_error = "no nodes available".to_string();
                        telemetry::metrics::TASKS_FAILED_TOTAL.inc();
                        let mut st = state.write().await;
                        st.total_failed += 1;
                        st.tasks.insert(
                            task_id.clone(),
                            TaskResult {
                                task_id: task_id.clone(),
                                node_id: "none".to_string(),
                                success: false,
                                output: Vec::new(),
                                execution_time_ms: 0,
                                error: Some(last_error.clone()),
                            },
                        );
                        return task_id;
                    }
                    state.write().await.queued += 1;
                    tracing::info!(
                        task_id      = %task_id,
                        trace_id     = %trace_id,
                        no_node_count,
                        "no schedulable node — queued, retrying in 1s"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    state.write().await.queued = state.read().await.queued.saturating_sub(1);
                }
            }
        };

        if error_retries > 0 {
            tracing::warn!(
                task_id     = %task_id,
                trace_id    = %trace_id,
                error_retry = error_retries,
                node_id     = %node.id,
                "retrying task on different node"
            );
            state.write().await.total_retried += 1;
        } else {
            tracing::info!(
                task_id  = %task_id,
                trace_id = %trace_id,
                node_id  = %node.id,
                "dispatching task"
            );
        }

        let grpc_req = ExecuteTaskRequest {
            task_id: task_id.clone(),
            function_name: function_name.clone(),
            input: input.clone(),
            timeout_ms,
            memory_limit_mb,
            trace_id: trace_id.clone(),
            wasm_source: Some(wasm_source.clone()),
        };

        let dispatch_span = tracing::info_span!(
            "grpc.dispatch_task",
            task_id  = %task_id,
            trace_id = %trace_id,
            node_id  = %node.id,
        );
        let result = {
            let _enter = dispatch_span.enter();
            grpc_execute(&state, &node.id, &node.address, grpc_req).await
        };

        match result {
            // ── Success ───────────────────────────────────────────────────────
            Ok(resp) if resp.success => {
                state
                    .write()
                    .await
                    .circuit_breakers
                    .entry(node.id.clone())
                    .or_default()
                    .record_success();

                let e2e_ms = e2e_start.elapsed().as_millis() as f64;
                telemetry::metrics::TASKS_SUCCESS_TOTAL.inc();
                telemetry::metrics::TASK_E2E_DURATION_MS.observe(e2e_ms);

                let task_result = TaskResult {
                    task_id: task_id.clone(),
                    node_id: node.id.clone(),
                    success: true,
                    output: resp.output,
                    execution_time_ms: resp.execution_time_ms,
                    error: None,
                };
                {
                    let mut st = state.write().await;
                    st.tasks.insert(task_id.clone(), task_result);
                    st.total_success += 1;
                }

                tracing::info!(
                    task_id      = %task_id,
                    trace_id     = %trace_id,
                    node_id      = %node.id,
                    duration_ms  = e2e_ms as u64,
                    "task succeeded"
                );
                return task_id;
            }

            // ── Execution failure (success=false) ────────────────────────────
            Ok(resp) => {
                last_error = resp.error.clone();
                tracing::warn!(
                    task_id     = %task_id,
                    trace_id    = %trace_id,
                    error_retry = error_retries + 1,
                    node_id     = %node.id,
                    error       = %last_error,
                    "task execution failed — marking node, will retry"
                );
                tried_nodes.insert(node.id.clone());
                state
                    .write()
                    .await
                    .circuit_breakers
                    .entry(node.id.clone())
                    .or_default()
                    .record_failure();
                error_retries += 1;
                if error_retries >= retry_cfg.max_attempts {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(retry_cfg.backoff_ms)).await;
            }

            // ── Node at capacity ─────────────────────────────────────────────
            Err(status) if status.code() == tonic::Code::ResourceExhausted => {
                capacity_waits += 1;
                tracing::debug!(
                    task_id        = %task_id,
                    trace_id       = %trace_id,
                    node_id        = %node.id,
                    capacity_waits,
                    "node at capacity — yielding 100ms"
                );
                if capacity_waits > MAX_CAPACITY_WAITS {
                    last_error = "all nodes persistently at capacity".to_string();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            // ── Transport / gRPC failure ──────────────────────────────────────
            Err(status) => {
                last_error = status.to_string();
                tracing::warn!(
                    task_id     = %task_id,
                    trace_id    = %trace_id,
                    error_retry = error_retries + 1,
                    node_id     = %node.id,
                    error       = %last_error,
                    "gRPC call failed — marking node, will retry"
                );
                tried_nodes.insert(node.id.clone());
                state
                    .write()
                    .await
                    .circuit_breakers
                    .entry(node.id.clone())
                    .or_default()
                    .record_failure();
                error_retries += 1;
                if error_retries >= retry_cfg.max_attempts {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(retry_cfg.backoff_ms)).await;
            }
        }
    }

    // All attempts exhausted.
    tracing::error!(
        task_id    = %task_id,
        trace_id   = %trace_id,
        last_error = %last_error,
        "all retry attempts failed"
    );
    let task_result = TaskResult {
        task_id: task_id.clone(),
        node_id: "none".to_string(),
        success: false,
        output: Vec::new(),
        execution_time_ms: 0,
        error: Some(format!("All nodes failed: {last_error}")),
    };
    let mut st = state.write().await;
    st.tasks.insert(task_id.clone(), task_result);
    st.total_failed += 1;
    telemetry::metrics::TASKS_FAILED_TOTAL.inc();

    task_id
}

// ── Handlers ───────────────────────────────────────────────────────────────

#[instrument(skip(state))]
pub async fn health(State(state): State<Arc<RwLock<AppState>>>) -> impl IntoResponse {
    let node_count = state.read().await.nodes.len();
    ApiResponse::ok(serde_json::json!({ "status": "ok", "node_count": node_count }))
}

#[instrument(skip(state))]
pub async fn metrics(State(state): State<Arc<RwLock<AppState>>>) -> impl IntoResponse {
    let st = state.read().await;

    let healthy_nodes = st.nodes.values().filter(|n| n.healthy).count();
    let unhealthy_nodes = st.nodes.values().filter(|n| !n.healthy).count();

    let cb_states: HashMap<String, &'static str> = st
        .nodes
        .keys()
        .map(|id| {
            let state_name = st
                .circuit_breakers
                .get(id)
                .map(|cb| cb.state_name())
                .unwrap_or("closed");
            (id.clone(), state_name)
        })
        .collect();

    Json(serde_json::json!({
        "scheduler": {
            "strategy":        st.scheduling_strategy,
            "healthy_nodes":   healthy_nodes,
            "unhealthy_nodes": unhealthy_nodes,
            "circuit_breakers": cb_states,
        },
        "tasks": {
            "total_submitted": st.total_submitted,
            "total_success":   st.total_success,
            "total_failed":    st.total_failed,
            "retried":         st.total_retried,
            "queued":          st.queued,
        }
    }))
}

/// POST /modules — upload a WASM module and proactively warm all healthy nodes.
#[instrument(skip(state, body))]
pub async fn upload_module(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(body): Json<UploadModuleRequest>,
) -> Result<impl IntoResponse, AppError> {
    let wasm_bytes = B64
        .decode(&body.wasm_base64)
        .map_err(|e| AppError(EdgeError::InvalidWasm(e.to_string())))?;

    let meta = state
        .write()
        .await
        .module_registry
        .store(&body.name, &body.version, wasm_bytes.clone())
        .map_err(|e| AppError(EdgeError::InvalidWasm(e.to_string())))?;

    tracing::info!(
        module_id  = %meta.id,
        name       = %meta.name,
        version    = %meta.version,
        size_bytes = meta.size_bytes,
        "module registered"
    );

    // ── Optimization 1: Pre-warm all healthy edge-nodes ─────────────────────
    // Fire-and-forget: do not block the upload response on warming completion.
    let node_http_addrs: Vec<String> = {
        let st = state.read().await;
        st.nodes
            .values()
            .filter(|n| n.healthy && !n.http_address.is_empty())
            .map(|n| n.http_address.clone())
            .collect()
    };

    if !node_http_addrs.is_empty() {
        let name = body.name.clone();
        let version = body.version.clone();
        let wasm_b64 = B64.encode(&wasm_bytes);
        tokio::spawn(async move {
            warm_nodes(node_http_addrs, name, version, wasm_b64).await;
        });
    }

    Ok(ApiResponse::ok(serde_json::json!({ "module_id": meta.id })))
}

/// Push a compiled module to a list of edge-node HTTP addresses.
async fn warm_nodes(
    node_http_addrs: Vec<String>,
    name: String,
    version: String,
    wasm_bytes_base64: String,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let body = serde_json::json!({
        "name":              name,
        "version":           version,
        "wasm_bytes_base64": wasm_bytes_base64,
    });

    let futures: Vec<_> = node_http_addrs
        .iter()
        .map(|addr| {
            let client = client.clone();
            let url = format!("{}/cache/warm", addr);
            let body = body.clone();
            async move {
                match client.post(&url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::info!(node_addr = %addr, "module pre-warm succeeded");
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            node_addr = %addr,
                            status    = %resp.status(),
                            "module pre-warm returned non-2xx"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(node_addr = %addr, error = %e, "module pre-warm failed");
                    }
                }
            }
        })
        .collect();

    futures::future::join_all(futures).await;
}

#[instrument(skip(state))]
pub async fn list_modules(State(state): State<Arc<RwLock<AppState>>>) -> impl IntoResponse {
    let guard = state.read().await;
    let modules: Vec<serde_json::Value> = guard
        .module_registry
        .list()
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect();
    ApiResponse::ok(modules)
}

#[instrument(skip(state))]
pub async fn get_module_bytes(
    State(state): State<Arc<RwLock<AppState>>>,
    Path((name, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let guard = state.read().await;
    let (_, bytes) = guard
        .module_registry
        .get(&name, &version)
        .ok_or_else(|| AppError(EdgeError::TaskNotFound(format!("{name}:{version}"))))?;

    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        Bytes::copy_from_slice(bytes),
    ))
}

/// POST /tasks — dispatch a single task to an edge-node.
#[instrument(skip(state, body), fields(trace_id = tracing::field::Empty, task_id = tracing::field::Empty))]
pub async fn submit_task(
    State(state): State<Arc<RwLock<AppState>>>,
    Extension(TraceId(trace_id)): Extension<TraceId>,
    Json(body): Json<SubmitTaskRequest>,
) -> Result<impl IntoResponse, AppError> {
    let task_id = Uuid::new_v4().to_string();
    tracing::Span::current()
        .record("trace_id", &tracing::field::display(&trace_id))
        .record("task_id", &tracing::field::display(&task_id));

    let (input, wasm_source) = resolve_task_inputs(&state, &body).await?;
    let timeout_ms = body.timeout_ms.unwrap_or(5_000);
    let memory_limit_mb = body.memory_limit_mb.unwrap_or(64);

    let returned_id = dispatch_task(
        state,
        task_id,
        trace_id,
        input,
        wasm_source,
        body.function_name,
        timeout_ms,
        memory_limit_mb,
    )
    .await;

    Ok(ApiResponse::ok(
        serde_json::json!({ "task_id": returned_id }),
    ))
}

/// POST /tasks/batch — dispatch up to 100 tasks concurrently.
///
/// All tasks are submitted simultaneously via `FuturesUnordered`; each is
/// independently scheduled, retried, and stored.  The response returns all
/// task IDs in submission order.
#[instrument(skip(state, body), fields(trace_id = tracing::field::Empty, batch_size))]
pub async fn submit_tasks_batch(
    State(state): State<Arc<RwLock<AppState>>>,
    Extension(TraceId(trace_id)): Extension<TraceId>,
    Json(body): Json<BatchSubmitRequest>,
) -> Result<impl IntoResponse, AppError> {
    if body.tasks.is_empty() {
        let empty: Vec<String> = Vec::new();
        return Ok(ApiResponse::ok(serde_json::json!({ "task_ids": empty })));
    }
    if body.tasks.len() > 100 {
        return Err(AppError(EdgeError::InvalidWasm(
            "batch limit is 100 tasks per request".to_string(),
        )));
    }

    tracing::Span::current()
        .record("trace_id", &tracing::field::display(&trace_id))
        .record("batch_size", body.tasks.len());

    // Build all task futures, resolving WASM sources upfront.
    let mut task_futures = Vec::with_capacity(body.tasks.len());

    for item in body.tasks {
        let req = SubmitTaskRequest {
            wasm_base64: item.wasm_base64,
            module_name: item.module_name,
            module_version: item.module_version,
            function_name: item.function_name,
            input_base64: item.input_base64,
            timeout_ms: item.timeout_ms,
            memory_limit_mb: item.memory_limit_mb,
        };

        let (input, wasm_source) = resolve_task_inputs(&state, &req).await?;

        let task_id = Uuid::new_v4().to_string();
        let timeout_ms = req.timeout_ms.unwrap_or(5_000);
        let mem_limit = req.memory_limit_mb.unwrap_or(64);
        let fn_name = req.function_name.clone();
        let tc = trace_id.clone();
        let state_clone = Arc::clone(&state);
        let tid = task_id.clone();

        task_futures.push(tokio::spawn(async move {
            dispatch_task(
                state_clone,
                tid,
                tc,
                input,
                wasm_source,
                fn_name,
                timeout_ms,
                mem_limit,
            )
            .await
        }));
    }

    // Await all concurrently.
    let results = futures::future::join_all(task_futures).await;
    let task_ids: Vec<String> = results
        .into_iter()
        .map(|r| r.unwrap_or_else(|_| "error".to_string()))
        .collect();

    tracing::info!(
        trace_id   = %trace_id,
        task_count = task_ids.len(),
        "batch submitted"
    );

    Ok(ApiResponse::ok(serde_json::json!({ "task_ids": task_ids })))
}

/// Decode input bytes and resolve WASM source from a task request.
async fn resolve_task_inputs(
    state: &Arc<RwLock<AppState>>,
    body: &SubmitTaskRequest,
) -> Result<(Vec<u8>, WasmSource), AppError> {
    let input = B64
        .decode(&body.input_base64)
        .map_err(|e| AppError(EdgeError::InvalidWasm(e.to_string())))?;

    let wasm_source: WasmSource = match (&body.module_name, &body.module_version) {
        (Some(name), Some(version)) => {
            let guard = state.read().await;
            let (meta, _) = guard
                .module_registry
                .get(name, version)
                .ok_or_else(|| AppError(EdgeError::TaskNotFound(format!("{name}:{version}"))))?;
            let fetch_url = format!("{}/modules/{}/{}", guard.self_url, name, version);
            let module_ref = ModuleRef {
                name: name.clone(),
                version: version.clone(),
                hash: meta.hash.clone(),
                fetch_url,
            };
            drop(guard);
            WasmSource::ModuleRef(module_ref)
        }
        _ => {
            let wasm_bytes = B64
                .decode(body.wasm_base64.as_deref().ok_or_else(|| {
                    AppError(EdgeError::InvalidWasm(
                        "provide wasm_base64 or module_name+module_version".to_string(),
                    ))
                })?)
                .map_err(|e| AppError(EdgeError::InvalidWasm(e.to_string())))?;
            WasmSource::WasmBytes(wasm_bytes)
        }
    };

    Ok((input, wasm_source))
}

#[instrument(skip(state))]
pub async fn get_task(
    State(state): State<Arc<RwLock<AppState>>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let guard = state.read().await;
    match guard.tasks.get(&id) {
        Some(result) => Ok(ApiResponse::ok(
            serde_json::to_value(result).unwrap_or_default(),
        )),
        None => Ok(ApiResponse::ok(serde_json::json!({ "status": "pending" }))),
    }
}

#[instrument(skip(state, body))]
pub async fn register_node(
    State(state): State<Arc<RwLock<AppState>>>,
    Json(body): Json<RegisterNodeRequest>,
) -> impl IntoResponse {
    let node = NodeInfo {
        id: body.node_id.clone(),
        address: body.address,
        http_address: body.http_address.unwrap_or_default(),
        active_tasks: 0,
        healthy: true,
        max_concurrent_tasks: body.max_concurrent_tasks.unwrap_or(10),
    };
    state.write().await.nodes.insert(body.node_id.clone(), node);
    tracing::info!(node_id = %body.node_id, "node registered");
    ApiResponse::ok(serde_json::json!({ "registered": true }))
}

#[instrument(skip(state))]
pub async fn list_nodes(State(state): State<Arc<RwLock<AppState>>>) -> impl IntoResponse {
    let nodes: Vec<NodeInfo> = state.read().await.nodes.values().cloned().collect();
    ApiResponse::ok(nodes)
}
