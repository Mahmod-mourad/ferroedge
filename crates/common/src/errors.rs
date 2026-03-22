use thiserror::Error;

#[derive(Debug, Error)]
pub enum EdgeError {
    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("node unavailable: {0}")]
    NodeUnavailable(String),

    #[error("execution timed out")]
    Timeout,

    #[error("invalid wasm: {0}")]
    InvalidWasm(String),

    #[error("network error: {0}")]
    NetworkError(String),
}
