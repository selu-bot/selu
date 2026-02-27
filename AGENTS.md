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

The i18n system lives in `crates/selu-orchestrator/templates/layout.html`. It is client-side JavaScript. All translations are in a single `translations` object with `en` and `de` keys.

**Rules:**

- Never hardcode user-visible text in templates. Always use `data-i18n` attributes for text content and `data-i18n-placeholder` for input placeholders.
- Never hardcode user-visible text in Rust code that ends up in the UI (e.g. template variables rendered as text). If a template variable contains user-visible text, either use a translation key in the template or document why it's an exception.
- When you add a new page or feature, add both `en` and `de` translations to the `translations` object in `layout.html` before considering the feature complete.
- Use dot-separated keys following the existing pattern: `section.element` (e.g., `pipes.title`, `chat.placeholder`, `agents.install`).
- The `t(key)` JavaScript function is available for translations in inline scripts (e.g., in SSE handlers or dynamic JS).
- Always fall back to English if a key is missing in the current language.

**Pattern — adding a translatable element:**

```html
<!-- In the template -->
<h1 data-i18n="mypage.title">My Page Title</h1>
<input data-i18n-placeholder="mypage.search" placeholder="Search..." />
```

```javascript
// In layout.html translations object
en: {
  'mypage.title': 'My Page Title',
  'mypage.search': 'Search...',
},
de: {
  'mypage.title': 'Meine Seite',
  'mypage.search': 'Suchen...',
},
```

The English text in the HTML attribute is the fallback if JS hasn't loaded yet.

**Pattern — translating in JavaScript:**

```javascript
var msg = t('chat.confirm.approved');
element.textContent = t('some.key');
```

### 3. Secure and flexible

Security is non-negotiable but must never get in the way of usability.

**Authentication:**

- All web routes except `/login`, `/logout`, and `/setup` require the `AuthUser` extractor. This is enforced per-handler, not via middleware — so every new handler must include `auth: AuthUser` in its signature.
- Sessions use HttpOnly cookies (`selu_session`) with a 7-day TTL, stored in the `web_sessions` SQLite table.
- Passwords are hashed with Argon2id (see `web/auth.rs`).

**Credentials and secrets:**

- All stored secrets (API keys, capability credentials) are encrypted at rest with AES-256-GCM (see `permissions/store.rs`).
- Never log secrets, API keys, or credential values. Not even at `debug` or `trace` level.
- Never include secrets in error messages or template variables.
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
      web/            — HTML page handlers (Askama templates, server-rendered)
      api/            — JSON REST API handlers
      agents/         — Agent loading, routing, sessions, execution engine
      llm/            — LLM provider abstraction (Bedrock, Anthropic, OpenAI, Ollama)
      capabilities/   — Docker container lifecycle, gRPC, egress proxy
      permissions/    — Credential encryption, tool policies, approval queue
      events/         — EventBus, CEL filters, subscriptions
      pipes/          — Message transport (inbound webhooks, outbound delivery)
      channels/       — Channel abstraction and routing
    templates/        — Askama HTML templates (extends layout.html)
    migrations/       — SQLite migrations
```

**Rust patterns:**

Use `anyhow::Result` for application-level error handling. Use `.context("descriptive message")?` to add context when propagating errors. Use `thiserror` for typed error enums (in `selu-core`).

Axum handlers follow this shape:

```rust
pub async fn my_handler(
    State(state): State<AppState>,  // shared app state
    auth: AuthUser,                 // session authentication (required on protected routes)
    Form(form): Form<MyForm>,      // form data (or Path, Query, Json as needed)
) -> impl IntoResponse {
    // ...
}
```

For templates, use Askama with the existing `layout.html` base:

```rust
#[derive(Template)]
#[template(path = "my_page.html")]
struct MyPageTemplate {
    active_nav: &'static str,  // highlights the current nav item
    // ... page-specific data
}
```

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

**Template patterns:**

- All templates extend `layout.html` using `{% extends "layout.html" %}`.
- Use Tailwind CSS classes for styling (loaded via CDN).
- Use HTMX for dynamic interactions (loaded via CDN). Prefer `hx-get`, `hx-post`, `hx-delete` with `hx-target` and `hx-swap` over custom JavaScript.
- Use `data-i18n` on every user-visible text element (see i18n section above).

**Background tasks:**

Use `tokio::spawn` for fire-and-forget async work (personality extraction, title generation). Use `tokio::time::interval` in spawned loops for periodic tasks (session cleanup, workspace TTL).

**Testing:**

- Tests are standard Rust `#[cfg(test)]` modules, co-located with source code.
- Run with `cargo test --workspace`.
- When adding new logic (especially parsing, filtering, encryption, routing), add unit tests.

---

## Checklist — before you consider a change complete

1. Does the UI make sense to a non-technical person?
2. Are all new user-visible strings in both `en` and `de` translations?
3. Are all new web handlers protected with `AuthUser` (unless they're public)?
4. Are secrets handled safely (encrypted at rest, never logged, never in error messages)?
5. Does `cargo test --workspace` pass?
6. If you changed any `sqlx::query!` / `sqlx::query_as!` call or migration, did you run `cargo sqlx prepare --workspace` and verify the `.sqlx/` directory is staged? (The pre-commit hook does this automatically, but always run it explicitly too — do not rely on the hook alone.)
7. If there are compile warnings -> fix them! We want to have a clean as debt free as possible codebase.