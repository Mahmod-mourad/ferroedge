# ferroedge

> A distributed edge computing platform written in Rust, capable of executing WebAssembly workloads across multiple nodes with low-latency scheduling, intelligent caching, and production observability.

[![CI](https://github.com/Mahmod-mourad/ferroedge/actions/workflows/ci.yml/badge.svg)](https://github.com/Mahmod-mourad/ferroedge/actions/workflows/ci.yml)
[![Rust 1.77+](https://img.shields.io/badge/rust-1.77%2B-orange)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## Table of Contents

- [Architecture](#architecture)
- [Features](#features)
- [Tech Stack](#tech-stack)
- [Getting Started](#getting-started)
- [API Reference](#api-reference)
- [Performance](#performance)
- [Design Decisions](#design-decisions)
- [Roadmap](#roadmap)

---

## Architecture

```
                        ┌──────────────────────────────────────────────┐
                        │              Control Plane (HTTP :8080)       │
                        │                                              │
  Client                │  ┌────────────┐   ┌──────────────────────┐  │
    │                   │  │  Scheduler │   │   Module Registry    │  │
    │  POST /tasks       │  │            │   │  (SHA-256 hashed)    │  │
    └──────────────────► │  │ ● round-   │   └──────────────────────┘  │
                        │  │   robin    │                              │
    GET /tasks/:id ◄─── │  │ ● least-   │   ┌──────────────────────┐  │
                        │  │   loaded   │   │   Health Monitor     │  │
                        │  └────────────┘   │  (2s check interval) │  │
                        │        │          └──────────────────────┘  │
                        │        │  gRPC                              │
                        └────────┼───────────────────────────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │                         │
                    ▼                         ▼
         ┌──────────────────┐     ┌──────────────────┐
         │  Edge Node 1     │     │  Edge Node 2     │
         │  gRPC :50051     │     │  gRPC :50052     │
         └──────────────────┘     └──────────────────┘

Edge Node Internals:

  gRPC Request
       │
       ▼
  ┌─────────────────────────────────────────────────┐
  │  gRPC Server                                    │
  │       │                                         │
  │       ▼                                         │
  │  Semaphore (MAX_TASKS slots)                    │
  │       │                                         │
  │       ▼                                         │
  │  Module Cache (LRU + TTL)  ──── miss ──────────►│── HTTP GET ──► Control Plane
  │       │ hit                     Module           │               (module bytes)
  │       ▼                        Downloader        │
  │  WASM Sandbox                                   │
  │  (Wasmtime + ResourceLimiter)                   │
  │       │                                         │
  │       ▼                                         │
  │  ExecuteTaskResponse ◄──────────────────────────┘
  └─────────────────────────────────────────────────┘
```

---

## Features

| Category | Feature |
|---|---|
| Execution | ✅ WASM module execution with Wasmtime sandboxing |
| Execution | ✅ Per-task memory limits (configurable, default 64 MB) |
| Execution | ✅ Per-task timeout enforcement via `tokio::time::timeout` |
| Scheduling | ✅ Round-robin scheduling across healthy nodes |
| Scheduling | ✅ Least-loaded scheduling (considers `active_tasks` vs capacity) |
| Scheduling | ✅ Automatic retry on node failure (up to 3 attempts, 500 ms backoff) |
| Reliability | ✅ Automatic circuit breaker per node (5 failures → open, 30 s cooldown) |
| Reliability | ✅ Background health monitoring with automatic node failover |
| Reliability | ✅ Graceful shutdown (in-flight requests complete before exit) |
| Caching | ✅ LRU cache for compiled `wasmtime::Module` objects (avoids re-compilation) |
| Caching | ✅ TTL-based cache expiry (default 300 s) |
| Caching | ✅ Module pre-warming on upload — zero cold-start for registered modules |
| Networking | ✅ gRPC (Tonic) for control-plane → edge-node communication |
| Networking | ✅ Connection pooling (one persistent HTTP-2 channel per node) |
| Networking | ✅ Batch task submission (up to 100 tasks per request, concurrently dispatched) |
| Observability | ✅ Distributed tracing with OpenTelemetry + Jaeger (W3C `traceparent` propagation) |
| Observability | ✅ Prometheus metrics with Grafana-ready output |
| Observability | ✅ Structured JSON logging with `trace_id` correlation across services |
| API | ✅ Module registry (`POST /modules`, `GET /modules`) with SHA-256 integrity |
| API | ✅ Node self-registration (`POST /nodes/register`) |
| API | ✅ Runtime metrics endpoint (`GET /metrics`) |

---

## Tech Stack

| Component | Technology |
|---|---|
| Language | Rust 1.77+ (edition 2021) |
| Async Runtime | Tokio (full features) |
| HTTP Framework | Axum 0.7 |
| RPC | gRPC via Tonic 0.11 |
| WASM Runtime | Wasmtime 20 (Cranelift backend) |
| Serialization | Serde + Protobuf (prost) |
| Tracing | OpenTelemetry 0.23 + Jaeger (OTLP) |
| Metrics | Prometheus (prometheus crate) |
| Containerization | Docker + Docker Compose |
| Testing | Cargo test + Criterion benchmarks |

---

## Getting Started

### Prerequisites

- **Rust 1.77+** — [rustup.rs](https://rustup.rs)
- **Docker + Docker Compose** — for Jaeger, Prometheus, and containerised services
- **protoc** — protobuf compiler for gRPC code generation

```bash
# macOS
brew install protobuf

# Ubuntu / Debian
sudo apt-get install -y protobuf-compiler
```

### Quick Start

```bash
# 1. Clone
git clone https://github.com/Mahmod-mourad/ferroedge
cd ferroedge

# 2. Configure
cp .env.example .env

# 3. Start the full stack (control-plane + 2 edge-nodes + Jaeger + Prometheus)
make docker-up

# 4. Submit a task using a registered module
curl -s -X POST http://localhost:8080/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "module_name": "echo",
    "module_version": "1.0.0",
    "function_name": "process",
    "input_base64": "aGVsbG8="
  }' | jq .

# 5. Check task result
curl -s http://localhost:8080/tasks/<task_id> | jq .
```

### Observability UIs

| Service | URL |
|---|---|
| Jaeger (traces) | http://localhost:16686 |
| Prometheus (metrics) | http://localhost:9090 |
| Control-plane metrics | http://localhost:9091/metrics |
| Edge-node-1 metrics | http://localhost:9092/metrics |

### Running Locally (without Docker)

```bash
# Terminal 1 — infrastructure
docker-compose up jaeger prometheus

# Terminal 2 — control-plane
make run-control

# Terminal 3 — edge-node
NODE_ID=node-1 GRPC_PORT=50051 make run-edge
```

---

## API Reference

All responses use the envelope:
```json
{ "data": <payload>, "error": null }
{ "data": null, "error": "message" }
```

The `X-Trace-Id` header is echoed on every response (generated if not provided).

---

### Health

**`GET /health`**

```bash
curl http://localhost:8080/health
```

Response `200`:
```json
{ "data": { "status": "ok", "node_count": 2 } }
```

---

### Submit Task

**`POST /tasks`**

Submit a single task. Provide either `wasm_base64` (inline bytes) or `module_name` + `module_version` (registered module).

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `module_name` | string | * | — | Name of a pre-registered module |
| `module_version` | string | * | — | Version of a pre-registered module |
| `wasm_base64` | string | * | — | Base64-encoded raw WASM bytes (alternative to module ref) |
| `function_name` | string | yes | — | WASM export function to call |
| `input_base64` | string | yes | — | Base64-encoded input bytes |
| `timeout_ms` | integer | no | 5000 | Execution timeout in milliseconds |
| `memory_limit_mb` | integer | no | 64 | Memory cap for the WASM sandbox |

*One of `wasm_base64` or (`module_name` + `module_version`) is required.

```bash
# Using a registered module
curl -X POST http://localhost:8080/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "module_name": "echo",
    "module_version": "1.0.0",
    "function_name": "process",
    "input_base64": "aGVsbG8=",
    "timeout_ms": 3000,
    "memory_limit_mb": 32
  }'
```

Response `200`:
```json
{ "data": { "task_id": "550e8400-e29b-41d4-a716-446655440000" } }
```

---

### Get Task Result

**`GET /tasks/:id`**

Poll for task completion. Returns `{ "status": "pending" }` while the task is in-flight.

```bash
curl http://localhost:8080/tasks/550e8400-e29b-41d4-a716-446655440000
```

Response (completed):
```json
{
  "data": {
    "task_id": "550e8400-e29b-41d4-a716-446655440000",
    "node_id": "node-1",
    "success": true,
    "output": [104, 101, 108, 108, 111],
    "execution_time_ms": 1,
    "error": null
  }
}
```

Response (pending):
```json
{ "data": { "status": "pending" } }
```

---

### Batch Submit

**`POST /tasks/batch`**

Submit up to 100 tasks in a single HTTP request. All tasks are dispatched concurrently.

```bash
curl -X POST http://localhost:8080/tasks/batch \
  -H "Content-Type: application/json" \
  -d '{
    "tasks": [
      {
        "module_name": "echo",
        "module_version": "1.0.0",
        "function_name": "process",
        "input_base64": "dGFzazE="
      },
      {
        "module_name": "echo",
        "module_version": "1.0.0",
        "function_name": "process",
        "input_base64": "dGFzazI="
      }
    ]
  }'
```

Response `200`:
```json
{
  "data": {
    "task_ids": [
      "550e8400-e29b-41d4-a716-446655440000",
      "550e8400-e29b-41d4-a716-446655440001"
    ]
  }
}
```

---

### Upload Module

**`POST /modules`**

Register a WASM module. The control-plane stores the bytes, computes a SHA-256 hash, and **proactively warms all healthy edge-nodes** in the background — eliminating cold-start latency for the first execution.

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Module name |
| `version` | string | yes | Semver string |
| `wasm_base64` | string | yes | Base64-encoded WASM bytes |

```bash
curl -X POST http://localhost:8080/modules \
  -H "Content-Type: application/json" \
  -d '{
    "name": "echo",
    "version": "1.0.0",
    "wasm_base64": "<base64-encoded .wasm>"
  }'
```

Response `200`:
```json
{ "data": { "module_id": "echo:1.0.0" } }
```

---

### List Modules

**`GET /modules`**

```bash
curl http://localhost:8080/modules
```

Response `200`:
```json
{
  "data": [
    {
      "id": "echo:1.0.0",
      "name": "echo",
      "version": "1.0.0",
      "hash": "sha256:abc123...",
      "size_bytes": 2048
    }
  ]
}
```

---

### Register Node

**`POST /nodes/register`**

Edge-nodes call this on startup to join the cluster.

```bash
curl -X POST http://localhost:8080/nodes/register \
  -H "Content-Type: application/json" \
  -d '{
    "node_id": "node-1",
    "address": "edge-node-1:50051",
    "http_address": "http://edge-node-1:9090",
    "max_concurrent_tasks": 10
  }'
```

---

### List Nodes

**`GET /nodes`**

```bash
curl http://localhost:8080/nodes
```

---

### Runtime Metrics

**`GET /metrics`**

Returns scheduler state and task counters as JSON (distinct from the Prometheus `/metrics` scrape endpoint).

```bash
curl http://localhost:8080/metrics
```

Response `200`:
```json
{
  "scheduler": {
    "strategy": "round_robin",
    "healthy_nodes": 2,
    "unhealthy_nodes": 0,
    "circuit_breakers": {
      "node-1": "closed",
      "node-2": "closed"
    }
  },
  "tasks": {
    "total_submitted": 1000,
    "total_success": 987,
    "total_failed": 5,
    "retried": 8,
    "queued": 0
  }
}
```

---

## Performance

> Measured on Apple M-series (ARM64, ~3.2 GHz), 16 GB, 2 edge-nodes on localhost.
> Reproduce: `cargo bench -p wasm-runtime` and `cargo run -p load-tester -- --tasks 1000 --concurrency 50`.

### WASM Execution Latency (Criterion)

| Scenario | Mean | Min | Max |
|---|---|---|---|
| Cold execution (cache miss) | ~8–15 ms | ~7 ms | ~20 ms |
| Warm execution (cache hit) | ~0.4 ms | ~0.35 ms | ~0.6 ms |
| 10 concurrent executions | ~4 ms | ~3.5 ms | ~6 ms |
| 1 MB input | ~1.2 ms | ~1.0 ms | ~1.8 ms |

**Warm path is ≥20× faster than cold path** — Cranelift compilation is paid once per module per node.

### End-to-End Load Test (1 000 tasks, 50 concurrent)

| Metric | Single Node | Two Nodes | Scaling Factor |
|---|---|---|---|
| P50 | ~22 ms | ~20 ms | 1.1× |
| P95 | ~87 ms | ~72 ms | 1.2× |
| P99 | ~210 ms | ~155 ms | 1.35× |
| Max | ~890 ms | ~620 ms | 1.4× |
| Throughput | ~45 req/s | ~80 req/s | 1.8× |

### Optimisation Results

| Optimisation | Before | After | Gain |
|---|---|---|---|
| Module pre-warming (first-request P50) | ~12 ms | ~0.4 ms | **30×** |
| Connection pooling (gRPC setup overhead) | ~5–20 ms/req | ~0 ms | **eliminated** |
| Connection pooling (P95 @ 50 concurrent) | ~87 ms | ~62 ms | **1.4×** |
| Batch vs 100 serial POSTs (wall time) | ~2 200 ms | ~180 ms | **12×** |

### Memory Profile (10 concurrent tasks, 2 modules cached)

```
heap_allocated_bytes:        50 MB  (process RSS, Linux only)
wasm_memory_bytes:          640 MB  (64 MB × 10 active stores)
cache_memory_estimate_bytes: 512 KB (raw WASM bytes in cache)
active_stores:                  10  (one per in-flight task)
```

---

## Design Decisions

### Why Rust?

Consistent tail latency matters in edge infrastructure. Rust eliminates GC pauses that would create unpredictable P99 spikes in a Java or Go implementation. The ownership model statically prevents data races in the concurrent scheduling path — an important property when multiple Tokio tasks share `Arc<RwLock<AppState>>`. Zero-cost abstractions mean we pay no performance penalty for the type safety.

**Tradeoff:** Slower initial development velocity compared to Go; steeper learning curve. Justified here because correctness and latency predictability are non-negotiable for execution infrastructure.

### Why Wasmtime over Wasmer?

Wasmtime is the reference implementation of the W3C WebAssembly specification maintained by the Bytecode Alliance, with Cranelift as a production-grade optimising compiler. Its `ResourceLimiter` trait provides a first-class API for per-instance memory and fuel limits — exactly what a multi-tenant execution sandbox requires. The `epoch_interruption` mechanism integrates cleanly with Tokio's async runtime to implement preemptive timeouts without busy-polling.

**Tradeoff:** Wasmer offers a broader plugin ecosystem and supports more backends. If portable WASM compilation targets were a requirement, Wasmer would be worth reconsidering.

### Why gRPC over REST for inter-service communication?

The control-plane → edge-node path is on the hot execution path. gRPC over HTTP-2 gives us: (1) binary framing with protobuf — smaller payloads than JSON, (2) multiplexing — many concurrent RPCs over one TCP connection, (3) a strongly-typed service contract enforced at compile time via `prost`-generated code. The strict schema also makes the internal protocol explicit and self-documenting.

**Tradeoff:** gRPC requires protoc, adds a build step, and is harder to debug with `curl`. REST is used for the external-facing control-plane API because HTTP tooling is universal.

### Why LRU for compiled module caching?

`wasmtime::Module` compilation via Cranelift takes 8–15 ms per cold load — this is the dominant latency on the execution path. Caching compiled modules in memory makes warm execution ~20× faster. LRU eviction bounds memory growth automatically when many distinct modules are deployed. TTL expiry ensures stale compiled code is eventually replaced after a module update.

**Tradeoff:** Compiled modules are large in-memory objects. On nodes with many distinct modules and limited RAM, this creates memory pressure. A production system would add a size-based eviction limit in addition to LRU count.

### Why in-memory state for tasks?

The task store (`HashMap<String, TaskResult>` inside `AppState`) is fast, simple, and sufficient for demonstrating the scheduling and execution logic. It avoids a storage dependency that would add operational complexity.

**Tradeoff:** All task state is lost on control-plane restart, and a single control-plane cannot horizontally scale. The roadmap item for persistent task storage (Redis or PostgreSQL) addresses this directly.

---

## Roadmap

- [ ] **Persistent task queue** — Kafka or Redis Streams for durability and replay
- [ ] **Control-plane HA** — Raft consensus (via `openraft`) to eliminate SPOF
- [ ] **mTLS between services** — mutual TLS for zero-trust internal networking
- [ ] **WASM module signing** — Ed25519 signatures to verify module provenance
- [ ] **Auto-scaling** — scale edge-node replicas based on queue depth
- [ ] **Multi-region** — latency-aware routing across geographic regions
- [ ] **WebAssembly Component Model** — upgrade from core modules to the component model for richer interface types
- [ ] **Persistent module store** — S3-compatible object storage backend for the module registry

---

## License

MIT
