//! Criterion benchmarks for WASM execution performance.
//!
//! Run with: `cargo bench -p wasm-runtime`
//!
//! Four scenarios:
//!   cold_execution          — new engine + compile per iteration (worst case)
//!   warm_execution          — pre-compiled module, only measures execution
//!   concurrent_execution_10 — 10 concurrent executions per iteration
//!   large_input_1mb         — 1 MB payload, measures memory-copy overhead

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::sync::Arc;
use wasm_runtime::{CompiledModuleCache, ExecutionConfig, WasmEngine, WasmSandbox};

// ─── WAT helpers ─────────────────────────────────────────────────────────────

/// Minimal echo module: alloc always returns address 0; process returns length.
/// One 64 KB page is sufficient for small inputs.
fn echo_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
             (memory (export "memory") 1)
             (func (export "alloc") (param i32) (result i32) i32.const 0)
             (func (export "process") (param i32) (param i32) (result i32)
               local.get 1)
           )"#,
    )
    .expect("valid WAT")
}

/// Same echo module but with 17 pages (1 088 KB) to hold a 1 MB input.
fn echo_wasm_large() -> Vec<u8> {
    wat::parse_str(
        r#"(module
             (memory (export "memory") 17)
             (func (export "alloc") (param i32) (result i32) i32.const 0)
             (func (export "process") (param i32) (param i32) (result i32)
               local.get 1)
           )"#,
    )
    .expect("valid WAT large")
}

fn exec_config() -> ExecutionConfig {
    ExecutionConfig {
        memory_limit_bytes: 64 * 1024 * 1024,
        timeout_ms: 10_000,
        function_name: "process".to_string(),
    }
}

// ─── Benchmarks ───────────────────────────────────────────────────────────────

fn wasm_benchmarks(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Shared byte slices (static refs are Send + 'static)
    let wasm_bytes = Arc::new(echo_wasm());
    let wasm_large = Arc::new(echo_wasm_large());

    // Single engine shared across all warm/concurrent/large benchmarks.
    let engine = Arc::new(WasmEngine::new().unwrap());

    // Pre-compile the two modules once.
    let compiled_small = rt.block_on(async {
        let cache = CompiledModuleCache::new(Arc::clone(&engine), 10, 300);
        cache
            .get_or_compile("echo:1.0", &wasm_bytes)
            .await
            .unwrap()
    });
    let compiled_large = rt.block_on(async {
        let cache = CompiledModuleCache::new(Arc::clone(&engine), 10, 300);
        cache
            .get_or_compile("echo-1mb:1.0", &wasm_large)
            .await
            .unwrap()
    });

    static INPUT_SMALL: &[u8] = b"hello benchmark";
    let input_large: Arc<[u8]> = Arc::from(vec![0xAB_u8; 1024 * 1024].as_slice());

    let mut group = c.benchmark_group("wasm_execution");

    // ── Benchmark 1: Cold execution ─────────────────────────────────────────
    // New WasmSandbox created per iteration; module compiled from bytes each time.
    // Shows: compilation overhead on cache-miss path.
    group.throughput(Throughput::Elements(1));
    {
        let wasm = Arc::clone(&wasm_bytes);
        let engine = Arc::clone(&engine);
        group.bench_function("cold_execution", |b| {
            let wasm = Arc::clone(&wasm);
            let engine = Arc::clone(&engine);
            b.to_async(&rt).iter(move || {
                let wasm = Arc::clone(&wasm);
                let engine = Arc::clone(&engine);
                async move {
                    // Reuse shared engine but compile from bytes every iteration.
                    let sandbox = WasmSandbox::new(Arc::clone(&engine));
                    sandbox
                        .execute(&wasm, exec_config(), INPUT_SMALL)
                        .await
                        .unwrap()
                }
            });
        });
    }

    // ── Benchmark 2: Warm execution ─────────────────────────────────────────
    // Module already compiled; benchmark measures instantiation + execution only.
    // Shows: runtime overhead without compilation — should be 10x+ faster.
    {
        let module = Arc::clone(&compiled_small);
        let engine = Arc::clone(&engine);
        group.bench_function("warm_execution", |b| {
            let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));
            let module = Arc::clone(&module);
            b.to_async(&rt).iter(move || {
                let sandbox = Arc::clone(&sandbox);
                let module = Arc::clone(&module);
                async move {
                    sandbox
                        .execute_compiled(module, exec_config(), INPUT_SMALL)
                        .await
                        .unwrap()
                }
            });
        });
    }

    // ── Benchmark 3: Concurrent execution (10 tasks / iteration) ────────────
    // 10 tokio tasks each executing the same pre-compiled module simultaneously.
    // Shows: scalability — contention on spawn_blocking thread pool.
    group.throughput(Throughput::Elements(10));
    {
        let module = Arc::clone(&compiled_small);
        let engine = Arc::clone(&engine);
        group.bench_function("concurrent_execution_10", |b| {
            let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));
            let module = Arc::clone(&module);
            b.to_async(&rt).iter(move || {
                let sandbox = Arc::clone(&sandbox);
                let module = Arc::clone(&module);
                async move {
                    let mut set = tokio::task::JoinSet::new();
                    for _ in 0..10 {
                        let s = Arc::clone(&sandbox);
                        let m = Arc::clone(&module);
                        set.spawn(async move {
                            s.execute_compiled(m, exec_config(), INPUT_SMALL)
                                .await
                                .unwrap()
                        });
                    }
                    while set.join_next().await.is_some() {}
                }
            });
        });
    }

    // ── Benchmark 4: Large input (1 MB) ─────────────────────────────────────
    // Executes with 1 MB of input, stressing memory writes into WASM linear memory.
    // Shows: memory-copy performance at scale.
    group.throughput(Throughput::Bytes(1024 * 1024));
    {
        let module = Arc::clone(&compiled_large);
        let engine = Arc::clone(&engine);
        let input = Arc::clone(&input_large);
        group.bench_function("large_input_1mb", |b| {
            let sandbox = Arc::new(WasmSandbox::new(Arc::clone(&engine)));
            let module = Arc::clone(&module);
            let input = Arc::clone(&input);
            b.to_async(&rt).iter(move || {
                let sandbox = Arc::clone(&sandbox);
                let module = Arc::clone(&module);
                let input = Arc::clone(&input);
                async move {
                    let cfg = ExecutionConfig {
                        memory_limit_bytes: 128 * 1024 * 1024,
                        timeout_ms: 30_000,
                        function_name: "process".to_string(),
                    };
                    sandbox
                        .execute_compiled(module, cfg, &input)
                        .await
                        .unwrap()
                }
            });
        });
    }

    group.finish();
}

criterion_group!(benches, wasm_benchmarks);
criterion_main!(benches);
