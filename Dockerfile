# PAP Orchestrator – Multi-arch Docker image
#
# Build:
#   docker buildx build --platform linux/amd64,linux/arm64 -t pap .
#
# Requires:
#   - crates/pap-orchestrator/.sqlx/ directory with offline query metadata
#     (run: cargo sqlx prepare   from crates/pap-orchestrator/)
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
COPY crates/pap-core/Cargo.toml crates/pap-core/Cargo.toml
COPY crates/pap-orchestrator/Cargo.toml crates/pap-orchestrator/Cargo.toml
COPY crates/pap-orchestrator/build.rs crates/pap-orchestrator/build.rs
COPY crates/pap-orchestrator/proto/ crates/pap-orchestrator/proto/
COPY crates/pap-orchestrator/.sqlx/ crates/pap-orchestrator/.sqlx/
COPY crates/pap-bluebubbles/Cargo.toml crates/pap-bluebubbles/Cargo.toml

# Stub source files – just enough for cargo to resolve the crate graph
RUN mkdir -p crates/pap-core/src && echo "pub fn _stub() {}" > crates/pap-core/src/lib.rs \
    && mkdir -p crates/pap-orchestrator/src && echo "fn main() {}" > crates/pap-orchestrator/src/main.rs \
    && mkdir -p crates/pap-bluebubbles/src && echo "fn main() {}" > crates/pap-bluebubbles/src/main.rs \
    && mkdir -p crates/pap-orchestrator/migrations \
    && mkdir -p crates/pap-orchestrator/templates

RUN cargo build --release --bin pap-orchestrator 2>/dev/null || true
RUN cargo build --release --bin pap-bluebubbles 2>/dev/null || true

# ── Layer 2: compile actual source (only this re-runs on code changes) ───────

# Remove stub source + ALL pap crate fingerprints to force recompilation
RUN rm -rf crates/pap-core/src crates/pap-orchestrator/src crates/pap-bluebubbles/src \
    && rm -rf target/release/.fingerprint/pap-* \
    && rm -rf target/release/.fingerprint/pap_* \
    && rm -rf target/release/deps/pap_* \
    && rm -rf target/release/deps/libpap_* \
    && rm -rf target/release/incremental/pap_*

COPY crates/ crates/

RUN cargo build --release --bin pap-orchestrator

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -g 1000 pap && useradd -u 1000 -g 1000 -m pap

WORKDIR /app

COPY --from=builder /build/target/release/pap-orchestrator /app/pap-orchestrator

# Bake in the default agent definitions (YAML, system prompts, capability containers).
# Override at runtime with: -v ./my-agents:/app/agents
COPY agents/ /app/agents/

RUN mkdir -p /app/data && chown pap:pap /app/data

# The orchestrator needs access to the Docker socket for capability containers,
# so it must run as root or in the docker group. We default to root here;
# use docker group mapping in production for tighter security.
# USER pap:pap

EXPOSE 3000

ENV PAP__SERVER__HOST=0.0.0.0
ENV PAP__SERVER__PORT=3000
ENV PAP__DATABASE__URL=sqlite:///app/data/pap.db?mode=rwc
ENV PAP__AGENTS_DIR=/app/agents

ENTRYPOINT ["/app/pap-orchestrator"]
