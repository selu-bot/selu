# Selu Orchestrator – Multi-arch Docker image
#
# Build:
#   docker buildx build --platform linux/amd64,linux/arm64 -t selu .
#
# Requires:
#   - crates/selu-orchestrator/.sqlx/ directory with offline query metadata
#     (run: cargo sqlx prepare   from crates/selu-orchestrator/)
#   - SQLX_OFFLINE=true is set automatically below
#
# Runtime requirements:
#   - Docker socket mounted (-v /var/run/docker.sock:/var/run/docker.sock)
#   - .env file or environment variables for configuration
#
# The default agents are baked into the image. Override with a volume mount
# if you want to use custom agents:  -v ./my-agents:/app/agents

# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

ENV SQLX_OFFLINE=true

# ── Layer 1: dependency compilation (cached when Cargo.toml/lock unchanged) ──
#
# Copy only manifests, lockfile, build.rs, proto, .sqlx, and templates – then
# create minimal stub source files so `cargo build` compiles every dependency
# without seeing our real code. This layer is expensive but rarely changes.

COPY Cargo.toml Cargo.lock ./
COPY crates/selu-core/Cargo.toml crates/selu-core/Cargo.toml
COPY crates/selu-orchestrator/Cargo.toml crates/selu-orchestrator/Cargo.toml
COPY crates/selu-orchestrator/build.rs crates/selu-orchestrator/build.rs
COPY crates/selu-orchestrator/proto/ crates/selu-orchestrator/proto/
COPY crates/selu-orchestrator/.sqlx/ crates/selu-orchestrator/.sqlx/

# Stub source files – just enough for cargo to resolve the crate graph
RUN mkdir -p crates/selu-core/src && echo "pub fn _stub() {}" > crates/selu-core/src/lib.rs \
    && mkdir -p crates/selu-orchestrator/src && echo "fn main() {}" > crates/selu-orchestrator/src/main.rs \
    && mkdir -p crates/selu-orchestrator/migrations \
    && mkdir -p crates/selu-orchestrator/templates

RUN cargo build --release --bin selu-orchestrator 2>/dev/null || true

# ── Layer 2: compile actual source (only this re-runs on code changes) ───────

# Remove stub source + ALL selu crate fingerprints to force recompilation
RUN rm -rf crates/selu-core/src crates/selu-orchestrator/src \
    && rm -rf target/release/.fingerprint/selu-* \
    && rm -rf target/release/.fingerprint/selu_* \
    && rm -rf target/release/deps/selu_* \
    && rm -rf target/release/deps/libselu_* \
    && rm -rf target/release/incremental/selu_*

COPY crates/ crates/

RUN cargo build --release --bin selu-orchestrator

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -g 1000 selu && useradd -u 1000 -g 1000 -m selu

WORKDIR /app

COPY --from=builder /build/target/release/selu-orchestrator /app/selu-orchestrator

# Bake in the default agent definitions (YAML, system prompts, capability containers).
# Override at runtime with: -v ./my-agents:/app/agents
COPY agents/ /app/agents/

RUN mkdir -p /app/data && chown selu:selu /app/data

# The orchestrator needs access to the Docker socket for capability containers,
# so it must run as root or in the docker group. We default to root here;
# use docker group mapping in production for tighter security.
# USER selu:selu

EXPOSE 3000

ENV SELU__SERVER__HOST=0.0.0.0
ENV SELU__SERVER__PORT=3000
ENV SELU__DATABASE__URL=sqlite:///app/data/selu.db?mode=rwc
ENV SELU__AGENTS_DIR=/app/agents

ENTRYPOINT ["/app/selu-orchestrator"]
