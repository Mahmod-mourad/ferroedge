use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::{Linker, Module, ResourceLimiter, Store};

use crate::engine::WasmEngine;

// ─── Public error type ────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum WasmError {
    #[error("Invalid WASM binary: {0}")]
    InvalidWasm(String),

    #[error("Compilation failed: {0}")]
    CompilationFailed(String),

    #[error("Missing export: {0}")]
    MissingExport(String),

    #[error("Execution timeout after {0}ms")]
    Timeout(u64),

    #[error("Memory limit exceeded")]
    MemoryLimitExceeded,

    #[error("Runtime trap: {0}")]
    Trap(String),

    #[error("IO error: {0}")]
    Io(String),
}

// ─── Public config / result types ────────────────────────────────────────────

#[derive(Debug)]
pub struct ExecutionConfig {
    /// Maximum linear memory the WASM module may use, in bytes.
    pub memory_limit_bytes: u64,
    /// Wall-clock budget for execution, in milliseconds.
    pub timeout_ms: u64,
    /// Name of the WASM export to call.
    pub function_name: String,
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub output: Vec<u8>,
    pub execution_time_ms: u64,
    pub memory_used_bytes: u64,
}

// ─── Internal store data / resource limiter ───────────────────────────────────

struct LimitState {
    max_bytes: usize,
    /// Set to true whenever the limiter denied a memory request.
    denied: bool,
}

impl ResourceLimiter for LimitState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> anyhow::Result<bool> {
        if desired > self.max_bytes {
            self.denied = true;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: u32,
        _desired: u32,
        _maximum: Option<u32>,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
}

// ─── Sandbox ──────────────────────────────────────────────────────────────────

pub struct WasmSandbox {
    engine: Arc<WasmEngine>,
}

impl WasmSandbox {
    pub fn new(engine: Arc<WasmEngine>) -> Self {
        Self { engine }
    }

    /// Execute `wasm_bytes` inside a sandboxed environment.
    ///
    /// Validates and compiles the bytes on every call. Use [`execute_compiled`]
    /// together with [`CompiledModuleCache`] to avoid repeated compilation.
    #[tracing::instrument(
        skip(self, wasm_bytes, input),
        fields(function_name = %config.function_name, wasm_size = wasm_bytes.len())
    )]
    pub async fn execute(
        &self,
        wasm_bytes: &[u8],
        config: ExecutionConfig,
        input: &[u8],
    ) -> Result<ExecutionResult, WasmError> {
        tracing::info!("Starting WASM execution from bytes");

        let wasmtime_engine = self.engine.inner.clone();
        let wasm_bytes_owned = wasm_bytes.to_vec();
        let input_owned = input.to_vec();
        let timeout_ms = config.timeout_ms;

        let engine_for_timeout = wasmtime_engine.clone();
        let timeout_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            engine_for_timeout.increment_epoch();
        });

        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            execute_sync(wasmtime_engine, wasm_bytes_owned, config, input_owned)
        })
        .await
        .map_err(|e| WasmError::Io(e.to_string()))?;

        timeout_handle.abort();

        match &result {
            Ok(r) => tracing::info!(
                execution_time_ms = r.execution_time_ms,
                memory_used_bytes = r.memory_used_bytes,
                "Execution completed"
            ),
            Err(e) => tracing::error!(error = %e, "Execution failed"),
        }

        result
    }

    /// Execute a pre-compiled [`Module`] without recompiling from bytes.
    ///
    /// Use this together with [`CompiledModuleCache::get_or_compile`] to avoid
    /// the CPU cost of compilation on every request.
    ///
    /// # Safety note
    /// `wasmtime::Module` is `Send + Sync` in wasmtime ≥1.0. We wrap it in
    /// `Arc` so ownership can be transferred into `spawn_blocking` without a
    /// copy.
    #[tracing::instrument(
        skip(self, module, input),
        fields(function_name = %config.function_name)
    )]
    pub async fn execute_compiled(
        &self,
        module: Arc<Module>,
        config: ExecutionConfig,
        input: &[u8],
    ) -> Result<ExecutionResult, WasmError> {
        tracing::info!("Starting WASM execution from compiled module");

        let wasmtime_engine = self.engine.inner.clone();
        let input_owned = input.to_vec();
        let timeout_ms = config.timeout_ms;

        let engine_for_timeout = wasmtime_engine.clone();
        let timeout_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            engine_for_timeout.increment_epoch();
        });

        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            let _guard = span.enter();
            execute_sync_compiled(wasmtime_engine, module, config, input_owned)
        })
        .await
        .map_err(|e| WasmError::Io(e.to_string()))?;

        timeout_handle.abort();

        match &result {
            Ok(r) => tracing::info!(
                execution_time_ms = r.execution_time_ms,
                memory_used_bytes = r.memory_used_bytes,
                "Execution completed (compiled)"
            ),
            Err(e) => tracing::error!(error = %e, "Execution failed (compiled)"),
        }

        result
    }
}

