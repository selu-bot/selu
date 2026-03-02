# Agent Rating System — Implementation Plan

Rating system that lets selu-core users rate installed agents (1-5 stars).
Ratings are submitted to the selu.bot marketplace API, aggregated, and
displayed both locally and on the public marketplace.

```
selu-core (local)                          selu-site (API)
+-------------------+    POST /api/        +------------------+
| Installed agent   | -- ratings/agents/   | DynamoDB table   |
| "Rate this agent" |    {id}              | selu-agent-      |
| [star picker UI]  | ------------------>  | ratings          |
+-------------------+                      +------------------+
                                                  |
                      GET /api/marketplace/       |
                      agents (includes            v
                      avg_rating,           +------------------+
                      rating_count)         | MarketplaceAgent |
                      <-------------------  | + average_rating  |
                                            | + rating_count   |
                                            +------------------+
```

---

## Phase 1: Backend (selu-site API)

### Task 1.1 — Create DynamoDB ratings table

**File:** `../selu-site/infra/lib/constructs/database.ts`

Add a third table alongside `agentsTable` and `agentVersionsTable`:

```typescript
this.ratingsTable = new dynamodb.Table(this, "AgentRatingsTable", {
  tableName: "selu-agent-ratings",
  partitionKey: { name: "agent_id", type: dynamodb.AttributeType.STRING },
  sortKey: { name: "instance_id", type: dynamodb.AttributeType.STRING },
  billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
  removalPolicy: cdk.RemovalPolicy.RETAIN,
});
```

- `agent_id` (PK) — the marketplace agent ID, e.g. `selu-bot-selu-agent-weather`
- `instance_id` (SK) — unique UUID per selu-core installation (see Task 2.1)
- Additional attributes stored per item: `rating` (Number 1-5), `created_at`, `updated_at`

Export `ratingsTable` from the construct, pass table name through to the
Lambda environment as `RATINGS_TABLE_NAME`, and add the field to
`../selu-site/api/src/config.rs`:

```rust
pub ratings_table_name: String,
// in from_env():
ratings_table_name: env::var("RATINGS_TABLE_NAME")?,
```

Grant the Lambda read/write access to the new table in the CDK API
construct (`../selu-site/infra/lib/constructs/api.ts`).

### Task 1.2 — Data model + repository methods

**File:** `../selu-site/api/src/models/agent.rs`

