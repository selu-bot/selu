# Unified conversation migration rollout

## What this change delivers

This change makes the versioned conversation API the only web and iOS chat
contract. The server-rendered `/chat/*` routes and former mobile chat routes
are no longer registered.

- `GET /api/v1/conversations` lists user-owned conversations, newest activity
  first. `?limit=` caps the page; when more exist the response carries
  `next_cursor`, which the client passes back as `?before=` for the next page.
  Schedule threads (`kind: "schedule"`) are included and the SPA groups them
  under "Scheduled runs".
- `POST /api/v1/conversations` creates a conversation on a caller-owned pipe.
- `PATCH /api/v1/conversations/{id}` renames a conversation (`{"title"}`) and
  publishes `conversation.changed`.
- `DELETE /api/v1/conversations/{id}` removes a conversation with its messages,
  runs, events and persisted artifacts. It answers `409
  conversation.run_in_progress` while a run is active and broadcasts a
  transient `conversation.deleted` event to connected clients.
- A scheduled run that writes into a schedule thread publishes
  `conversation.changed` so open chat clients refetch that thread.
- `GET /api/v1/conversations/{id}` returns a single consistent snapshot:
  messages, active/recent runs, and the durable event cursor.
- `POST /api/v1/conversations/{id}/messages` accepts an idempotent text turn.
- `GET /api/v1/events?after=<cursor>` replays persisted events then fans out
  live events to every connected client.
- A conversation run is persisted before execution. A client-generated UUID is
  used as the persisted user-message ID, so optimistic client state can be
  reconciled without matching timestamps or text.
- A tool boundary closes the current assistant text part for v1 consumers.
  This prevents pre-tool acknowledgements such as "I’ll check" being joined
  directly to the post-tool answer.
- The React/Vite SPA is built into the orchestrator Docker image and served at
  `/app/`, including deployments behind `SELU__BASE_PATH`.
- The SPA owns every browser surface. Administrator-only destinations are derived
  from `/api/v1/session`; credentials never enter browser storage.
- The chat UI has responsive navigation, light/dark themes, English/German
  copy, accessible expandable activity/tool details, reduced-motion support,
  optimistic sending, reconnect-aware scrolling, and inline approval cards.
- iOS chat listing and text chat now use v1 directly.
- Live Activity relay records/payloads now accept an optional `run_id`. New
  activity starts and ends can therefore target one execution rather than all
  activity registrations for a conversation.

## Product boundary

Chat is text-only in this release. Attachments, search, and share-sheet chat
handoff have been removed from the chat surface rather than falling back to a
second protocol. Approvals are delivered as v1 events; their durable,
restart-safe workflow is the next API extension.

## Rollout order

1. **Review and merge all three repositories together.** This is an atomic API
   cutover: do not deploy a client against an older orchestrator.
2. **GitHub CI build.** Build and test `selu`, `selu-site`, and `selu-ios` from
   the exact merged revisions. For `selu`, apply migrations to CI's throwaway
   database, run `cargo sqlx prepare --workspace`, and require the checked-in
   `.sqlx/` cache to be current before merge. Do not use a local build as the
   release signal.
3. **Deploy `selu-site` relay first from the Mac mini.** Verify relay health
   and a run-scoped Live Activity notification.
4. **Deploy the orchestrator from the Mac mini.** Startup applies migration
   `0060_conversation_api_foundation.sql`. Back up the SQLite database before
   deploying. Confirm `/api/health`, then authenticate and call the v1 list and
   snapshot endpoints.
5. **Web smoke test.** Open `/app/` through the production reverse proxy and
   test it with the same base path customers use. Verify every canonical SPA
   section and each compatibility GET redirect before testing execution.
6. **Internal/TestFlight iOS build.** Test the v1 chat surface against that
   deployed backend. Because there is no fallback protocol, a failing test is a
   deployment rollback, not a feature-flag change.
7. **Release.** Publish the iOS build and direct all web entry points to
   `/app/`. Add later chat features only through v1 endpoints.

## Required test matrix

Run these in CI and again against the deployed staging instance:

1. One text turn on web and iOS: same user message ID, same final assistant
   message, same run status.
2. Open the same conversation in two browsers and one iPhone: all receive the
   same ordered progress and completion state.
3. Disconnect/reconnect each client during delegation. Resume from its last
   event cursor; no missing, duplicated, or concatenated text.
4. Refresh midway through a tool run. The snapshot plus replay must show one
   current progress state, not a growing stack of active indicators.
5. Open `/app/` through a non-root reverse-proxy base path. Verify its asset
   and API requests stay under that base path after login and refresh.
6. Retry a POST after simulated timeout with the same `client_message_id`.
   Verify one run and one user message; a different ID while a run is active
   must get `409 conversation.run_in_progress`.
7. Start two different conversations concurrently. Their events and Live
   Activities must not cross.
8. Start run A, then run B in the same conversation after A completes. Ending A
   must never dismiss B's Live Activity.
9. Verify UTC timestamps render in the device/browser locale, including the
   screenshot case where web and iOS previously disagreed.
10. Confirm `/chat/*` and the removed `/api/mobile/.../threads` endpoints return
    `404`, so no client silently falls back to the retired protocol.
11. Test the SPA at desktop, tablet, and phone widths in both themes and both
    languages. Verify keyboard focus, Enter/Shift+Enter behavior, native
    disclosures, large touch targets, and reduced-motion mode.
12. Trigger an Ask-policy tool. Verify the approval is visible after refresh,
    another user receives `404` for its ID without invalidating the owner's
    approval, and Allow/Not now resumes the same run.

## Before enabling production v1 chat

- Add v1 upload/attachment and server-side search endpoints. (Thumbs feedback on
  the latest reply exists: `POST /api/v1/conversations/{id}/feedback`.)
- Persist resumable approval commands (not just their audit event) if agent
  runs must survive an orchestrator restart while waiting for a decision.
- Make tool steps and final assistant messages durable typed parts rather than
  token-only events.
- Add contract fixtures and generated TypeScript/Swift API bindings.
- Add notification-outbox persistence and relay authentication before treating
  APNs delivery as production-ready execution signalling.
