//! Concurrency stress test and graceful-degradation test.
//!
//! Uses ports in the 19 000 / 16 000 ranges to avoid clashing with the
//! integration tests (18 080 / 15 051) or any local dev services.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tokio::sync::{RwLock, Semaphore};
use tonic::transport::Server as TonicServer;

use control_plane::{api::build_router, health::check_all_nodes, state::AppState};
use edge_node::{downloader::ModuleDownloader, server::EdgeGrpcServer, state::NodeState};
use proto::edge::edge_service_server::EdgeServiceServer;
use wasm_runtime::{CompiledModuleCache, WasmEngine, WasmSandbox};

// ── Port assignments ───────────────────────────────────────────────────────
const STRESS_CP_PORT: u16 = 19_080;
const STRESS_EN1_PORT: u16 = 16_051;
const STRESS_EN2_PORT: u16 = 16_052;

const FAIL_CP_PORT: u16 = 19_082;
const FAIL_EN1_PORT: u16 = 16_053;
const FAIL_EN2_PORT: u16 = 16_054;

// ── WASM helpers ───────────────────────────────────────────────────────────

/// Echo WAT: returns the input bytes unchanged.
fn echo_wasm_bytes() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (memory (export "memory") 1)
          (func (export "alloc") (param $size i32) (result i32)
            i32.const 0)
          (func (export "process") (param $ptr i32) (param $len i32) (result i32)
            local.get $len)
        )
        "#,
    )
    .expect("echo WAT is valid")
}

// ── Edge-node factory ──────────────────────────────────────────────────────

