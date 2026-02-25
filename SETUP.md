# PAP – Setup Guide

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
git clone git@github.com:<your-org>/pap.git
cd pap
```

### 2. Create a `.env` file

```bash
cp .env.example .env   # then edit with your values
```

Required variables:

```env
# 32-byte base64-encoded key for credential encryption
# Generate: openssl rand -base64 32
PAP__ENCRYPTION_KEY=<base64-key>

# Optional: semantic memory (requires OpenAI API key)
# PAP__EMBEDDING__API_KEY=sk-...
```

Defaults that work out of the box (override if needed):

```env
PAP__SERVER__HOST=0.0.0.0
PAP__SERVER__PORT=3000
PAP__DATABASE__URL=sqlite://pap.db?mode=rwc
PAP__AGENTS_DIR=./agents
PAP__EGRESS_PROXY_ADDR=0.0.0.0:8888
```

### 3. Run locally

```bash
cargo run --bin pap-orchestrator
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
docker pull ghcr.io/<your-org>/pap:latest

# Specific version
docker pull ghcr.io/<your-org>/pap:1.0.0

# Latest dev build from main
docker pull ghcr.io/<your-org>/pap:dev_20260225_143000
```

### Run with Docker

PAP needs access to the Docker socket (it manages capability containers). The default agents (default, weather, pim, homekit) are baked into the image -- no mount required to get started:

```bash
docker run -d \
  --name pap \
  -p 3000:3000 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v pap-data:/app/data \
  -e PAP__ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  ghcr.io/<your-org>/pap:latest
```

To use custom agent definitions instead, mount over `/app/agents`:

```bash
docker run -d \
  --name pap \
  -p 3000:3000 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v $(pwd)/agents:/app/agents:ro \
  -v pap-data:/app/data \
  -e PAP__ENCRYPTION_KEY="$(openssl rand -base64 32)" \
  ghcr.io/<your-org>/pap:latest
```

#### Volume breakdown

| Mount | Purpose |
|-------|---------|
| `/var/run/docker.sock` | Required -- PAP manages capability containers via the Docker API |
| `/app/agents` | Optional override -- custom agent definitions (defaults are built-in) |
| `/app/data` | SQLite database (persistent state) |

### Docker Compose (recommended)

Create a `docker-compose.yml`:

```yaml
services:
  pap:
    image: ghcr.io/<your-org>/pap:latest
    ports:
      - "3000:3000"
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - pap-data:/app/data
      # Uncomment to use custom agents instead of the built-in ones:
      # - ./agents:/app/agents:ro
    environment:
      - PAP__ENCRYPTION_KEY=${PAP__ENCRYPTION_KEY}
      - PAP__EMBEDDING__API_KEY=${PAP__EMBEDDING__API_KEY:-}
    restart: unless-stopped

volumes:
  pap-data:
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
cargo sqlx migrate run --source crates/pap-orchestrator/migrations

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
│                   PAP Orchestrator                       │
│  ┌──────────┐  ┌────────────┐  ┌─────────────────────┐  │
│  │ Web UI   │  │ REST API   │  │ Pipe Inbound        │  │
│  │ (Askama) │  │ /api/*     │  │ /api/pipes/:id/in   │  │
│  └────┬─────┘  └─────┬──────┘  └──────────┬──────────┘  │
│       │              │                     │             │
│       └──────────────┴─────────┬───────────┘             │
│                                │                         │
│  ┌─────────────────────────────▼──────────────────────┐  │
│  │              Agent Router & Engine                  │  │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌───────┐ │  │
│  │  │ default  │ │ weather  │ │   pim    │ │homekit│ │  │
│  │  └────┬─────┘ └────┬─────┘ └────┬─────┘ └───┬───┘ │  │
│  └───────┼─────────────┼────────────┼───────────┼─────┘  │
│          │             │            │           │         │
│  ┌───────▼─────────────▼────────────▼───────────▼─────┐  │
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
| `PAP__SERVER__HOST` | `0.0.0.0` | Bind address |
| `PAP__SERVER__PORT` | `3000` | HTTP port |
| `PAP__DATABASE__URL` | `sqlite://pap.db?mode=rwc` | SQLite connection string |
| `PAP__AGENTS_DIR` | `./agents` | Path to agent definitions |
| `PAP__ENCRYPTION_KEY` | *(required)* | Base64-encoded 32-byte AES key |
| `PAP__EGRESS_PROXY_ADDR` | `0.0.0.0:8888` | Egress proxy listen address |
| `PAP__MAX_CHAIN_DEPTH` | `3` | Max event chain depth |
| `PAP__EMBEDDING__API_KEY` | *(empty)* | OpenAI API key for semantic memory |
| `PAP__EMBEDDING__BASE_URL` | `https://api.openai.com` | Embedding API base URL |
| `PAP__EMBEDDING__MODEL` | `text-embedding-3-small` | Embedding model name |
