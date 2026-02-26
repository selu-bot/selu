# Memory Evolution Plan

Parked for later implementation. Thread work comes first.

## Overview

Two changes to the memory system:

### 1. Short-term Memory Re-scoping (user-wide)

**Current:** Embedding-based semantic recall scoped to `session_id` (= per user+agent pair).
**Target:** Scoped to `user_id` so all agents share recent conversational context.

**Changes:**
- Migration: `ALTER TABLE message_embeddings ADD COLUMN user_id TEXT REFERENCES users(id);`
- Backfill: `UPDATE message_embeddings SET user_id = (SELECT user_id FROM sessions WHERE sessions.id = message_embeddings.session_id);`
- `memory.rs`: `store_message()` accepts and stores `user_id`
- `memory.rs`: `retrieve()` accepts `user_id` instead of `session_id`, queries `WHERE me.user_id = ?`
- `engine.rs`: Pass `user_id` to memory store/retrieve calls (lines 91-101, 188-200)
- `context.rs`: Pass `user_id` to memory retrieval (line 59)

### 2. Long-term Memory (new -- user facts)

Persistent facts about the user (city, children, preferences, etc.) that don't change often.

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS user_facts (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id),
    fact_key    TEXT NOT NULL,
    fact_value  TEXT NOT NULL,
    source      TEXT,            -- "extracted" or "manual"
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(user_id, fact_key)
);
CREATE INDEX IF NOT EXISTS idx_user_facts_user ON user_facts(user_id);
```

**New module:** `agents/facts.rs`

**Components:**
1. **Fact extraction** -- After each agent turn, async LLM call: "Extract persistent facts about the user from this exchange (name, location, family, preferences). Return JSON or empty if none."
2. **Fact storage** -- UPSERT into `user_facts`. Keys like `"location.city"`, `"family.children"`, etc.
3. **Fact injection** -- In `context.rs:build()`, after system prompt, inject: "Known facts about this user: ..."
4. **Fact CRUD** -- Web UI + API for viewing/editing/deleting facts (users can correct wrong extractions)

**Integration points:**
- `engine.rs`: After persisting assistant reply, spawn fact extraction
- `context.rs`: After system prompt (line 51), query and inject user facts
- `web/mod.rs`: Add routes for fact management UI

**Rate limiting:** Start with extracting every turn. Optimize later if API costs are a concern (e.g., every N turns, or only for "personal" conversations).

**Open decision:** Whether to use a structured key-value model or free-form text facts. Key-value is cleaner for dedup/updates but harder to extract reliably. Free-form is easier to extract but harder to deduplicate.