// ─── Synchronous execution — compile path (runs inside spawn_blocking) ────────

fn execute_sync(
    engine: wasmtime::Engine,
    wasm_bytes: Vec<u8>,
    config: ExecutionConfig,
    input: Vec<u8>,
) -> Result<ExecutionResult, WasmError> {
    // ── 1. Validate ──────────────────────────────────────────────────────────
    Module::validate(&engine, &wasm_bytes).map_err(|e| WasmError::InvalidWasm(e.to_string()))?;

    // ── 2. Compile ───────────────────────────────────────────────────────────
    let compile_start = Instant::now();
    let module = Arc::new(
        Module::new(&engine, &wasm_bytes)
            .map_err(|e| WasmError::CompilationFailed(e.to_string()))?,
    );
    tracing::debug!(
        compile_ms = compile_start.elapsed().as_millis(),
        "Compilation completed"
    );

    execute_sync_compiled(engine, module, config, input)
}

// ─── Synchronous execution — pre-compiled path (runs inside spawn_blocking) ───

fn execute_sync_compiled(
    engine: wasmtime::Engine,
    module: Arc<Module>,
    config: ExecutionConfig,
    input: Vec<u8>,
) -> Result<ExecutionResult, WasmError> {
    let start = Instant::now();
    let timeout_ms = config.timeout_ms;
    let memory_limit_bytes = config.memory_limit_bytes as usize;

    // ── 3. Store with resource limiter ───────────────────────────────────────
    let limit_state = LimitState {
        max_bytes: memory_limit_bytes,
        denied: false,
    };
    let mut store = Store::new(&engine, limit_state);
    store.limiter(|data| data as &mut dyn ResourceLimiter);
    // Interrupt the module after one epoch increment (fired by the timeout task).
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();

    // ── 4. Linker — intentionally empty (no FS, no network) ──────────────────
    let linker: Linker<LimitState> = Linker::new(&engine);

    // ── 5. Instantiate ───────────────────────────────────────────────────────
    let instance = {
        let res = linker.instantiate(&mut store, &module);
        let denied = store.data().denied;
        res.map_err(|e| {
            if denied {
                WasmError::MemoryLimitExceeded
            } else {
                map_trap_error(e, timeout_ms)
            }
        })?
    };

    // ── 6. Required exports ───────────────────────────────────────────────────
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, "alloc")
        .map_err(|_| WasmError::MissingExport("alloc".to_string()))?;

    let process = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, &config.function_name)
        .map_err(|_| WasmError::MissingExport(config.function_name.clone()))?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| WasmError::MissingExport("memory".to_string()))?;

    // ── 7. Write input into WASM linear memory ────────────────────────────────
    let input_len = input.len() as i32;
    let ptr = {
        let res = alloc.call(&mut store, input_len);
        let denied = store.data().denied;
        res.map_err(|e| {
            if denied {
                WasmError::MemoryLimitExceeded
            } else {
                map_trap_error(e, timeout_ms)
            }
        })?
    };

    memory
        .write(&mut store, ptr as usize, &input)
        .map_err(|e| WasmError::Io(e.to_string()))?;

    // ── 8. Call the process function ──────────────────────────────────────────
    let output_len_raw = {
        let res = process.call(&mut store, (ptr, input_len));
        let denied = store.data().denied;
        res.map_err(|e| {
            if denied {
                WasmError::MemoryLimitExceeded
            } else {
                map_trap_error(e, timeout_ms)
            }
        })?
    };

    // ── 9. Read output ────────────────────────────────────────────────────────
    if output_len_raw < 0 {
        return Err(WasmError::Io(format!(
            "process returned negative length: {output_len_raw}"
        )));
    }
    let output_len = output_len_raw as usize;
    let mut output = vec![0u8; output_len];
    if output_len > 0 {
        memory
            .read(&store, ptr as usize, &mut output)
            .map_err(|e| WasmError::Io(e.to_string()))?;
    }

    // ── 10. Metrics ───────────────────────────────────────────────────────────
    let execution_time_ms = start.elapsed().as_millis() as u64;
    let memory_used_bytes = memory.data_size(&store) as u64;

    Ok(ExecutionResult {
        output,
        execution_time_ms,
        memory_used_bytes,
    })
}

// ─── Error mapping helper ─────────────────────────────────────────────────────

fn map_trap_error(e: anyhow::Error, timeout_ms: u64) -> WasmError {
    if let Some(&trap) = e.downcast_ref::<wasmtime::Trap>() {
        return match trap {
            wasmtime::Trap::Interrupt => WasmError::Timeout(timeout_ms),
            other => WasmError::Trap(format!("{other:?}")),
        };
    }
    let msg = e.to_string();
    if msg.contains("interrupt") || msg.contains("Interrupt") {
        WasmError::Timeout(timeout_ms)
    } else {
        WasmError::Trap(msg)
    }
}
