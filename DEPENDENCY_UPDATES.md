# Dependency refresh — 2026-09-03

All 47 direct Rust registry dependencies (including build dependencies) were
checked against the current crates.io index. `Cargo.lock` now resolves their
latest non-yanked stable releases. Transitive dependencies were refreshed within
the constraints of those packages; incompatible transitive majors were not forced.

## Main upgrades

| Area | Updated versions |
| --- | --- |
| Rust web and templates | Axum 0.8.9, axum-extra 0.12.6, tower-http 0.7.1, Askama 0.16.0 |
| Database and RPC | SQLx/CLI 0.9.0, Tonic 0.14.6, Prost 0.14.4, Protox 0.9.1 |
| Other Rust major/minor migrations | Reqwest 0.13.4, Bollard 0.21.1, Argon2 0.6.0, base64 0.23.1, CEL 0.10.0, cron 0.17.0, Smithy eventstream 0.61.2 |
| WhatsApp bridge | Baileys 6.7.24, Express 5.2.1, Pino 10.3.1; qrcode 1.5.4 already current |
| Browser scripts | Tailwind browser 4.3.3, HTMX 2.0.10, SSE extension 2.2.4, Marked 18.0.11, qrcode-generator 2.0.4 |
| GitHub Actions | checkout v7, cache v6, upload-artifact v7, download-artifact v8, setup-node v7, create-github-app-token v3, setup-buildx v4, login v4, build-push v7 |
| Signing | cosign-installer 4.1.2 and Cosign 3.1.3 |
| Container bases | Debian 13/Trixie for Rust builds and runtime; Node 24 LTS/Alpine for the bridge |

Removed unused `askama_axum` and `reqwest-eventsource` declarations. Migrated
Tonic's Prost integration, Argon2's PHC API, Bollard request/response types,
SQLx 0.9 query safety/nullability, and Reqwest's explicit `query` feature.
Refreshed `.sqlx/` using a fresh database containing all 59 migrations.

The bridge now has a repository lockfile and uses `npm ci` in Docker.
Browser scripts are version-pinned. Tailwind's shared CSS theme and renamed
utilities retain Selu's existing brand colors and light/dark behavior.

## Runtime and development requirements

- Rust 1.94.1+ is required by the updated Smithy dependencies. SQLx requires
  1.94.0+. Install `sqlx-cli` 0.9.0 when regenerating the offline query cache.
- Docker Engine 29.2+ is required: Bollard targets API 1.53.
  See the [Docker API matrix](https://docs.docker.com/reference/api/engine/).
- Tailwind 4 requires Safari 16.4+, Chrome 111+, or Firefox 128+.
  See the [Tailwind upgrade guide](https://tailwindcss.com/docs/upgrade-guide).
- CI's Node 24 actions require Actions Runner 2.327.1+; authenticated Git from
  Docker container actions requires 2.329.0+.
  See [checkout's runtime notes](https://github.com/actions/checkout#whats-new).
- Node 24 is intentionally the latest LTS, rather than the Node 26 Current
  series, following [Node's production guidance](https://nodejs.org/en/about/previous-releases).
- Baileys 7 is still a release candidate, so the bridge stays on stable 6.7.24.
- `serde_yaml` remains on its final, deprecated 0.9.34 release; there is no newer
  stable release of that package. Replacing it with another library is separate work.

## Verification

- `cargo fmt --all -- --check`: passed.
- `SQLX_OFFLINE=true cargo test --workspace --locked`: 158 tests passed.
- SQLx 0.9 `prepare --workspace` and `prepare --workspace --check`: passed
  against an isolated temporary database; the application database was untouched.
- `SQLX_OFFLINE=true cargo check --workspace --all-targets --locked`: passed
  without compilation warnings.
- `npm ci --omit=dev`: passed; npm reported zero known vulnerabilities.
- Bridge smoke tests: Baileys exports, Pino, QR generation, and an isolated
  Express 5 JSON route passed; no WhatsApp connection was made.
- Chrome smoke tests using temporary template fixtures: login/setup at desktop
  and mobile widths, light/dark themes, gradients, migrated utilities, HTMX,
  Markdown, and QR rendering passed with no script or HTTP errors.
- RustSec audit: no known vulnerabilities; one unmaintained transitive package,
  `paste` 1.0.15 through `cel-interpreter` 0.10.0, remains at its latest release.
  See [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436).
- All workflow YAML files parsed successfully and updated action references
  were verified. Workflows and image publishing were not run remotely.

## Remaining limits

- Docker base tags were verified, but the local Docker daemon is stopped.
  Full image builds and live container/capability tests could not run.
- Authenticated end-to-end UI, live LLM, and live WhatsApp flows were not tested.
- `DOCS_IMPACT.yaml` already contained malformed indentation in older entries
  before this task (including root-level entries alongside the `entries:` mapping).
  Those existing entries were preserved. The new dependency entry was added
  under `entries:` and validated separately; the full file still needs repair.
- Existing staged work was preserved. These dependency changes are local;
  nothing was committed, pushed, or deployed.
