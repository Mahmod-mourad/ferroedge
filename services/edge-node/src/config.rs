use anyhow::{Context, Result};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub node_id: String,
    pub grpc_port: u16,
    pub control_plane_url: String,
    pub rust_log: String,
    pub max_concurrent_tasks: usize,
    /// Port for the metrics HTTP server (default: 9090).
    pub metrics_port: u16,
    /// Maximum number of compiled modules to keep in the LRU cache.
    pub cache_max_size: usize,
    /// Cache TTL in seconds (default: 300 = 5 min).
    pub cache_ttl_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Config {
            node_id: std::env::var("NODE_ID").context("NODE_ID must be set")?,
            grpc_port: std::env::var("GRPC_PORT")
                .unwrap_or_else(|_| "50051".to_string())
                .parse::<u16>()
                .context("GRPC_PORT must be a valid port number")?,
            control_plane_url: std::env::var("CONTROL_PLANE_URL")
                .context("CONTROL_PLANE_URL must be set")?,
            rust_log: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            max_concurrent_tasks: std::env::var("MAX_TASKS")
                .unwrap_or_else(|_| "10".to_string())
                .parse::<usize>()
                .context("MAX_TASKS must be a valid integer")?,
            metrics_port: std::env::var("METRICS_PORT")
                .unwrap_or_else(|_| "9090".to_string())
                .parse::<u16>()
                .context("METRICS_PORT must be a valid port number")?,
            cache_max_size: std::env::var("CACHE_MAX_SIZE")
                .unwrap_or_else(|_| "50".to_string())
                .parse::<usize>()
                .context("CACHE_MAX_SIZE must be a valid integer")?,
            cache_ttl_secs: std::env::var("CACHE_TTL_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse::<u64>()
                .context("CACHE_TTL_SECS must be a valid integer")?,
        })
    }
}
