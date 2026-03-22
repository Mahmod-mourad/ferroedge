# System Design: Rust Edge Computing Platform

## Problem Statement

Modern applications increasingly need to execute untrusted or user-supplied code at the network edge — close to the data source, with minimal round-trip to a central data centre. Traditional approaches (spawning OS processes, calling a remote Lambda) either lack the isolation guarantees required for multi-tenant execution or introduce too much cold-start latency to be practical for real-time workloads.

WebAssembly provides a compelling middle ground: a compact, sandboxed instruction format that compiles to near-native speed, enforces strict memory isolation, and starts in microseconds once compiled. The challenge is building the *surrounding infrastructure* — scheduling, caching, health management, and observability — that turns a WASM runtime library into a reliable distributed execution platform.

This platform solves that problem. It provides a REST API for task submission, a gRPC-based fleet of edge execution nodes, an intelligent scheduler, and production-grade observability — all written in Rust for predictable low-latency performance.

---

## High-Level Design

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Clients                                     │
│              (HTTP REST, curl, load-tester service)                 │
└─────────────────────┬───────────────────────────────────────────────┘
                      │ HTTP/1.1 + JSON
                      ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      Control Plane  :8080                           │
│                                                                     │
│  ┌───────────┐  ┌─────────────┐  ┌──────────────┐  ┌───────────┐  │
│  │  Axum     │  │  Scheduler  │  │   Module     │  │  Health   │  │
│  │  Router   │  │ round-robin │  │  Registry    │  │  Monitor  │  │
│  │ + Trace   │  │ least-      │  │ (SHA-256)    │  │ (bg task) │  │
│  │  Middleware│  │  loaded     │  │              │  │           │  │
│  └───────────┘  └─────────────┘  └──────────────┘  └───────────┘  │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                       AppState (Arc<RwLock<>>)              │   │
│  │  nodes: HashMap<id, NodeInfo>                               │   │
│  │  tasks: HashMap<id, TaskResult>                             │   │
│  │  circuit_breakers: HashMap<id, CircuitBreaker>              │   │
│  │  connection_pool: Arc<Mutex<NodeConnectionPool>>            │   │
│  │  module_registry: ModuleRegistry                            │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                     │
│  Prometheus metrics  :9091           Jaeger OTLP export :4317      │
└──────────────┬──────────────────────────────────────────────────────┘
               │  gRPC / HTTP-2  (persistent pooled channels)
        ┌──────┴──────┐
        │             │
        ▼             ▼
┌──────────────┐  ┌──────────────┐
│  Edge Node 1 │  │  Edge Node 2 │
│  gRPC :50051 │  │  gRPC :50052 │
│  HTTP  :9090 │  │  HTTP  :9090 │
│              │  │              │
│  Semaphore   │  │  Semaphore   │
│  LRU Cache   │  │  LRU Cache   │
│  WASM Sandbox│  │  WASM Sandbox│
└──────────────┘  └──────────────┘
        │                │
        └────────────────┘
               │
               ▼
  ┌────────────────────────┐
  │  Infrastructure        │
  │  Jaeger   :16686       │
  │  Prometheus :9090      │
  └────────────────────────┘
