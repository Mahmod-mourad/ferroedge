pub mod cache;
pub mod engine;
pub mod sandbox;

#[cfg(test)]
mod tests;

pub use cache::{CacheStats, CachedModule, CompiledModuleCache};
pub use engine::WasmEngine;
pub use sandbox::{ExecutionConfig, ExecutionResult, WasmError, WasmSandbox};

use std::sync::Arc;

/// Convenience wrapper that creates a [`WasmSandbox`] and executes the module
/// in a single call.
pub async fn execute_wasm(
    engine: Arc<WasmEngine>,
    wasm_bytes: &[u8],
    function_name: &str,
    input: &[u8],
    timeout_ms: u64,
    memory_limit_mb: u64,
) -> Result<ExecutionResult, WasmError> {
    let sandbox = WasmSandbox::new(engine);
    let config = ExecutionConfig {
        memory_limit_bytes: memory_limit_mb * 1024 * 1024,
        timeout_ms,
        function_name: function_name.to_string(),
    };
    sandbox.execute(wasm_bytes, config, input).await
}
