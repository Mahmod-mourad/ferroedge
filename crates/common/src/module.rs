use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Metadata for a stored WASM module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleMetadata {
    /// Unique id: UUID v4
    pub id: String,
    /// Human-readable module name (e.g. "image-processor")
    pub name: String,
    /// Semantic version string (e.g. "1.0.0")
    pub version: String,
    /// Size of raw wasm bytes
    pub size_bytes: usize,
    /// Wall-clock time the module was registered
    pub created_at: DateTime<Utc>,
    /// SHA-256 hex digest of the raw wasm bytes
    pub hash: String,
}

/// Errors that can be returned by [`ModuleRegistry`].
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("module '{0}' version '{1}' already exists")]
    AlreadyExists(String, String),

    #[error("module '{0}' version '{1}' not found")]
    NotFound(String, String),

    #[error("invalid wasm bytes: {0}")]
    InvalidWasm(String),
}
