# ── Stage 1: builder ──────────────────────────────────────────────────────────
FROM rust:1.77-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

# Copy the full workspace so Cargo can resolve all members.
COPY . .

RUN cargo build --release -p control-plane

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 1000 --no-create-home --shell /bin/false app

COPY --from=builder /workspace/target/release/control-plane /usr/local/bin/control-plane

USER 1000

EXPOSE 8080

CMD ["/usr/local/bin/control-plane"]
