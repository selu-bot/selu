# Selu – Setup Guide

## Prerequisites

| Tool | Version | Purpose |
|------|---------|---------|
| Rust | 1.87+ | Build the orchestrator |
| Docker | 24+ | Capability containers + containerized deployment |
| SQLx CLI | 0.8 | Offline query metadata for CI builds |
| Git | any | Source control |

### Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Install SQLx CLI

```bash
cargo install sqlx-cli --no-default-features --features sqlite
```

---

## Local Development

### 1. Clone the repository

```bash
git clone git@github.com:selu-bot/selu.git
cd selu
```

### 2. Create a `.env` file

```bash
cp .env.example .env   # then edit with your values
```

Required variables:

```env
# 32-byte base64-encoded key for credential encryption
# Generate: openssl rand -base64 32
SELU__ENCRYPTION_KEY=<base64-key>
```

Defaults that work out of the box (override if needed):

```env
SELU__SERVER__HOST=0.0.0.0
SELU__SERVER__PORT=3000
SELU__DATABASE__URL=sqlite://selu.db?mode=rwc
SELU__MARKETPLACE_URL=https://selu.bot/api/marketplace/agents
SELU__INSTALLED_AGENTS_DIR=./installed_agents
SELU__EGRESS_PROXY_ADDR=0.0.0.0:8888
```

### 3. Run locally

```bash
cargo run --bin selu-orchestrator
```

The web UI is at `http://localhost:3000`. On first launch you'll be guided through initial setup (admin user creation, provider configuration).

### 4. Run tests

```bash
cargo test --workspace
```

---

## Docker Deployment

### Pull a pre-built image

```bash
# Latest release
docker pull ghcr.io/selu-bot/selu:latest

# Specific version
docker pull ghcr.io/selu-bot/selu:1.0.0

# Latest dev build from main
docker pull ghcr.io/selu-bot/selu:dev_20260225_143000
```

### Run with Docker

Selu needs access to the Docker socket (it manages capability containers). The default assistant agent is bundled into the binary. Additional agents (weather, homekit, etc.) are installed at runtime from the marketplace via the web UI:

```bash
docker run -d \
  --name selu \
  -p 3000:3000 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v selu-data:/app/data \
  -v selu-agents:/app/installed_agents \
  -e SELU__ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  ghcr.io/selu-bot/selu:latest
```

#### Volume breakdown

| Mount | Purpose |
|-------|---------|
| `/var/run/docker.sock` | Required -- Selu manages capability containers via the Docker API |
| `/app/installed_agents` | Persistent storage for agents installed from the marketplace |
| `/app/data` | SQLite database (persistent state) |

### Docker Compose (recommended)

Create a `docker-compose.yml`:

```yaml
services:
  selu:
    image: ghcr.io/selu-bot/selu:latest
    ports:
      - "3000:3000"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - selu-data:/app/data
      - selu-agents:/app/installed_agents
    environment:
      - SELU__ENCRYPTION_KEY=${SELU__ENCRYPTION_KEY}
      # Optional: override marketplace URL
      # - SELU__MARKETPLACE_URL=https://selu.bot/api/marketplace/agents
    restart: unless-stopped

volumes:
  selu-data:
  selu-agents:
```

Then:

```bash
docker compose up -d
```

---

## CI/CD

### How it works

The GitHub Actions workflow (`.github/workflows/docker.yml`) builds multi-arch Docker images (linux/amd64 + linux/arm64) and pushes them to the GitHub Container Registry (GHCR).

### Tagging strategy

| Trigger | Image tags |
|---------|------------|
| Push to `main` | `dev_<YYYYMMDD_HHMMSS>` |
| Git tag `release_x.x.x` | `latest` + `x.x.x` |

### Preparing SQLx offline metadata

The Docker build uses `SQLX_OFFLINE=true` so it doesn't need a live database at compile time. The `.sqlx/` directory must contain pre-generated query metadata.

**Whenever you change a `sqlx::query!` call or a migration, regenerate:**

```bash
# Ensure you have a local database with all migrations applied
cargo sqlx database create
cargo sqlx migrate run --source crates/selu-orchestrator/migrations

# Generate offline metadata
cargo sqlx prepare --workspace
```

This creates/updates the `.sqlx/` directory. **Commit it to the repository.**

### Creating a release

```bash
# 1. Create a release branch
git checkout -b release_1.0.0 main

# 2. Bump version in Cargo.toml if desired
#    (workspace.package.version)

# 3. Push the branch
git push -u origin release_1.0.0

# 4. Tag the release
git tag release_1.0.0
git push origin release_1.0.0
```

The GitHub Action triggers on the tag and produces images tagged `1.0.0` and `latest`.

### Repository secrets

No additional secrets are needed. The workflow uses the built-in `GITHUB_TOKEN` which automatically has permission to push to GHCR for the repository.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                   Selu Orchestrator                       │
│  ┌──────────┐  ┌────────────┐  ┌─────────────────────┐  │
│  │ Web UI   │  │ REST API   │  │ Pipe Inbound        │  │
│  │ (Askama) │  │ /api/*     │  │ /api/pipes/:id/in   │  │
│  └────┬─────┘  └─────┬──────┘  └──────────┬──────────┘  │
│       │              │                     │             │
│       └──────────────┴─────────┬───────────┘             │
│                                │                         │
│  ┌─────────────────────────────▼──────────────────────┐  │
│  │              Agent Router & Engine                  │  │
│  │  ┌──────────┐                                      │  │
│  │  │ default  │  + installed agents from marketplace  │  │
│  │  │(bundled) │  (homekit, weather, ...)              │  │
│  │  └────┬─────┘                                      │  │
│  └───────┼────────────────────────────────────────────┘  │
│          │                                               │
│  ┌───────▼──────────────────────────────────────────┐  │
│  │         Capability Engine (Docker + gRPC)          │  │
│  └────────────────────────────────────────────────────┘  │
│                                                          │
│  ┌────────────┐  ┌────────────┐  ┌─────────────────┐    │
│  │  SQLite    │  │ LLM Provs  │  │  Event Bus      │    │
│  │  (sqlx)    │  │ (Bedrock,  │  │  (CEL filters)  │    │
│  │            │  │  Anthropic, │  │                 │    │
│  │            │  │  OpenAI)   │  │                 │    │
│  └────────────┘  └────────────┘  └─────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

---

## Environment Variables Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `SELU__SERVER__HOST` | `0.0.0.0` | Bind address |
| `SELU__SERVER__PORT` | `3000` | HTTP port |
| `SELU__DATABASE__URL` | `sqlite://selu.db?mode=rwc` | SQLite connection string |
| `SELU__MARKETPLACE_URL` | `https://selu.bot/api/marketplace/agents` | Agent marketplace catalogue URL |
| `SELU__INSTALLED_AGENTS_DIR` | `./installed_agents` | Directory for marketplace-installed agents |
| `SELU__ENCRYPTION_KEY` | *(required)* | Base64-encoded 32-byte AES key |
| `SELU__EGRESS_PROXY_ADDR` | `0.0.0.0:8888` | Egress proxy listen address |
| `SELU__MAX_CHAIN_DEPTH` | `3` | Max event chain depth |