```

---

## Component Deep Dives

### Control Plane

**Responsibilities**

- Accept task submissions via HTTP REST and dispatch them to edge nodes via gRPC.
- Maintain the authoritative list of registered nodes and their health state.
- Store and serve WASM module bytes with SHA-256 integrity hashes.
- Run the scheduling algorithm and circuit-breaker logic.
- Emit distributed traces (OTLP → Jaeger) and expose Prometheus metrics.

**State Management**

All mutable state lives in a single `AppState` struct wrapped in `Arc<RwLock<AppState>>`. This is shared across all Axum handler tasks. The `RwLock` allows many concurrent readers (e.g. task result lookups, metrics reads) while serialising writers (node registration, task completion). The lock is held for the minimum possible duration — gRPC calls are made *outside* the lock.

**Failure Modes**

| Failure | Behaviour |
|---|---|
| Edge node becomes unreachable | Circuit breaker opens after 5 failures; node excluded from scheduling for 30 s |
| All nodes unhealthy | Task queued for up to 3 × 1 s retries, then failed with error |
| Concurrent write contention | `RwLock` serialises writers; no data corruption possible |
| Control-plane OOM | In-memory task store unbounded; mitigated by Kubernetes memory limit + restart |
| WASM module hash mismatch | Edge node rejects download; task fails with `InvalidWasm` error |

---

### Edge Node

**Execution Lifecycle**

1. gRPC `ExecuteTask` arrives.
2. Extract W3C `traceparent` from metadata → continue distributed trace.
3. Attempt semaphore acquisition (non-blocking check). If all slots taken → return `ResourceExhausted`.
4. Resolve WASM bytes: check LRU cache by `(name, version)` key. On miss: HTTP-fetch from control-plane, compile with Cranelift, insert into cache.
5. Create a `wasmtime::Store` with a `ResourceLimiter` (memory cap) and configure epoch interruption for the timeout.
6. Call the exported WASM function.
7. Release semaphore. Return `ExecuteTaskResponse` with output bytes and execution time.

**WASM Sandboxing Approach**

Each task executes in its own `wasmtime::Store`. Stores are completely isolated — a bug or malicious WASM in one task cannot affect another's memory. Resource limits are enforced at the Wasmtime level:

- **Memory**: `ResourceLimiter` returns an error if the WASM instance requests more than `memory_limit_mb`.
- **Timeout**: `epoch_interruption` combined with a Tokio `timeout` future preempts long-running execution. The epoch counter is incremented by a background thread; the WASM engine checks it at every backward branch.
- **Stack depth**: Engine configured with a 1 MB WASM stack limit.

**Cache Strategy**

`CompiledModuleCache` wraps an `lru::LruCache<String, (Instant, Arc<wasmtime::Module>)>`:

- Key: `name:version` string (or SHA-256 hash for inline bytes).
- Value: compiled `Module` + insertion timestamp.
- On `get`: evict if `now - inserted_at > TTL` before returning.
- On `put`: LRU evicts the least-recently-used entry when capacity is exceeded.
- Thread safety: `Arc<Mutex<CompiledModuleCache>>` shared across concurrent task handlers.

---

### Scheduling Algorithm

The scheduler is invoked on every task dispatch. It operates on the `nodes` map inside `AppState` after filtering out nodes blocked by an open circuit breaker.

**Round-Robin**

```
candidates = healthy_nodes - circuit_open_nodes
idx = scheduler_index % len(candidates)
scheduler_index += 1
return candidates[idx]
```

Best for: homogeneous workloads where all tasks have similar cost. Stateless and O(n) with candidate list size.

**Least-Loaded**

```
candidates = healthy_nodes - circuit_open_nodes
return candidates.min_by(|n| n.active_tasks / n.max_concurrent_tasks)
```

Best for: heterogeneous workloads with variable execution time. Routes new tasks to the node with the most available capacity headroom.

**Tradeoffs**

Round-robin is simpler and has no coordination overhead — it only reads `scheduler_index`. Least-loaded requires reading `active_tasks` from every candidate node, which is accurate only if the health-check loop updates counts promptly. For CPU-bound WASM workloads, least-loaded generally produces better tail latency under uneven load distribution.

---

### Circuit Breaker

**State Machine**

```
                  5 consecutive failures
  ┌──────────┐ ──────────────────────────► ┌──────────┐
  │  Closed  │                             │   Open   │
  │ (normal) │ ◄─────────────────────────  │ (blocked)│
  └──────────┘    1 success in HalfOpen    └──────────┘
       ▲                                        │
       │                                        │ 30 s cooldown elapsed
       │                                        ▼
       │          failure in HalfOpen      ┌──────────┐
       └──────────────────────────────────  │ HalfOpen │
                                           │ (probe)  │
                                           └──────────┘
```

**Tuning Parameters**

| Parameter | Default | Effect |
|---|---|---|
| `failure_threshold` | 5 | Consecutive failures before opening |
| `cooldown` | 30 s | Time in Open state before allowing a probe |

**Failure Detection**

Failures are recorded on: gRPC transport error, non-success `ExecuteTaskResponse`, or `ResourceExhausted`. Successes reset the counter and close the circuit immediately. The circuit breaker is checked *before* each scheduling decision — open-circuit nodes are excluded from the candidate set without making any network call.

---

## Data Flow: Task Execution (Cache Hit)

```
 Client                Control Plane             Edge Node
   │                        │                        │
   │  POST /tasks            │                        │
   │ ──────────────────────► │                        │
   │                         │                        │
   │                    ① Generate task_id            │
   │                    ② Assign trace_id             │
   │                    ③ Increment tasks_submitted   │
   │                         │                        │
   │                    ④ Scheduler selects node      │
   │                       (least-loaded / RR)        │
   │                         │                        │
   │                    ⑤ Inject traceparent          │
   │                       into gRPC metadata         │
   │                         │                        │
   │                         │  gRPC ExecuteTask      │
   │                         │ ──────────────────────► │
   │                         │                        │
   │                         │                   ⑥ Extract traceparent
   │                         │                      → continue trace
   │                         │                        │
   │                         │                   ⑦ Acquire semaphore slot
   │                         │                        │
   │                         │                   ⑧ Cache lookup → HIT
   │                         │                      Arc<Module> returned
   │                         │                        │
   │                         │                   ⑨ Create Store +
   │                         │                      ResourceLimiter
   │                         │                        │
   │                         │                   ⑩ Call WASM function
   │                         │                      (epoch timeout armed)
   │                         │                        │
   │                         │                   ⑪ Release semaphore
   │                         │                        │
   │                         │  ExecuteTaskResponse   │
   │                         │ ◄────────────────────── │
   │                         │                        │
   │                    ⑫ Record success              │
   │                       in circuit breaker         │
   │                    ⑬ Store TaskResult            │
   │                    ⑭ Observe e2e duration        │
   │                    ⑮ Increment tasks_success     │
   │                         │                        │
   │  { task_id }            │                        │
   │ ◄────────────────────── │                        │
   │                         │                        │
   │  GET /tasks/:id         │                        │
   │ ──────────────────────► │                        │
   │                         │                        │
   │  TaskResult             │                        │
   │ ◄────────────────────── │                        │
