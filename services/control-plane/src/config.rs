use anyhow::{Context, Result};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub http_port: u16,
    /// Prometheus metrics server port (default: 9090).
    pub metrics_port: u16,
    pub rust_log: String,
    pub scheduling_strategy: String,
    /// Public base URL of this control-plane, used to construct `fetch_url`
    /// inside `ModuleRef` when dispatching tasks to edge-nodes.
    /// Env: `SELF_URL` (default: `http://127.0.0.1:{http_port}`)
    pub self_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let http_port = std::env::var("HTTP_PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse::<u16>()
            .context("HTTP_PORT must be a valid port number")?;

        let metrics_port = std::env::var("METRICS_PORT")
            .unwrap_or_else(|_| "9090".to_string())
            .parse::<u16>()
            .context("METRICS_PORT must be a valid port number")?;

        let self_url =
            std::env::var("SELF_URL").unwrap_or_else(|_| format!("http://127.0.0.1:{http_port}"));

        Ok(Config {
            http_port,
            metrics_port,
            rust_log: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            scheduling_strategy: std::env::var("SCHEDULING_STRATEGY")
                .unwrap_or_else(|_| "round_robin".to_string()),
            self_url,
        })
    }
}
