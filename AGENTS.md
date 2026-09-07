# AGENTS.md — Selu Development Rules

## What is Selu?

Selu is a personal AI agent platform built in Rust. It makes AI agents accessible to non-technical users. The product must always feel simple, warm, and approachable — never like a developer tool.

Read `identity_and_design.md` for the full brand identity. Read `SETUP.md` for architecture and setup.

---

## Core Principles

### 1. Simple — not just for techies

Selu is for people who are not developers. Every feature, every label, every error message must make sense to someone who has never used a terminal.

- Use plain language everywhere. Say "Couldn't sign you in — try again?" not "Authentication failed".
- Avoid jargon in the UI. Technical terms belong in code comments, not in things users see.
- When you add a new page or feature, imagine explaining it to someone who only uses their phone and a browser. If your explanation needs the word "endpoint" or "payload", simplify.
- Error messages must tell the user what happened and what to do next. Never show raw error codes or stack traces in the UI.

### 2. i18n always and everywhere

Every user-visible string must be translatable. We currently support English (en) and German (de). No exceptions.

**How i18n works in Selu:**

The React i18n system lives in `ui/src/i18n.ts`. Shared shell strings live there; feature pages define colocated English and German bundles with `defineTranslations(...)` and read them with `useTranslations(...)`.

**Rules:**

- Never hardcode user-visible text in JSX. Put shared copy in `ui/src/i18n.ts` or define a feature-local bundle.
- Every bundle must provide matching English and German keys before the feature is complete. `defineTranslations` enforces parity at compile time.
- Use `t(key)` for shared strings and `useTranslations(bundle)` for feature copy. Use `useLanguage()` when formatting dates, times, and numbers.
- Never expose raw Rust errors, API codes, secrets, or stack traces. Map failures to warm, actionable copy in both languages.
- English is the fallback when a saved browser language is unsupported.

**Pattern — adding translated feature copy:**

```tsx
const messages = defineTranslations(
  { title: 'My page', search: 'Search…' },
  { title: 'Meine Seite', search: 'Suchen…' },
)

export function MyPage() {
  const copy = useTranslations(messages)
  return <><h1>{copy.title}</h1><input placeholder={copy.search} /></>
}
```

### 3. Secure and flexible

Security is non-negotiable but must never get in the way of usability.

**Authentication:**

- The SPA shell is public so React can render login, setup, and expired-session states; protected data and mutations live behind authenticated JSON APIs.
- Use `ApiPrincipal` for authenticated `/api/v1` handlers and `ApiAdmin` for administrator-only handlers (see `api/auth.rs`).
- Legacy management GET URLs may exist only as compatibility redirects to canonical `/app/*` routes and must use `ApiPrincipal`. Do not add legacy form mutations or HTML renderers.
- Sessions use HttpOnly cookies (`selu_session`) with a 7-day TTL, stored in the `web_sessions` SQLite table.
- Passwords are hashed with Argon2id in `services/auth.rs`.

**Credentials and secrets:**

- All stored secrets (API keys, capability credentials) are encrypted at rest with AES-256-GCM (see `permissions/store.rs`).
- Never log secrets, API keys, or credential values. Not even at `debug` or `trace` level.
- Never include secrets in error messages, API responses, or UI state.
- The encryption key comes from the `SELU__ENCRYPTION_KEY` env var. Never hardcode it.

**Tool policies:**

- Every tool exposed by a capability container is subject to per-user Allow/Ask/Block policies (see `permissions/tool_policy.rs`).
- The secure default is Block — tools without an explicit policy are blocked.
- "Ask" triggers interactive confirmation in web chat or an async approval queue for non-interactive channels.

**Container security:**

- Capability containers run with resource limits (memory, CPU, PIDs).
- Network and filesystem access is controlled per-container via policies (`none`, `allowlist`, `any` for network; `none`, `temp`, `workspace` for filesystem).
- All outbound HTTP from containers goes through the egress proxy.

### 4. Code quality and patterns

**Project structure:**

