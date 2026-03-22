# Contributing to ferroedge

Thank you for your interest in contributing. This document explains how to build and run the project locally, the code style requirements, and the process for submitting changes.

---

## Running Locally

### Prerequisites

- Rust 1.77+ (`rustup update stable`)
- Docker + Docker Compose (for Jaeger and Prometheus)
- `protoc` protobuf compiler

```bash
# macOS
brew install protobuf

# Ubuntu / Debian
sudo apt-get install -y protobuf-compiler
```

### Start infrastructure

```bash
docker-compose up -d jaeger prometheus
```

### Run the control-plane

```bash
cargo run -p control-plane
# Listens on http://localhost:8080 (HTTP API) and http://localhost:9091/metrics (Prometheus)
```

### Run an edge-node

```bash
NODE_ID=node-1 GRPC_PORT=50051 cargo run -p edge-node
# Registers itself with the control-plane on startup
# gRPC on :50051, Prometheus metrics on :9090
```

### Run the full Docker Compose stack

```bash
make docker-up    # starts control-plane, 2 edge-nodes, Jaeger, Prometheus
make docker-down  # stop and remove containers
```

---

## Code Style

All code must pass `cargo fmt` and `cargo clippy` before merging.

```bash
# Format
cargo fmt --all

# Lint (zero warnings required)
cargo clippy --workspace -- -D warnings
```

CI enforces both checks on every push and pull request.

---

## Running Tests

```bash
# Unit + integration tests
cargo test --workspace

# Criterion benchmarks (wasm-runtime crate)
cargo bench -p wasm-runtime

# Load test (requires running stack)
cargo run -p load-tester -- --tasks 1000 --concurrency 50
```

---

## Adding a New Feature

1. **Open an issue first** for anything non-trivial. Describe the problem and proposed approach before writing code.

2. **Create a branch** from `main`:
   ```bash
   git checkout -b feat/my-feature
   ```

3. **Make your changes.** Keep scope focused — one logical change per PR. If you find an unrelated bug while working, fix it in a separate branch.

4. **Write tests.** New behaviour should be covered by at least one unit test. Integration-level changes should include an integration test in `tests/`.

5. **Update documentation.** If you add or change an API endpoint, update `README.md` → API Reference. If you change a core subsystem, update `SYSTEM_DESIGN.md`.

6. **Run the full check suite:**
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace -- -D warnings
   cargo test --workspace
   cargo build --release --workspace
   ```

7. **Open a PR** against `main`. Fill in the PR template: what changed, why, and how to test it.

---

## PR Requirements

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes (zero warnings)
- [ ] `cargo test --workspace` passes
- [ ] `cargo build --release --workspace` succeeds
- [ ] New public API endpoints documented in README.md
- [ ] Non-trivial changes include a test

PRs that fail CI will not be merged.

---

## Project Structure

```
ferroedge/
├── services/
│   ├── control-plane/   # HTTP REST API + scheduler + module registry
│   ├── edge-node/       # gRPC execution server + WASM sandbox
│   └── load-tester/     # CLI load generator
├── crates/
│   ├── common/          # Shared types and error definitions
│   ├── proto/           # Protobuf definitions + generated gRPC code
│   ├── telemetry/       # OpenTelemetry init + Prometheus metrics
│   └── wasm-runtime/    # Wasmtime engine, sandbox, LRU module cache
└── tests/               # Integration and stress tests
```

---

## Reporting Issues

Please include:
- Rust version (`rustc --version`)
- OS and architecture
- Steps to reproduce
- Actual vs expected behaviour
- Relevant logs (with `RUST_LOG=debug` if possible)
