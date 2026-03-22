//! End-to-end integration test:
//! 1. Starts the control-plane HTTP server in a background task.
//! 2. Starts the edge-node gRPC server in a background task.
//! 3. Waits 500 ms for both to be ready.
//! 4. Registers the edge-node with the control-plane.
//! 5. Submits a task (tiny echo WASM).
//! 6. Polls GET /tasks/:id until the result is available (max 5 s).
//! 7. Asserts success = true.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tokio::sync::{RwLock, Semaphore};
use tonic::transport::Server as TonicServer;

use control_plane::{api::build_router, state::AppState};
use edge_node::{downloader::ModuleDownloader, server::EdgeGrpcServer, state::NodeState};
use proto::edge::edge_service_server::EdgeServiceServer;
use wasm_runtime::{CompiledModuleCache, WasmEngine, WasmSandbox};

// Use ports unlikely to clash with dev services.
const CP_PORT: u16 = 18_080;
const EN_PORT: u16 = 15_051;

/// Echo WAT: returns the input bytes unchanged.
/// Exports: memory, alloc, process.
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

#[tokio::test]
async fn test_full_task_flow() {
    // ── 1. Start control-plane ────────────────────────────────────────────────
    let cp_state = Arc::new(RwLock::new(AppState::default()));
    let cp_router = build_router(Arc::clone(&cp_state));
    let cp_addr = SocketAddr::from(([127, 0, 0, 1], CP_PORT));
    let cp_listener = tokio::net::TcpListener::bind(cp_addr)
        .await
        .expect("bind control-plane port");
    tokio::spawn(async move {
        axum::serve(cp_listener, cp_router).await.ok();
    });

    // ── 2. Start edge-node ────────────────────────────────────────────────────
    let engine = Arc::new(WasmEngine::new().expect("WasmEngine"));
    let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));
    let module_cache = Arc::new(CompiledModuleCache::new(Arc::clone(&engine), 50, 300));
    let downloader = Arc::new(ModuleDownloader::new(format!("http://127.0.0.1:{CP_PORT}")));
    let en_state = Arc::new(RwLock::new(NodeState::new("test-node")));
    let semaphore = Arc::new(Semaphore::new(10));
    let grpc_svc = EdgeGrpcServer {
        node_id: "test-node".to_string(),
        sandbox,
        module_cache,
        downloader,
        state: Arc::clone(&en_state),
        semaphore,
    };
    let en_addr = SocketAddr::from(([127, 0, 0, 1], EN_PORT));
    tokio::spawn(async move {
        TonicServer::builder()
            .add_service(EdgeServiceServer::new(grpc_svc))
            .serve(en_addr)
            .await
            .ok();
    });

    // ── 3. Wait for both servers to be ready ──────────────────────────────────
    tokio::time::sleep(Duration::from_millis(500)).await;

    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{CP_PORT}");

    // ── 4. Register edge-node ─────────────────────────────────────────────────
    let reg = http
        .post(format!("{base}/nodes/register"))
        .json(&serde_json::json!({
            "node_id": "test-node",
            "address": format!("http://127.0.0.1:{EN_PORT}"),
        }))
        .send()
        .await
        .expect("register request");
    assert!(
        reg.status().is_success(),
        "node registration failed: {}",
        reg.status()
    );

    // ── 5. Submit task ────────────────────────────────────────────────────────
    let wasm_b64 = B64.encode(echo_wasm_bytes());
    let input_b64 = B64.encode(b"hello");

    let submit = http
        .post(format!("{base}/tasks"))
        .json(&serde_json::json!({
            "wasm_base64":   wasm_b64,
            "function_name": "process",
            "input_base64":  input_b64,
        }))
        .send()
        .await
        .expect("submit request");
    assert!(
        submit.status().is_success(),
        "task submission failed: {}",
        submit.status()
    );

    let submit_body: serde_json::Value = submit.json().await.expect("submit JSON");
    let task_id = submit_body["data"]["task_id"]
        .as_str()
        .expect("task_id in response")
        .to_string();

    // ── 6. Poll GET /tasks/:id until result is ready (max 5 s) ───────────────
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let poll: serde_json::Value = http
            .get(format!("{base}/tasks/{task_id}"))
            .send()
            .await
            .expect("poll request")
            .json()
            .await
            .expect("poll JSON");

        let data = &poll["data"];

        // Still pending?
        if data.get("status").and_then(|s| s.as_str()) == Some("pending") {
            assert!(
                std::time::Instant::now() < deadline,
                "task did not complete within 5 seconds"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // ── 7. Assert success ─────────────────────────────────────────────────
        assert_eq!(
            data.get("success").and_then(|v| v.as_bool()),
            Some(true),
            "expected success=true, got: {poll}"
        );

        // Output should equal the input ("hello").
        let raw_output = data["output"]
            .as_array()
            .expect("output is a byte array")
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect::<Vec<_>>();
        assert_eq!(raw_output, b"hello", "output mismatch");

        break;
    }
}