```
crates/
  selu-core/          — Shared types and errors
  selu-orchestrator/  — Main binary (all application logic)
    src/
      web/            — React shell serving, base-path handling, compatibility GET redirects
      api/            — authenticated, versioned JSON API handlers
      services/       — reusable application operations behind API and runtime callers
      agents/         — Agent loading, routing, sessions, execution engine
      llm/            — LLM provider abstraction (Bedrock, Anthropic, OpenAI, Pico)
      capabilities/   — Docker container lifecycle, gRPC, egress proxy
      permissions/    — Credential encryption, tool policies, approval queue
      events/         — EventBus, CEL filters, subscriptions
      pipes/          — Message transport (inbound webhooks, outbound delivery)
      channels/       — Channel abstraction and routing
    migrations/       — SQLite migrations
ui/                    — React/Vite SPA (all browser UI)
```

**Rust patterns:**

Use `anyhow::Result` for application-level error handling. Use `.context("descriptive message")?` to add context when propagating errors. Use `thiserror` for typed error enums (in `selu-core`).

Axum API handlers follow this shape:

```rust
pub async fn my_handler(
    principal: ApiPrincipal,          // authenticated session
    State(state): State<AppState>,    // shared app state
    Json(input): Json<MyInput>,       // typed JSON, or Path / Query
) -> Response {
    // Return a typed JSON response or the shared plain-language error envelope.
}
```

Keep reusable business operations in `services/`; API handlers should validate transport input, enforce authorization, and map outcomes to stable JSON contracts. The browser UI belongs in `ui/src`, never in Rust templates or inline HTML.

**Database patterns:**

- Use `sqlx::query!` for compile-time checked queries. No ORM, no repository pattern — SQL is inline.
- UUIDs as text primary keys, `datetime('now')` for timestamps.

**CRITICAL — sqlx offline cache (`.sqlx/` directory):**

The `sqlx::query!` macro verifies SQL at compile time against a live database. The Docker build has no database, so it relies on pre-generated query metadata in `.sqlx/`. If this cache is stale, the Docker build fails.

A pre-commit hook (`.githooks/pre-commit`) automatically regenerates the cache when `.rs` or migration files are committed. CI also verifies freshness before building.

Rules for AI agents and developers:
- **After adding, changing, or removing any `sqlx::query!` / `sqlx::query_as!` call, always run `cargo sqlx prepare --workspace` before considering the task complete.** Do not rely solely on the pre-commit hook — run it explicitly.
- After adding or changing a migration file, run `cargo sqlx migrate run --source crates/selu-orchestrator/migrations` first, then `cargo sqlx prepare --workspace`.
- Always commit the `.sqlx/` directory alongside your code changes. Never `.gitignore` it.
- If a build fails with `SQLX_OFFLINE=true but there is no cached data`, it means this step was missed.

**Frontend patterns:**

- Build every browser surface in the React SPA under `ui/src`; do not add Askama, HTMX, inline browser scripts, or legacy form handlers.
- Use TanStack Query for server state and the typed feature API modules for `/api/v1` requests.
- Follow the management pattern: calm overview cards, right-side sheets on desktop, and full-screen sheets on phones.
- Use shared UI components and design tokens. Preserve keyboard access, focus management, screen-reader announcements, reduced motion, and responsive layouts.
- Put every user-visible string in matching English and German translation bundles.

**Background tasks:**

Use `tokio::spawn` for fire-and-forget async work (personality extraction, title generation). Use `tokio::time::interval` in spawned loops for periodic tasks (session cleanup, workspace TTL).

**Testing:**

- Tests are standard Rust `#[cfg(test)]` modules, co-located with source code.
- Run with `cargo test --workspace`.
- When adding new logic (especially parsing, filtering, encryption, routing), add unit tests.

### 5. Documentation impact

External user-facing documentation lives at `docs.selu.bot` (source in `selu-site/docs/`). When code changes affect what users see or what developers build against, the docs must be updated too. Stale docs erode trust faster than missing docs.

**Docs-relevant paths:**

Any change touching these directories is potentially docs-relevant:

- `src/web/` — SPA shell, compatibility redirects, and deployment behavior
- `ui/src/` — all browser pages, interactions, accessibility, and copy
- `src/api/` — REST API endpoints developers call
- `src/agents/` — Agent format, routing, sessions, execution
- `src/capabilities/` — Capability system, manifests, gRPC interface, container lifecycle
- `src/pipes/`, `src/channels/` — Messaging channels (Telegram, iMessage, webhooks, web chat)
- `src/permissions/` — Security model, tool policies, credential management
- `src/llm/` — LLM provider configuration and behavior
- `src/events/` — EventBus, subscriptions, CEL filters
- `migrations/` — Schema changes that imply feature changes
- `.env.example` — Configuration changes
- `agents/` — Agent package format (agent.yaml, agent.md)

