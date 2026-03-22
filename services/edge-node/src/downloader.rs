//! HTTP client for fetching WASM modules from the control-plane.

use sha2::{Digest, Sha256};

pub struct ModuleDownloader {
    client: reqwest::Client,
    pub control_plane_url: String,
}

impl ModuleDownloader {
    pub fn new(control_plane_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            control_plane_url: control_plane_url.into(),
        }
    }

    /// Download raw WASM bytes for `name`/`version` from the control-plane.
    ///
    /// Issues `GET {control_plane_url}/modules/{name}/{version}` and returns
    /// the response body bytes.
    pub async fn download(&self, name: &str, version: &str) -> Result<Vec<u8>, anyhow::Error> {
        let url = format!("{}/modules/{}/{}", self.control_plane_url, name, version);
        tracing::debug!(url = %url, "downloading module");

        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "control-plane returned {} for module {name}:{version}",
                resp.status()
            );
        }

        let bytes = resp.bytes().await?.to_vec();
        tracing::debug!(name = %name, version = %version, bytes = bytes.len(), "module downloaded");
        Ok(bytes)
    }

    /// Verify that the SHA-256 hex digest of `bytes` matches `expected_hash`.
    pub fn verify_hash(bytes: &[u8], expected_hash: &str) -> bool {
        let actual = hex::encode(Sha256::digest(bytes));
        actual == expected_hash
    }
}
