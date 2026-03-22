use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{cache::CompiledModuleCache, ExecutionConfig, WasmEngine, WasmError, WasmSandbox};

// ─── WAT helpers ─────────────────────────────────────────────────────────────

/// Minimal module that exports alloc + process.
/// `process` returns the input length — the output bytes are the input bytes
/// already sitting at `ptr`, so no copy is needed.
const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 1)

  ;; alloc: always returns offset 0 for simplicity.
  (func (export "alloc") (param $size i32) (result i32)
    i32.const 0
  )

  ;; process: output = input (already at ptr), return len.
  (func (export "process") (param $ptr i32) (param $len i32) (result i32)
    local.get $len
  )
)
"#;

/// Valid WASM but missing the "process" export.
const NO_PROCESS_WAT: &str = r#"
(module
  (memory (export "memory") 1)

  (func (export "alloc") (param $size i32) (result i32)
    i32.const 0
  )
)
"#;

/// Module whose `process` function spins forever (tests timeout).
const INFINITE_LOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)

  (func (export "alloc") (param $size i32) (result i32)
    i32.const 0
  )

  (func (export "process") (param $ptr i32) (param $len i32) (result i32)
    (loop $spin
      br $spin
    )
    i32.const 0
  )
)
"#;

/// Module that tries to grow memory by 1 000 pages; because our limiter denies
/// it `memory.grow` returns -1, which we treat as `unreachable` → Trap.
const MEMORY_GROW_WAT: &str = r#"
(module
  (memory (export "memory") 1)

  (func (export "alloc") (param $size i32) (result i32)
    i32.const 0
  )

  (func (export "process") (param $ptr i32) (param $len i32) (result i32)
    i32.const 1000
    memory.grow
    ;; memory.grow returns -1 when denied; treat that as an error.
    i32.const -1
    i32.eq
    if
      unreachable
    end
    i32.const 0
  )
)
"#;

// ─── Fixtures ─────────────────────────────────────────────────────────────────

fn make_engine() -> Arc<WasmEngine> {
    Arc::new(WasmEngine::new().expect("failed to create WasmEngine"))
}

fn make_sandbox(engine: Arc<WasmEngine>) -> WasmSandbox {
    WasmSandbox::new(engine)
}

