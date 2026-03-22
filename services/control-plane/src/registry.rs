//! In-memory WASM module registry for the control-plane.
//!
//! Modules are keyed by `"name:version"` and held in memory alongside their
//! raw bytes so the control-plane can serve them to edge-nodes on demand.

use std::collections::HashMap;

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use common::module::{ModuleMetadata, RegistryError};

#[derive(Debug)]
pub struct ModuleRegistry {
    /// key: "name:version"
    modules: HashMap<String, (ModuleMetadata, Vec<u8>)>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    /// Store a WASM module. Returns the metadata (including generated id and
    /// SHA-256 hash). Returns [`RegistryError::AlreadyExists`] if that
    /// name+version combination is already registered.
    pub fn store(
        &mut self,
        name: &str,
        version: &str,
        bytes: Vec<u8>,
    ) -> Result<ModuleMetadata, RegistryError> {
        let key = format!("{name}:{version}");
        if self.modules.contains_key(&key) {
            return Err(RegistryError::AlreadyExists(
                name.to_string(),
                version.to_string(),
            ));
        }

        let hash = hex::encode(Sha256::digest(&bytes));
        let meta = ModuleMetadata {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            version: version.to_string(),
            size_bytes: bytes.len(),
            created_at: Utc::now(),
            hash,
        };

        self.modules.insert(key, (meta.clone(), bytes));
        Ok(meta)
    }

    /// Retrieve the metadata and raw bytes for a module.
    pub fn get(&self, name: &str, version: &str) -> Option<&(ModuleMetadata, Vec<u8>)> {
        self.modules.get(&format!("{name}:{version}"))
    }

    /// List all registered module metadata (sorted by name then version for
    /// deterministic output).
    pub fn list(&self) -> Vec<&ModuleMetadata> {
        let mut items: Vec<&ModuleMetadata> = self.modules.values().map(|(meta, _)| meta).collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
        items
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}
