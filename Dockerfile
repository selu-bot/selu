# PAP Orchestrator – Multi-arch Docker image
#
# Build:
#   docker buildx build --platform linux/amd64,linux/arm64 -t pap .
#
# Requires:
#   - .sqlx/ directory with offline query metadata (run: cargo sqlx prepare)
#   - SQLX_OFFLINE=true is set automatically below
#
# Runtime requirements:
#   - Docker socket mounted (-v /var/run/docker.sock:/var/run/docker.sock)
#   - agents/ directory mounted (-v ./agents:/app/agents)
#   - .env file or environment variables for configuration

# ── Build stage: compile the application ─────────────────────────────────────
FROM rust:1-bookworm AS builder

# Install protobuf compiler (needed by protox/tonic-build at compile time)
# and pkg-config for ring's native dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/pap-core/Cargo.toml crates/pap-core/Cargo.toml
COPY crates/pap-orchestrator/Cargo.toml crates/pap-orchestrator/Cargo.toml
COPY crates/pap-bluebubbles/Cargo.toml crates/pap-bluebubbles/Cargo.toml

# Copy SQLx offline query metadata for compile-time SQL checking
COPY .sqlx/ .sqlx/

ENV SQLX_OFFLINE=true

# Copy full source
COPY crates/ crates/

# Build the orchestrator binary.
# Cargo registry and git checkouts are cached via buildx cache mounts so
# dependencies are not re-downloaded on every build. The target directory is
# also cached so incremental compilation works across builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin pap-orchestrator \
    && cp /build/target/release/pap-orchestrator /usr/local/bin/pap-orchestrator

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN groupadd -g 1000 pap && useradd -u 1000 -g 1000 -m pap

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /usr/local/bin/pap-orchestrator /app/pap-orchestrator

# Default agents directory – mount or copy your agents here
RUN mkdir -p /app/agents && chown pap:pap /app/agents

# Data directory for SQLite database
RUN mkdir -p /app/data && chown pap:pap /app/data

# The orchestrator needs access to the Docker socket for capability containers,
# so it must run as root or in the docker group. We default to root here;
# use docker group mapping in production for tighter security.
# USER pap:pap

EXPOSE 3000

# Default environment – override via .env or -e flags
ENV PAP__SERVER__HOST=0.0.0.0
ENV PAP__SERVER__PORT=3000
ENV PAP__DATABASE__URL=sqlite:///app/data/pap.db?mode=rwc
ENV PAP__AGENTS_DIR=/app/agents

ENTRYPOINT ["/app/pap-orchestrator"]