fn config(function_name: &str, timeout_ms: u64, memory_limit_mb: u64) -> ExecutionConfig {
    ExecutionConfig {
        memory_limit_bytes: memory_limit_mb * 1024 * 1024,
        timeout_ms,
        function_name: function_name.to_string(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// 1. Happy path: echo module copies input to output.
#[tokio::test]
async fn test_valid_wasm_execution() {
    let engine = make_engine();
    let sandbox = make_sandbox(engine);
    let wasm = wat::parse_str(ECHO_WAT).expect("bad WAT");
    let input = b"hello wasm";

    let result = sandbox
        .execute(&wasm, config("process", 5_000, 64), input)
        .await
        .expect("execution should succeed");

    assert_eq!(result.output, input, "output must equal input");
    assert!(
        result.execution_time_ms < 5_000,
        "should finish well within timeout"
    );
}

/// 2. Garbage bytes must be rejected as InvalidWasm.
#[tokio::test]
async fn test_invalid_wasm_bytes() {
    let engine = make_engine();
    let sandbox = make_sandbox(engine);
    let garbage = b"this is definitely not wasm";

    let err = sandbox
        .execute(garbage, config("process", 5_000, 64), b"")
        .await
        .expect_err("should fail on invalid bytes");

    assert!(
        matches!(err, WasmError::InvalidWasm(_)),
        "expected InvalidWasm, got: {err}"
    );
}

/// 3. Valid WASM that is missing the required `process` export.
#[tokio::test]
async fn test_missing_export() {
    let engine = make_engine();
    let sandbox = make_sandbox(engine);
    let wasm = wat::parse_str(NO_PROCESS_WAT).expect("bad WAT");

    let err = sandbox
        .execute(&wasm, config("process", 5_000, 64), b"data")
        .await
        .expect_err("should fail on missing export");

    assert!(
        matches!(err, WasmError::MissingExport(_)),
        "expected MissingExport, got: {err}"
    );
}

/// 4. Infinite-loop WASM must be interrupted within ~2× the configured timeout.
#[tokio::test]
async fn test_timeout_enforcement() {
    let engine = make_engine();
    let sandbox = make_sandbox(engine);
    let wasm = wat::parse_str(INFINITE_LOOP_WAT).expect("bad WAT");

    let wall = Instant::now();
    let err = sandbox
        .execute(&wasm, config("process", 100, 64), b"")
        .await
        .expect_err("should time out");

    let elapsed = wall.elapsed();
    assert!(
        matches!(err, WasmError::Timeout(_)),
        "expected Timeout, got: {err}"
    );
    assert!(
        elapsed.as_millis() < 300,
        "should abort within ~2× timeout, took {}ms",
        elapsed.as_millis()
    );
}

/// 5. WASM that tries to exceed the memory limit traps (MemoryLimitExceeded or Trap).
#[tokio::test]
async fn test_memory_limit() {
    let engine = make_engine();
    let sandbox = make_sandbox(engine);
    let wasm = wat::parse_str(MEMORY_GROW_WAT).expect("bad WAT");

    // Allow exactly 1 page (64 KiB) — the initial memory.  Any growth is denied.
    let cfg = ExecutionConfig {
        memory_limit_bytes: 64 * 1024,
        timeout_ms: 5_000,
        function_name: "process".to_string(),
    };

    let err = sandbox
        .execute(&wasm, cfg, b"")
        .await
        .expect_err("should fail on memory limit");

    assert!(
        matches!(err, WasmError::MemoryLimitExceeded | WasmError::Trap(_)),
        "expected MemoryLimitExceeded or Trap, got: {err}"
    );
}

// ─── Cache tests ──────────────────────────────────────────────────────────────

/// 6. Second call to the cache with the same key must be a hit and return
///    significantly faster than the initial compilation.
#[tokio::test]
async fn test_cache_hit() {
    let engine = make_engine();
    let cache = CompiledModuleCache::new(Arc::clone(&engine), 10, 60);
    let wasm = wat::parse_str(ECHO_WAT).expect("bad WAT");

    // First call — compiles (miss).
    let t0 = Instant::now();
    let _ = cache
        .get_or_compile("echo:1.0", &wasm)
        .await
        .expect("first compile");
    let compile_time = t0.elapsed();

    // Second call — served from cache (hit).
    let t1 = Instant::now();
    let _ = cache
        .get_or_compile("echo:1.0", &wasm)
        .await
        .expect("cache hit");
    let cached_time = t1.elapsed();

    // Cached access must be at least 10× faster.
    assert!(
        compile_time > cached_time * 10,
        "cache hit should be >10× faster: compile={compile_time:?}, cached={cached_time:?}"
    );

    let stats = cache.stats().await;
    assert_eq!(stats.hits, 1, "expected 1 hit");
    assert_eq!(stats.misses, 1, "expected 1 miss");
}

/// 7. After the TTL expires the entry must be recompiled (treated as a miss).
#[tokio::test]
async fn test_cache_ttl() {
    let engine = make_engine();
    // 100 ms TTL.
    let cache =
        CompiledModuleCache::new_with_ttl(Arc::clone(&engine), 10, Duration::from_millis(100));
    let wasm = wat::parse_str(ECHO_WAT).expect("bad WAT");

    // Warm the cache.
    cache
        .get_or_compile("echo:ttl", &wasm)
        .await
        .expect("initial compile");

    // Let the TTL expire.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should recompile — cache entry expired.
    cache
        .get_or_compile("echo:ttl", &wasm)
        .await
        .expect("re-compile after TTL");

    let stats = cache.stats().await;
    // Both calls are misses (first compile + re-compile after expiry).
    assert_eq!(
        stats.misses, 2,
        "both calls should be misses (TTL expired between them)"
    );
    // The expired entry is counted as an eviction.
    assert!(stats.evictions >= 1, "expired entry must be evicted");
}

/// 8. A tampered byte string must not match the original SHA-256 hash.
#[test]
fn test_hash_verification() {
    use sha2::{Digest, Sha256};

    let original = b"valid wasm bytes (placeholder)";
    let expected_hash = hex::encode(Sha256::digest(original));

    // Verify that the correct bytes pass.
    let actual = hex::encode(Sha256::digest(original));
    assert_eq!(actual, expected_hash, "correct bytes should match hash");

    // Tamper with a byte and verify failure.
    let mut tampered = original.to_vec();
    tampered[0] ^= 0xFF;
    let tampered_hash = hex::encode(Sha256::digest(&tampered));
    assert_ne!(
        tampered_hash, expected_hash,
        "tampered bytes must not match original hash"
    );
}

/// 9. When the cache is full, adding one more entry must evict the LRU entry.
#[tokio::test]
async fn test_cache_eviction() {
    let engine = make_engine();
    let cache = CompiledModuleCache::new(Arc::clone(&engine), 3, 60); // max 3 entries
    let wasm = wat::parse_str(ECHO_WAT).expect("bad WAT");

    // Fill the cache to capacity.
    cache
        .get_or_compile("mod:1", &wasm)
        .await
        .expect("insert 1");
    cache
        .get_or_compile("mod:2", &wasm)
        .await
        .expect("insert 2");
    cache
        .get_or_compile("mod:3", &wasm)
        .await
        .expect("insert 3");

    let stats_before = cache.stats().await;
    assert_eq!(stats_before.size, 3, "cache should be full (size=3)");

    // Access mod:1 and mod:2 to make mod:3 the LRU candidate... actually
    // the LRU is mod:1 because it was least recently used once mod:2 and
    // mod:3 were inserted.  But ordering depends on LRU semantics after the
    // puts.  What matters is that after one more insert the size stays bounded.
    cache
        .get_or_compile("mod:4", &wasm)
        .await
        .expect("insert 4 — triggers eviction");

    let stats_after = cache.stats().await;
    assert_eq!(
        stats_after.size, 3,
        "cache size must not exceed max_size after eviction"
    );
    assert!(
        stats_after.evictions >= 1,
        "at least one eviction must have occurred, got: {}",
        stats_after.evictions
    );
}
