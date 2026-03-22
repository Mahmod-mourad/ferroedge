//! Global Prometheus metrics, initialised exactly once via lazy_static.
//!
//! Call [`init`] once at startup to ensure all metrics are registered with the
//! default registry before the first `/metrics` scrape.

use lazy_static::lazy_static;
use prometheus::{
    register_counter, register_counter_vec, register_gauge, register_gauge_vec,
    register_histogram, Counter, CounterVec, Gauge, GaugeVec, Histogram, HistogramOpts,
};

// ── Task lifecycle counters (control-plane) ────────────────────────────────

lazy_static! {
    pub static ref TASKS_SUBMITTED_TOTAL: Counter = register_counter!(
        "tasks_submitted_total",
        "Total number of tasks submitted via POST /tasks"
    )
    .expect("metric registration failed");

    pub static ref TASKS_SUCCESS_TOTAL: Counter = register_counter!(
        "tasks_success_total",
        "Total number of tasks that completed successfully"
    )
    .expect("metric registration failed");

    pub static ref TASKS_FAILED_TOTAL: Counter = register_counter!(
        "tasks_failed_total",
        "Total number of tasks that failed after all retries"
    )
    .expect("metric registration failed");
}

// ── WASM cache counters (edge-node) ────────────────────────────────────────

lazy_static! {
    pub static ref WASM_CACHE_HITS_TOTAL: Counter = register_counter!(
        "wasm_cache_hits_total",
        "Number of WASM module cache hits"
    )
    .expect("metric registration failed");

    pub static ref WASM_CACHE_MISSES_TOTAL: Counter = register_counter!(
        "wasm_cache_misses_total",
        "Number of WASM module cache misses (requires download + compile)"
    )
    .expect("metric registration failed");
}

// ── Health-check failure counter (control-plane) ──────────────────────────

lazy_static! {
    pub static ref NODE_HEALTH_CHECK_FAILURES: CounterVec = register_counter_vec!(
        "node_health_check_failures_total",
        "Number of failed health-check probes per node",
        &["node_id"]
    )
    .expect("metric registration failed");
}

// ── Execution-duration histograms ─────────────────────────────────────────

lazy_static! {
    /// WASM execution time measured inside the edge-node sandbox.
    pub static ref TASK_EXECUTION_DURATION_MS: Histogram = {
        let opts = HistogramOpts::new(
            "task_execution_duration_ms",
            "WASM task execution duration in milliseconds",
        )
        .buckets(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 5_000.0]);
        register_histogram!(opts).expect("metric registration failed")
    };

    /// End-to-end task duration measured at the control-plane (POST /tasks → result stored).
    pub static ref TASK_E2E_DURATION_MS: Histogram = {
        let opts = HistogramOpts::new(
            "task_e2e_duration_ms",
            "End-to-end task duration from HTTP submission to result stored (ms)",
        )
        .buckets(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 5_000.0]);
        register_histogram!(opts).expect("metric registration failed")
    };
}

// ── Live gauges ────────────────────────────────────────────────────────────

lazy_static! {
    /// Number of WASM tasks currently executing on each node.
    pub static ref ACTIVE_TASKS_GAUGE: GaugeVec = register_gauge_vec!(
        "active_tasks",
        "Number of WASM tasks currently executing",
        &["node_id"]
    )
    .expect("metric registration failed");

    /// Number of healthy edge-nodes seen in the last health-check round.
    pub static ref HEALTHY_NODES_GAUGE: Gauge = register_gauge!(
        "healthy_nodes",
        "Number of edge-nodes that passed the last health check"
    )
    .expect("metric registration failed");

    /// Number of compiled WASM modules currently held in the edge-node LRU cache.
    pub static ref CACHE_SIZE_GAUGE: Gauge = register_gauge!(
        "wasm_cache_size",
        "Number of compiled WASM modules in the edge-node LRU cache"
    )
    .expect("metric registration failed");
}

// ── Initialisation ─────────────────────────────────────────────────────────

/// Force-initialise all lazy_static metrics so they appear in the registry
/// before the first `/metrics` scrape.  Call once at service startup.
pub fn init() {
    let _ = &*TASKS_SUBMITTED_TOTAL;
    let _ = &*TASKS_SUCCESS_TOTAL;
    let _ = &*TASKS_FAILED_TOTAL;
    let _ = &*WASM_CACHE_HITS_TOTAL;
    let _ = &*WASM_CACHE_MISSES_TOTAL;
    let _ = &*NODE_HEALTH_CHECK_FAILURES;
    let _ = &*TASK_EXECUTION_DURATION_MS;
    let _ = &*TASK_E2E_DURATION_MS;
    let _ = &*ACTIVE_TASKS_GAUGE;
    let _ = &*HEALTHY_NODES_GAUGE;
    let _ = &*CACHE_SIZE_GAUGE;
}