```

---

## Failure Scenarios

| Failure | Detection | Recovery |
|---|---|---|
| Edge node process crash | Health check loop (2 s interval) marks node unhealthy | Scheduler excludes unhealthy nodes; in-flight tasks on that node are retried on another node (up to 3 attempts) |
| WASM execution timeout | `tokio::time::timeout` wraps the execute call | Returns `ExecuteTaskResponse { success: false, error: "timeout" }`; circuit breaker records failure |
| WASM memory exceeded | Wasmtime `ResourceLimiter` traps the instance | Same as timeout — execution error returned, node not penalised |
| gRPC transport failure | `tonic::Status` error on RPC call | Circuit breaker incremented; task retried on different node with 500 ms backoff |
| Circuit breaker open | `allow_request()` returns false | Node excluded from scheduling; 30 s cooldown; probe attempt in HalfOpen state |
| Module hash mismatch | Edge node computes SHA-256 after download | Download rejected; task fails; control-plane registry authoritative |
| All nodes at capacity | `ResourceExhausted` gRPC status | Control-plane yields 100 ms and retries up to 30 times before failing the task |
| Control-plane OOM | Kubernetes liveness probe | Pod restart; in-memory task state lost (see roadmap: persistent store) |
| Module download timeout | `reqwest` 30 s timeout in `warm_nodes` | Pre-warm silently fails; cold execution falls back to inline module bytes path |

---

## Scaling Strategy

### Horizontal Scaling (Edge Nodes)

Add more edge-node replicas:

1. Start new `edge-node` container with a unique `NODE_ID`.
2. On startup, the node calls `POST /nodes/register` with its gRPC address and HTTP address.
3. The control-plane immediately includes the new node in scheduling decisions.
4. No coordination between nodes is required — each maintains its own independent cache.

Throughput scales roughly linearly with node count up to the point where the control-plane's `RwLock` becomes the bottleneck (observed at ~10 nodes on a single core). Beyond that, shard the control-plane.

### Vertical Scaling (Edge Nodes)

- Increase `MAX_TASKS` (the semaphore limit) to allow more concurrent WASM executions.
- Increase `memory_limit_mb` per task if modules require it.
- Increase the LRU cache size (`CACHE_MAX_SIZE`) to hold more compiled modules.
- Tune `CACHE_TTL_SECS` based on module update frequency.

### Limitations

| Limitation | Root Cause | Planned Fix |
|---|---|---|
| Control-plane is SPOF | Single in-process `AppState` | Raft consensus (openraft); replicated state machine |
| Task state lost on restart | In-memory `HashMap` | Redis or PostgreSQL for task results |
| No auth between services | No mTLS configured | Mutual TLS; service mesh (Linkerd/Istio) |
| Module registry unbounded | In-memory `Vec` | Eviction policy + object storage backend (S3) |
| No backpressure on task queue | Unbounded `tokio::spawn` | Bounded work queue with async back-pressure |

---

## Future Improvements

1. **Persistent task queue** — Kafka topics for task submission; consumer groups per edge node enable natural fan-out and replay.
2. **Control-plane HA** — Raft-based consensus over task assignment metadata. Each control-plane replica is authoritative for a shard of nodes.
3. **mTLS between services** — Issue short-lived certificates via cert-manager; enforce in Tonic server/client config.
4. **WASM module signing** — Ed25519 signatures on module bytes; edge node verifies before compilation.
5. **Auto-scaling** — KEDA `ScaledObject` watching queue depth; scale edge-node `Deployment` replicas up/down.
6. **Multi-region** — Anycast routing to nearest PoP; latency-aware scheduler using node `region` tag.
7. **WebAssembly Component Model** — Upgrade from core modules to WIT-defined interfaces for richer type-safe APIs between host and guest.
8. **Structured audit log** — Immutable append-only log of every execution (task ID, module hash, node ID, duration) for compliance.