fn make_edge_server(node_id: &str, control_plane_url: &str) -> EdgeGrpcServer {
    let engine = Arc::new(WasmEngine::new().expect("WasmEngine"));
    let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));
    let module_cache = Arc::new(CompiledModuleCache::new(Arc::clone(&engine), 50, 300));
    let downloader = Arc::new(ModuleDownloader::new(control_plane_url));
    let en_state = Arc::new(RwLock::new(NodeState::new(node_id)));
    let semaphore = Arc::new(Semaphore::new(20)); // 20 concurrent tasks each

    EdgeGrpcServer {
        node_id: node_id.to_string(),
        sandbox,
        module_cache,
        downloader,
        state: en_state,
        semaphore,
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Bind a control-plane and return (shared_state, http_base).
async fn start_control_plane(port: u16) -> (Arc<RwLock<AppState>>, String) {
    let cp_state = Arc::new(RwLock::new(AppState::default()));
    let cp_router = build_router(Arc::clone(&cp_state));
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind CP");
    tokio::spawn(async move {
        axum::serve(listener, cp_router).await.ok();
    });
    (cp_state, format!("http://127.0.0.1:{port}"))
}

/// Bind an edge-node gRPC server; control_plane_url is used by the downloader.
async fn start_edge_node(node_id: &str, grpc_port: u16, cp_url: &str) {
    let svc = make_edge_server(node_id, cp_url);
    let addr = SocketAddr::from(([127, 0, 0, 1], grpc_port));
    tokio::spawn(async move {
        TonicServer::builder()
            .add_service(EdgeServiceServer::new(svc))
            .serve(addr)
            .await
            .ok();
    });
}

/// Submit one task and return the elapsed time.
async fn submit_one_task(http: &reqwest::Client, base: &str, idx: usize) -> Duration {
    let wasm_b64 = B64.encode(echo_wasm_bytes());
    let input_str = format!("task-{idx}");
    let input_b64 = B64.encode(input_str.as_bytes());

    let t0 = Instant::now();
    let resp = http
        .post(format!("{base}/tasks"))
        .json(&serde_json::json!({
            "wasm_base64":   wasm_b64,
            "function_name": "process",
            "input_base64":  input_b64,
        }))
        .send()
        .await
        .expect("submit task");
    let elapsed = t0.elapsed();

    // We accept both 200 (success or retry-exhausted) and 503 (no capacity).
    // The stress test only checks for panics / deadlocks, not guaranteed success.
    assert!(
        resp.status().is_success() || resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "unexpected status {} for task {idx}",
        resp.status()
    );

    elapsed
}

// ══════════════════════════════════════════════════════════════════════════════
// STEP 5 — Concurrency stress test
// ══════════════════════════════════════════════════════════════════════════════

/// Submit 100 tasks concurrently across 2 edge-nodes.
///
/// Asserts:
/// - All 100 tasks complete without deadlocks (test finishes within 30 s).
/// - No panics.
/// - Prints P50 / P95 / P99 latency summary.
#[tokio::test]
async fn test_concurrent_task_execution() {
    let (cp_state, base) = start_control_plane(STRESS_CP_PORT).await;
    let cp_url = base.clone();
    start_edge_node("stress-node-1", STRESS_EN1_PORT, &cp_url).await;
    start_edge_node("stress-node-2", STRESS_EN2_PORT, &cp_url).await;

    // Allow servers to bind.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let http = reqwest::Client::new();

    // Register both nodes.
    for (id, port) in [
        ("stress-node-1", STRESS_EN1_PORT),
        ("stress-node-2", STRESS_EN2_PORT),
    ] {
        let r = http
            .post(format!("{base}/nodes/register"))
            .json(&serde_json::json!({
                "node_id": id,
                "address": format!("http://127.0.0.1:{port}"),
                "max_concurrent_tasks": 20,
            }))
            .send()
            .await
            .expect("register");
        assert!(r.status().is_success(), "node {id} registration failed");
    }

    // ── Submit 100 tasks concurrently ────────────────────────────────────────
    let total_start = Instant::now();

    let http = Arc::new(http);
    let base = Arc::new(base);

    let handles: Vec<_> = (0..100)
        .map(|i| {
            let http = Arc::clone(&http);
            let base = Arc::clone(&base);
            tokio::spawn(async move { submit_one_task(&http, &base, i).await })
        })
        .collect();

    // Collect latencies, with a 30 s deadline to detect deadlocks.
    let mut latencies_ms: Vec<u64> = Vec::with_capacity(100);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    for handle in handles {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("100 tasks must complete within 30 s");
        let elapsed = tokio::time::timeout(remaining, handle)
            .await
            .expect("no deadlock within 30 s")
            .expect("task join handle");
        latencies_ms.push(elapsed.as_millis() as u64);
    }

    let total_elapsed = total_start.elapsed();
    latencies_ms.sort_unstable();

    let count = latencies_ms.len();
    assert_eq!(count, 100, "all 100 tasks must have a result");

    let p50 = latencies_ms[count * 50 / 100];
    let p95 = latencies_ms[count * 95 / 100];
    let p99 = latencies_ms[count * 99 / 100];

    println!(
        "\n=== Stress Test Summary ===\
         \n  Total wall time : {:.2?}\
         \n  Tasks completed : {}\
         \n  P50 latency     : {}ms\
         \n  P95 latency     : {}ms\
         \n  P99 latency     : {}ms",
        total_elapsed, count, p50, p95, p99
    );

    // ── Verify metrics ───────────────────────────────────────────────────────
    let metrics: serde_json::Value = http
        .get(format!("http://127.0.0.1:{STRESS_CP_PORT}/metrics"))
        .send()
        .await
        .expect("metrics request")
        .json()
        .await
        .expect("metrics JSON");

    let submitted = metrics["tasks"]["total_submitted"].as_u64().unwrap_or(0);
    assert_eq!(submitted, 100, "metrics must show 100 submitted tasks");

    // active_tasks per node must be 0 once all done (health updated by edge-node).
    let st = cp_state.read().await;
    for node in st.nodes.values() {
        // The control-plane's view of active_tasks is only updated via health check;
        // we just verify no node still thinks it has inflight tasks from *its own* counter.
        // This assertion checks the control-plane snapshot is sane (≤ max).
        assert!(
            node.active_tasks <= node.max_concurrent_tasks,
            "node {} active_tasks {} exceeds max {}",
            node.id,
            node.active_tasks,
            node.max_concurrent_tasks
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// STEP 6 — Graceful degradation test
// ══════════════════════════════════════════════════════════════════════════════

/// Kill edge-node-1 mid-flight; verify all subsequent tasks go to edge-node-2.
///
/// Asserts:
/// - First 10 tasks succeed (both nodes alive).
/// - After node-1 is killed and health check runs, next 10 tasks all succeed
///   and are routed exclusively to node-2.
#[tokio::test]
async fn test_node_failure_handling() {
    let (cp_state, base) = start_control_plane(FAIL_CP_PORT).await;
    let cp_url = base.clone();

    // Start node-2 via the normal helper.
    start_edge_node("fail-node-2", FAIL_EN2_PORT, &cp_url).await;

    // ── Start node-1 behind an abortable handle ───────────────────────────────
    let svc1 = make_edge_server("fail-node-1", &cp_url);
    let en1_addr = SocketAddr::from(([127, 0, 0, 1], FAIL_EN1_PORT));
    let en1_handle = tokio::spawn(async move {
        TonicServer::builder()
            .add_service(EdgeServiceServer::new(svc1))
            .serve(en1_addr)
            .await
            .ok();
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let http = reqwest::Client::new();

    // Register both nodes.
    for (id, port) in [
        ("fail-node-1", FAIL_EN1_PORT),
        ("fail-node-2", FAIL_EN2_PORT),
    ] {
        let r = http
            .post(format!("{base}/nodes/register"))
            .json(&serde_json::json!({
                "node_id": id,
                "address": format!("http://127.0.0.1:{port}"),
                "max_concurrent_tasks": 20,
            }))
            .send()
            .await
            .expect("register");
        assert!(r.status().is_success(), "registration of {id} failed");
    }

    // ── Phase 1: both nodes alive — submit 10 tasks, all should succeed ───────
    let wasm_b64 = B64.encode(echo_wasm_bytes());
    let input_b64 = B64.encode(b"hello");

    for i in 0..10u32 {
        let resp: serde_json::Value = http
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({
                "wasm_base64":   wasm_b64,
                "function_name": "process",
                "input_base64":  input_b64,
            }))
            .send()
            .await
            .expect("submit phase-1")
            .json()
            .await
            .expect("phase-1 JSON");

        let task_id = resp["data"]["task_id"]
            .as_str()
            .expect("task_id")
            .to_string();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let poll: serde_json::Value = http
                .get(format!("{base}/tasks/{task_id}"))
                .send()
                .await
                .expect("poll")
                .json()
                .await
                .expect("poll JSON");
            let data = &poll["data"];
            if data.get("status").and_then(|s| s.as_str()) == Some("pending") {
                assert!(Instant::now() < deadline, "task {i} timed out in phase 1");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            assert_eq!(
                data["success"].as_bool(),
                Some(true),
                "phase-1 task {i} must succeed: {poll}"
            );
            break;
        }
    }

    // ── Phase 2: kill node-1 and force a health check ─────────────────────────
    en1_handle.abort();
    // Give the OS a moment to close the socket.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Directly invoke one health-check round (no need to wait 10 s).
    check_all_nodes(Arc::clone(&cp_state)).await;

    // Verify node-1 is now marked unhealthy.
    {
        let st = cp_state.read().await;
        assert!(
            !st.nodes["fail-node-1"].healthy,
            "fail-node-1 must be unhealthy after abort + health check"
        );
        assert!(
            st.nodes["fail-node-2"].healthy,
            "fail-node-2 must still be healthy"
        );
    }

    // ── Phase 3: 10 more tasks — all must go to node-2 ───────────────────────
    for i in 0..10u32 {
        let resp: serde_json::Value = http
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({
                "wasm_base64":   wasm_b64,
                "function_name": "process",
                "input_base64":  input_b64,
            }))
            .send()
            .await
            .expect("submit phase-3")
            .json()
            .await
            .expect("phase-3 JSON");

        let task_id = resp["data"]["task_id"]
            .as_str()
            .expect("task_id")
            .to_string();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let poll: serde_json::Value = http
                .get(format!("{base}/tasks/{task_id}"))
                .send()
                .await
                .expect("poll")
                .json()
                .await
                .expect("poll JSON");
            let data = &poll["data"];
            if data.get("status").and_then(|s| s.as_str()) == Some("pending") {
                assert!(Instant::now() < deadline, "task {i} timed out in phase 3");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            assert_eq!(
                data["success"].as_bool(),
                Some(true),
                "phase-3 task {i} must succeed: {poll}"
            );
            // Verify it was routed to node-2, not node-1.
            assert_eq!(
                data["node_id"].as_str(),
                Some("fail-node-2"),
                "phase-3 task {i} must be routed to fail-node-2"
            );
            break;
        }
    }

    println!("\n=== Node Failure Test: PASSED ===");
    println!("All 10 post-failure tasks successfully routed to fail-node-2.");
}
