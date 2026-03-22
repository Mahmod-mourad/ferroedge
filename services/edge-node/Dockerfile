# ── Stage 1: builder ──────────────────────────────────────────────────────────
FROM rust:1.77-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

COPY . .

RUN cargo build --release -p edge-node

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 1000 --no-create-home --shell /bin/false app

COPY --from=builder /workspace/target/release/edge-node /usr/local/bin/edge-node

USER 1000

EXPOSE 50051

CMD ["/usr/local/bin/edge-node"]