**When to add a docs impact entry:**

Ask yourself: "If a user or agent developer read the current docs after this change, would anything be wrong or missing?" If yes, add an entry to `DOCS_IMPACT.yaml`.

Examples that need an entry:
- Adding a new field to `agent.yaml` or `manifest.yaml`
- Changing how a channel is configured
- Adding or removing an LLM provider
- Changing tool policy behavior
- New API endpoints
- New UI pages or significant UI changes
- Changed environment variables

Examples that do NOT need an entry (use `docs-impact: none` label on the PR instead):
- Internal refactors that don't change behavior
- Bug fixes that restore already-documented behavior
- Performance improvements
- Test additions
- Code style changes

**How to add an entry:**

Append to the `DOCS_IMPACT.yaml` file in the repo root. Each entry needs enough context for a docs author (human or AI) to write the update without reading the full diff:

```yaml
- id: 2026-03-03-gpu-resources          # date + short slug, must be unique
  date: 2026-03-03
  pr: 47                                 # PR number (fill in when known)
  area: capabilities                     # general area of the change
  type: changed                          # added | changed | removed | deprecated
  audience: developers                   # users | developers | both
  summary: "Capability containers can now request GPU memory"
  affected_files:
    - crates/selu-orchestrator/src/capabilities/container.rs
    - crates/selu-orchestrator/src/capabilities/manifest.rs
  docs_sections:
    - developer-guide/capabilities/container-guidelines
    - reference/manifest-yaml-schema
  details: |
    The manifest.yaml `resources` block now accepts an optional `gpu_memory`
    field (string, e.g. "1Gi"). When set, the container is scheduled with
    GPU access. Default remains no GPU.
```

Valid `docs_sections` values map to the docs site structure:

- `getting-started/*` — Installation, quick start, first conversation
- `user-guide/channels/*` — Web chat, iMessage, Telegram
- `user-guide/agents/*` — Installing, sessions, subscriptions
- `user-guide/personality` — Personality and memory
- `user-guide/llm-providers/*` — Anthropic, OpenAI, Bedrock, Pico
- `user-guide/security/*` — Credentials, tool policies
- `user-guide/self-hosting/*` — Docker, env vars, updating
- `developer-guide/agent-format/*` — Package structure, agent.yaml, agent.md, routing
- `developer-guide/building-your-first-agent/*` — Tutorial, testing locally
- `developer-guide/capabilities/*` — Manifests, gRPC, containers, examples
- `developer-guide/built-in-tools/*` — emit_event, delegate_to_agent
- `developer-guide/publishing/*` — Marketplace, release pipeline, versioning
- `reference/*` — Schemas, API, proto, env vars

CI will block the PR if docs-relevant files changed but neither `DOCS_IMPACT.yaml` was updated nor the `docs-impact: none` label was added. After merge, a workflow automatically reads new entries and creates a docs update PR in `selu-site`.

---

## Checklist — before you consider a change complete

1. Does the UI make sense to a non-technical person?
2. Are all new user-visible strings in both `en` and `de` translations?
3. Are protected APIs using `ApiPrincipal` or `ApiAdmin`, with no legacy HTML/form route added?
4. Are secrets handled safely (encrypted at rest, never logged, never in error messages)?
5. Format the code using `cargo fmt --all`
6. Does `cargo test --workspace` pass?
7. If you changed any `sqlx::query!` / `sqlx::query_as!` call or migration, did you run `cargo sqlx prepare --workspace` and verify the `.sqlx/` directory is staged? (The pre-commit hook does this automatically, but always run it explicitly too — do not rely on the hook alone.)
8. If there are compile warnings -> fix them! We want to have a clean as debt free as possible codebase.
9. Does this change affect user-facing behavior or developer-facing APIs? If yes, add an entry to `DOCS_IMPACT.yaml`. If not, add the `docs-impact: none` label to the PR. CI enforces this — PRs that touch docs-relevant paths without either will fail.