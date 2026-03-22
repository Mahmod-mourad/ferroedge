use anyhow::Result;
use wasmtime::{Config, OptLevel};

/// WasmEngine is a thin, cloneable wrapper around a configured wasmtime::Engine.
///
/// Cloning is cheap — wasmtime::Engine is internally reference-counted.
/// One engine instance should be shared across the process (via Arc<WasmEngine>).
#[derive(Clone)]
pub struct WasmEngine {
    pub(crate) inner: wasmtime::Engine,
}

impl WasmEngine {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config
            .max_wasm_stack(1024 * 1024) // 1 MB
            .cranelift_opt_level(OptLevel::Speed)
            .epoch_interruption(true);

        let engine = wasmtime::Engine::new(&config)?;
        Ok(Self { inner: engine })
    }
}

impl std::fmt::Debug for WasmEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmEngine").finish_non_exhaustive()
    }
}
