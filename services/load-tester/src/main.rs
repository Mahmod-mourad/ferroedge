//! Edge platform load tester.
//!
//! Submits tasks to the control-plane at configurable concurrency, tracks
//! latency percentiles, and prints live progress + a final report.
//!
//! Usage:
//!   load-tester --url http://127.0.0.1:8080 --tasks 1000 --concurrency 50 --duration 60

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use clap::Parser;
use reqwest::Client;
use tokio::sync::{Mutex, Semaphore};

// ─── CLI arguments ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "load-tester",
    about = "Edge platform HTTP load tester — submits WASM tasks to the control-plane"
)]
struct Args {
    /// Base URL of the control-plane (e.g. http://127.0.0.1:8080)
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,

    /// Total number of tasks to submit
    #[arg(long, default_value_t = 1000)]
    tasks: usize,

    /// Maximum in-flight requests at any time
    #[arg(long, default_value_t = 50)]
    concurrency: usize,

    /// Stop submitting new tasks after this many seconds (0 = no limit)
    #[arg(long, default_value_t = 60)]
    duration: u64,
}

// ─── Statistics helpers ───────────────────────────────────────────────────────

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p / 100.0) as usize).min(sorted.len() - 1);
    sorted[idx]
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── Compile a minimal echo WASM module at startup ─────────────────────────
    let wasm_bytes = wat::parse_str(
        r#"(module
             (memory (export "memory") 1)
             (func (export "alloc") (param i32) (result i32) i32.const 0)
             (func (export "process") (param i32) (param i32) (result i32)
               local.get 1)
           )"#,
    )?;
    let wasm_b64 = Arc::new(B64.encode(&wasm_bytes));
    let input_b64 = Arc::new(B64.encode(b"load-test-payload"));

    println!("┌─ Load Test Configuration ──────────────────────────────────────┐");
    println!("│  URL          : {:<48}│", args.url);
    println!("│  Tasks        : {:<48}│", args.tasks);
    println!("│  Concurrency  : {:<48}│", args.concurrency);
    println!(
        "│  Duration     : {:<48}│",
        if args.duration == 0 {
            "unlimited".to_string()
        } else {
            format!("{}s", args.duration)
        }
    );
    println!("└────────────────────────────────────────────────────────────────┘");
    println!();

    let client = Arc::new(Client::builder().timeout(Duration::from_secs(30)).build()?);

    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let completed = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let latencies: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(args.tasks)));

    let deadline = if args.duration > 0 {
        Some(Instant::now() + Duration::from_secs(args.duration))
    } else {
        None
    };
    let start = Instant::now();

    // ── Live progress reporter (every 5 s) ────────────────────────────────────
    let completed_r = Arc::clone(&completed);
    let errors_r = Arc::clone(&errors);
    let latencies_r = Arc::clone(&latencies);
    let total = args.tasks;
    let _progress_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_completed = 0usize;
        let mut last_tick = Instant::now();
        loop {
            interval.tick().await;
            let now_done = completed_r.load(Ordering::Relaxed);
            let errs = errors_r.load(Ordering::Relaxed);
            let elapsed = last_tick.elapsed().as_secs_f64().max(0.001);
            let rps = (now_done - last_completed) as f64 / elapsed;
            last_completed = now_done;
            last_tick = Instant::now();

            let (p50, p95) = {
                let lats = latencies_r.lock().await;
                let mut sorted = lats.clone();
                sorted.sort_unstable();
                (percentile(&sorted, 50.0), percentile(&sorted, 95.0))
            };

            println!(
                "Completed: {}/{} | RPS: {:.1} | P50: {}ms | P95: {}ms | Errors: {}",
                now_done, total, rps, p50, p95, errs
            );

            if now_done >= total {
                break;
            }
        }
    });

    // ── Submit tasks ──────────────────────────────────────────────────────────
    let mut handles = Vec::with_capacity(args.tasks);

    for i in 0..args.tasks {
        // Respect duration limit
        if let Some(dl) = deadline {
            if Instant::now() > dl {
                println!("⏱  Duration limit reached after {} tasks submitted.", i);
                break;
            }
        }

        let permit = Arc::clone(&semaphore).acquire_owned().await?;
        let client = Arc::clone(&client);
        let submit_url = format!("{}/tasks", args.url);
        let wasm_b64 = Arc::clone(&wasm_b64);
        let input_b64 = Arc::clone(&input_b64);
        let completed = Arc::clone(&completed);
        let errors = Arc::clone(&errors);
        let latencies = Arc::clone(&latencies);

        let handle = tokio::spawn(async move {
            let _permit = permit; // dropped when this task ends

            let req_start = Instant::now();
            let body = serde_json::json!({
                "wasm_base64":    *wasm_b64,
                "function_name":  "process",
                "input_base64":   *input_b64,
                "timeout_ms":     5000,
                "memory_limit_mb": 64,
            });

            let latency_ms = match client.post(&submit_url).json(&body).send().await {
                Ok(resp) => {
                    let lat = req_start.elapsed().as_millis() as u64;
                    if !resp.status().is_success() {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    lat
                }
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    req_start.elapsed().as_millis() as u64
                }
            };

            latencies.lock().await.push(latency_ms);
            completed.fetch_add(1, Ordering::Relaxed);
        });

        handles.push(handle);
    }

    // Wait for all in-flight requests.
    for h in handles {
        let _ = h.await;
    }

    let total_elapsed = start.elapsed();
    let total_done = completed.load(Ordering::Relaxed);
    let total_errs = errors.load(Ordering::Relaxed);
    let total_success = total_done.saturating_sub(total_errs);
    let throughput = total_done as f64 / total_elapsed.as_secs_f64();

    let mut all_lats = latencies.lock().await.clone();
    all_lats.sort_unstable();

    let p50 = percentile(&all_lats, 50.0);
    let p95 = percentile(&all_lats, 95.0);
    let p99 = percentile(&all_lats, 99.0);
    let max_lat = all_lats.last().copied().unwrap_or(0);

    println!();
    println!("┌─ Final Report ─────────────────────────────────────────────────┐");
    println!(
        "│  Total: {} | Success: {} | Failed: {}",
        total_done, total_success, total_errs
    );
    println!(
        "│  Latency — P50: {}ms | P95: {}ms | P99: {}ms | Max: {}ms",
        p50, p95, p99, max_lat
    );
    println!("│  Throughput: {:.1} req/s", throughput);
    println!("└────────────────────────────────────────────────────────────────┘");

    Ok(())
}