Add these types:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRating {
    pub agent_id: String,
    pub instance_id: String,
    pub rating: u8, // 1–5
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitRatingRequest {
    pub rating: u8, // 1–5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRatingSummary {
    pub agent_id: String,
    pub average_rating: f64,
    pub rating_count: u32,
}
```

Add to `MarketplaceAgent` (already has `category`):

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub average_rating: Option<f64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub rating_count: Option<u32>,
```

**File:** `../selu-site/api/src/db/dynamo.rs`

Add three repository methods to `AgentRepository`:

```rust
/// Upsert a rating (same instance_id overwrites previous rating).
pub async fn put_rating(&self, rating: &AgentRating) -> AppResult<()>;

/// Query all ratings for an agent (to compute avg + count).
pub async fn list_ratings_for_agent(&self, agent_id: &str) -> AppResult<Vec<AgentRating>>;

/// Get a single rating (for "did this instance already rate?" checks).
pub async fn get_rating(&self, agent_id: &str, instance_id: &str) -> AppResult<Option<AgentRating>>;
```

`put_rating` uses `PutItem`. `list_ratings_for_agent` uses `Query` with
`agent_id` as the key condition. `get_rating` uses `GetItem`.

### Task 1.3 — Rating service logic

**File:** `../selu-site/api/src/services/agent_service.rs`

Add two methods:

```rust
pub async fn submit_rating(
    &self,
    agent_id: &str,
    instance_id: &str,
    rating: u8,
) -> AppResult<AgentRatingSummary> {
    // 1. Validate rating is 1–5
    // 2. Verify agent exists and status == Approved
    // 3. Build AgentRating { agent_id, instance_id, rating, created_at, updated_at }
    // 4. repo.put_rating(&rating)  (upsert semantics)
    // 5. Fetch all ratings -> compute average + count
    // 6. Return AgentRatingSummary
}

pub async fn get_rating_summary(
    &self,
    agent_id: &str,
) -> AppResult<AgentRatingSummary> {
    // 1. repo.list_ratings_for_agent(agent_id)
    // 2. Compute average + count (return 0.0 / 0 if none)
    // 3. Return AgentRatingSummary
}
```

Update `get_marketplace_catalog()` to include ratings on each agent:

```rust
// After building the Vec<MarketplaceAgent>, batch-fetch rating
// summaries. For each agent, query ratings and attach:
//   agent.average_rating = Some(summary.average_rating);
//   agent.rating_count = Some(summary.rating_count);
```

Same for `get_marketplace_agent()`.

### Task 1.4 — API routes

**File:** `../selu-site/api/src/routes/ratings.rs` (new file)

Two endpoints:

```rust
/// POST /api/ratings/agents/{id}
/// Header: X-Instance-Id: <uuid>
/// Body: { "rating": 4 }
/// No Cognito auth — selu-core has no Cognito identity.
pub async fn submit_rating(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SubmitRatingRequest>,
) -> AppResult<Json<AgentRatingSummary>> {
    let instance_id = headers
        .get("x-instance-id")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::BadRequest("Missing X-Instance-Id header".into()))?;
    let summary = state.agent_service
        .submit_rating(&agent_id, instance_id, body.rating)
        .await?;
    Ok(Json(summary))
}

/// GET /api/ratings/agents/{id}
/// Public, no auth.
pub async fn get_rating_summary(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> AppResult<Json<AgentRatingSummary>> {
    let summary = state.agent_service.get_rating_summary(&agent_id).await?;
    Ok(Json(summary))
}
```

**File:** `../selu-site/api/src/routes/mod.rs`

Add `pub mod ratings;`.

**File:** `../selu-site/api/src/main.rs`

Register the routes:

```rust
.route(
    "/api/ratings/agents/{id}",
    get(routes::ratings::get_rating_summary)
        .post(routes::ratings::submit_rating),
)
```

### Testing Phase 1

After deploying, verify with curl:

```bash
# Submit a rating
curl -X POST https://selu.bot/api/ratings/agents/selu-bot-selu-agent-weather \
  -H "Content-Type: application/json" \
  -H "X-Instance-Id: test-instance-001" \
  -d '{"rating": 4}'

# Get summary
curl https://selu.bot/api/ratings/agents/selu-bot-selu-agent-weather
```

---

## Phase 2: selu-core (local)

### Task 2.1 — Stable instance ID

Each selu installation needs a unique, stable identifier so the same
installation can't submit duplicate ratings (but can update its rating).

**File:** `crates/selu-orchestrator/migrations/0016_instance_id.sql`

```sql
CREATE TABLE IF NOT EXISTS instance_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Generate a random UUID-like hex string on first migration run.
-- Subsequent runs are no-ops (INSERT OR IGNORE).
INSERT OR IGNORE INTO instance_meta (key, value)
    VALUES ('instance_id', lower(hex(randomblob(16))));
```

**File:** `crates/selu-orchestrator/src/persistence/db.rs`

Add a helper:

```rust
pub async fn get_instance_id(db: &SqlitePool) -> Result<String> {
    let id = sqlx::query_scalar::<_, String>(
        "SELECT value FROM instance_meta WHERE key = 'instance_id'"
    )
    .fetch_one(db)
    .await?;
    Ok(id)
}
```

### Task 2.2 — Rating submission handler

**File:** `crates/selu-orchestrator/src/web/agents.rs`

Add a form struct and handler:

```rust
#[derive(Deserialize)]
pub struct RateAgentForm {
    pub rating: u8,
}

pub async fn rate_agent(
    State(state): State<AppState>,
    _user: AuthUser,  // require login
    Path(agent_id): Path<String>,
    Form(form): Form<RateAgentForm>,
) -> impl IntoResponse {
    // 1. Verify agent is installed:
    //    SELECT 1 FROM agents WHERE id = ? AND setup_complete = 1
    //    If not installed, redirect with error.

    // 2. Validate rating 1–5.

    // 3. Get instance_id from DB:
    //    persistence::db::get_instance_id(&state.db).await

    // 4. Derive the marketplace API base URL from config.marketplace_url.
    //    config.marketplace_url is "https://selu.bot/api/marketplace/agents".
    //    Strip "/marketplace/agents" to get "https://selu.bot/api".
    //    POST to {base}/ratings/agents/{agent_id}
    //    with headers: Content-Type: application/json, X-Instance-Id: {id}
    //    and body: { "rating": N }

    // 5. Redirect back to /agents with success or error query param.
}
```

**File:** `crates/selu-orchestrator/src/web/mod.rs`

Register the route (inside the admin-only agent management section):

```rust
.route("/agents/{agent_id}/rate", post(agents::rate_agent))
```

### Task 2.3 — Display ratings in the UI

**File:** `crates/selu-orchestrator/src/agents/marketplace.rs`

Update `MarketplaceEntry` to accept optional rating fields from the API:

```rust
#[serde(default)]
pub average_rating: Option<f64>,
#[serde(default)]
pub rating_count: Option<u32>,
```

**File:** `crates/selu-orchestrator/src/web/agents.rs`

Update `MarketplaceAgentView` to carry ratings:

```rust
pub average_rating: Option<f64>,
pub rating_count: Option<u32>,
```

Pass these through when building the view (in the `list_agents` handler,
around line 330-354, where `MarketplaceAgentView` is constructed from
`MarketplaceEntry`):

```rust
MarketplaceAgentView {
    // ... existing fields ...
    average_rating: entry.average_rating,
    rating_count: entry.rating_count,
}
```

**File:** `crates/selu-orchestrator/templates/agents.html`

In the **marketplace agent cards** section, after the description and
before the install button, add:

```html
{% if agent.average_rating.is_some() %}
<div class="flex items-center gap-1 text-sm text-gray-500 mt-1">
  {% for i in 1..=5 %}
    {% if i as f64 <= agent.average_rating.unwrap_or(0.0) %}
      <span class="text-yellow-400">&#9733;</span>  {# filled star #}
    {% else %}
      <span class="text-gray-300">&#9733;</span>     {# empty star #}
    {% endif %}
  {% endfor %}
  <span class="ml-1">({{ agent.rating_count.unwrap_or(0) }})</span>
</div>
{% endif %}
```

In the **installed agents** section, add a rating form for each installed
agent (below the agent info, only for non-bundled agents):

```html
{% if !agent.is_bundled %}
<form method="post" action="{{ base_path }}/agents/{{ agent.id }}/rate"
      class="flex items-center gap-2 mt-2">
  <label class="text-sm text-gray-500">Rate:</label>
  {% for i in 1..=5 %}
    <button type="submit" name="rating" value="{{ i }}"
            class="text-xl hover:text-yellow-400 cursor-pointer
                   {% if i <= current_rating %}text-yellow-400{% else %}text-gray-300{% endif %}">
      &#9733;
    </button>
  {% endfor %}
</form>
{% endif %}
```

### Testing Phase 2

1. Start selu-core locally
2. Install an agent from the marketplace
3. On the `/agents` page, click a star to rate
4. Verify the POST goes to selu.bot and succeeds
5. Refresh the page — the marketplace card should show the updated rating

---

## Phase 3: selu-site frontend (display only)

### Task 3.1 — Update TypeScript types

**File:** `../selu-site/web/src/components/marketplace/types.ts`

Add to `MarketplaceAgent`:

```typescript
average_rating?: number;
rating_count?: number;
```

### Task 3.2 — Show ratings in marketplace components

**File:** `../selu-site/web/src/components/marketplace/AgentCard.tsx`

Between the description and the version/details row, add a star display:

```tsx
{agent.rating_count && agent.rating_count > 0 && (
  <div className="flex items-center gap-1 mt-2">
    {[1, 2, 3, 4, 5].map((star) => (
      <span key={star} className={
        star <= Math.round(agent.average_rating || 0)
          ? "text-yellow-400" : "text-gray-300"
      }>&#9733;</span>
    ))}
    <span className="text-xs text-muted ml-1">
      ({agent.rating_count})
    </span>
  </div>
)}
```

**File:** `../selu-site/web/src/components/marketplace/AgentDetail.tsx`

Show the rating prominently in the detail header (next to version/author).

**File:** `../selu-site/web/src/components/landing/MarketplacePreviewIsland.tsx`

Optionally show rating in the featured agent cards on the home page.

---

## Implementation order

1. **Phase 1** (selu-site backend) — deploy, test with curl
2. **Phase 2** (selu-core) — wire up UI + submission
3. **Phase 3** (selu-site frontend) — display ratings

## Design decisions

| Decision | Rationale |
|---|---|
| No Cognito auth for rating endpoint | selu-core users don't have Cognito accounts. The per-installation `instance_id` prevents duplicates. |
| Upsert semantics (PutItem) | An installation can change its rating. Same PK+SK replaces the old item. |
| Real-time aggregation (query all ratings) | Simple. At small scale (~hundreds of ratings per agent) this is fine. If it gets slow, add a cached `rating_summary` attribute on the agents table. |
| Stars only, no text reviews | Text reviews require moderation. Start with numeric ratings, add text later if needed. |
| `X-Instance-Id` header (not body) | Keeps the request body clean and mirrors auth-header patterns. Easy to set from reqwest. |
