use anyhow::Result;
use chrono::{Duration, Utc};
use chrono_tz::Tz;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::agents::delegation;
use crate::agents::loader::AgentDefinition;
use crate::capabilities::manifest::CapabilityManifest;
use crate::llm::provider::{ChatMessage, ToolCall};
use sqlx::Row;

fn current_time_context_block(
    timezone: &str,
    user_language: &str,
    now_utc: chrono::DateTime<Utc>,
) -> String {
    let tz: Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
    let now_local = now_utc.with_timezone(&tz);
    let today = now_local.date_naive();
    let yesterday = today - Duration::days(1);
    let tomorrow = today + Duration::days(1);

    let human_format = match user_language {
        "de" => "%A, %-d. %B %Y, %H:%M (%Z)",
        _ => "%A, %B %-d, %Y, %H:%M (%Z)",
    };

    format!(
        "## Current date and time\n\
         Current user timezone: {timezone}\n\
         Current local datetime: {local_human}\n\
         Current local datetime (ISO 8601): {local_iso}\n\
         Today: {today}\n\
         Tomorrow: {tomorrow}\n\
         Yesterday: {yesterday}\n\
         Resolve relative dates like \"today\", \"tomorrow\", and \"yesterday\" \
         strictly from these values.",
        timezone = timezone,
        local_human = now_local.format(human_format),
        local_iso = now_local.to_rfc3339(),
        today = today,
        tomorrow = tomorrow,
        yesterday = yesterday,
    )
}

/// Builds the LLM context window for a turn:
///   [system prompt + capability prompts (own + inlined) + agent registry (delegated only)]
///   [unified BM25 memory (user-scoped, shared across all agents)]
///   [behavioral lessons (agent-scoped, from self-improvement)]
///   [recent N messages from this thread (or session/pipe if no thread)]
pub async fn build(
    db: &SqlitePool,
    agent: &AgentDefinition,
    pipe_id: &str,
    session_id: &str,
    thread_id: Option<&str>,
    user_id: &str,
    user_language: &str,
    _latest_message: &str,
    inlined_manifests: &HashMap<String, CapabilityManifest>,
    delegated_agents: Option<&HashMap<String, Arc<AgentDefinition>>>,
    location_context: Option<&str>,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    // ── System prompt ─────────────────────────────────────────────────────────
    let mut system = agent.localized_system_prompt(user_language).to_string();

    // Append each of the agent's own capability prompts (if non-empty) so the
    // LLM knows what tools are available and how to use them.
    for manifest in agent.capability_manifests.values() {
        let prompt = manifest.localized_prompt(user_language);
        if !prompt.is_empty() {
            system.push_str("\n\n## Capability: ");
            system.push_str(&manifest.id);
            system.push('\n');
            system.push_str(prompt);
        }
    }

    // Append capability prompts from inlined agents — their tools are directly
    // available on this agent's tool list, so the LLM needs usage instructions.
    for manifest in inlined_manifests.values() {
        let prompt = manifest.localized_prompt(user_language);
        if !prompt.is_empty() {
            system.push_str("\n\n## Capability: ");
            system.push_str(&manifest.id);
            system.push('\n');
            system.push_str(prompt);
        }
    }

    // Append agent registry so the orchestrator knows which specialists exist
    // and can use the delegate_to_agent tool effectively.
    // Only delegated agents appear here — inlined agents' tools are already
    // directly available, so they don't need delegation.
    if let Some(agents) = delegated_agents {
        if !agents.is_empty() {
            system.push_str("\n\n");
            system.push_str(&delegation::agent_registry_prompt(&agent.id, agents));
        }
    }

    // ── Current date & time (per-turn, user timezone) ──────────────────────
    // Injected fresh every turn so the agent always knows the current date
    // and time for THIS user — even when users in different timezones send
    // messages at the same server clock time.
    let user_timezone = sqlx::query_scalar::<_, String>("SELECT timezone FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .filter(|tz| !tz.trim().is_empty())
        .unwrap_or_else(|| "UTC".to_string());
    system.push_str("\n\n");
    system.push_str(&current_time_context_block(
        &user_timezone,
        user_language,
        Utc::now(),
    ));

    // ── User location (mobile only) ──────────────────────────────────────────
    if let Some(loc) = location_context {
        system.push_str("\n\n## Current user location\n");
        system.push_str(loc);
    }

    // ── User language preference ──────────────────────────────────────────────
    // Instruct the model to respond in the user's preferred language.
    let lang_name = match user_language {
        "de" => "German (Deutsch)",
        "en" => "English",
        _ => "German (Deutsch)",
    };
    system.push_str(&format!(
        "\n\n## Response language\nAlways reply in {}. This is the user's preferred language.",
        lang_name
    ));

    // ── Slash commands ────────────────────────────────────────────────────────
    // Tell the agent about available slash commands and scheduling tools.
    system.push_str(
        "\n\n## System commands\n\
         The following slash commands are available to the user. \
         You also have built-in scheduling tools and should prefer using them \
         directly when users ask in natural language.\n\n\
         - `/schedule add <what to do + when>` — Create a recurring schedule \
         (e.g. `/schedule add Give me a morning summary at 06:45 from monday to friday`)\n\
         - `/schedule list` — List all schedules and reminders\n\
         - `/schedule delete <name>` — Delete a schedule or reminder by name\n\
         - `/remind <what to do + when>` — Set a one-time reminder \
         (e.g. `/remind Check the weather and email about grilling next Sunday morning`)\n\n\
         Built-in scheduling tools:\n\
         - `set_schedule` — use for recurring requests (daily, weekdays, every Monday, etc.)\n\
         - `set_reminder` — use for one-time future requests (tomorrow, next Sunday, in 2 hours, etc.)\n\n\
         For `set_reminder`: when the user gives a local time, pass `fire_at` as a local datetime \
         without a timezone suffix (e.g. `2026-03-04T11:10:00`). The system will interpret it in the \
         user's timezone. \
         Prefer these tools over asking users to type slash commands, unless the user explicitly \
         asks for command syntax. \
         Do NOT try to implement scheduling yourself using store_set, memory_remember, or emit_event — \
         the system has built-in scheduling and reminders.\n\n\
         Users can also manage schedules from the Schedules page in the web interface.",
    );

    system.push_str(
        "\n\n## Long-term memory tools\n\
         Use `store_*` for keyed, mutable state you'll look up by exact key \
         (e.g. task checkpoints, sync timestamps, workflow state). \
         Use `memory_*` for prose notes about the user or project that should surface contextually in future conversations.\n\n\
         Memories are shared across all agents — anything you remember is available to every agent \
         that works with this user. The system also automatically remembers key facts about users \
         from conversations.\n\n\
         Use `memory_remember` to save information that may help in future interactions. \
         Examples: recurring project context, preferences, and stable workflow notes. \
         Use `memory_search` or `memory_list` when you need to inspect memory explicitly, and `memory_forget` when asked to remove a memory.",
    );

    system.push_str(
        "\n\n## Tool approval copy\n\
         When you call a tool, also include `_approval_message` in the tool arguments. \
         If you want to provide locale-specific approval copy, include `_approval_message_i18n` \
         as an object keyed by locale codes such as `en` and `de`. \
         This must be a short, plain-language sentence describing what you want to do \
         from the user's perspective (no JSON, no field names, no technical wording). \
         Example (German): `Ich möchte den Artikel Spülmaschinensalz von deiner Liste entfernen.` \
         Keep it under 140 characters.",
    );

    // ── Current conversation channel ─────────────────────────────────────────
    // Tell the agent which pipe/channel this conversation is happening on so
    // it can distinguish "send it here" (= reply in chat) from "send via email".
    let pipe_meta = sqlx::query!(
        "SELECT name, transport FROM pipes WHERE id = ? LIMIT 1",
        pipe_id
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();

    if let Some(ref meta) = pipe_meta {
        let transport_label = match meta.transport.as_str() {
            "imessage" => "iMessage",
            "telegram" => "Telegram",
            "whatsapp" => "WhatsApp",
            "web" => "Web-Chat",
            "mobile" => "Mobile-App",
            _ => &meta.transport,
        };
        system.push_str(&format!(
            "\n\n## Current conversation channel\n\
             You are talking to the user via: **{name}** ({transport}).\n\
             Your reply — including any artifacts or attachments — will be delivered \
             directly to this channel automatically.\n\
             - When the user says \"send it here\", \"schick es mir\", \"schick es hier hin\", \
             or similar **without naming a specific delivery method**, they mean this channel. \
             Simply include the artifact in your reply — do NOT delegate to email or another agent.\n\
             - When the user **explicitly** requests a specific delivery method — e.g. \
             \"per E-Mail\", \"send by email\", \"save to disk\", \"speicher es\" — delegate \
             to the appropriate agent (pim-icloud for email, file system agent, etc.) as usual.",
            name = meta.name,
            transport = transport_label,
        ));
    }

    // ── Multi-step completion rules ─────────────────────────────────────────
    // The instruction varies depending on whether this agent is the
    // orchestrator (has delegate_to_agent) or a specialist sub-agent.
    // Sub-agents should focus on their part and return — the orchestrator
    // is responsible for chaining steps across agents.
    let has_delegate = delegated_agents.map_or(false, |m| !m.is_empty());

    if has_delegate {
        // Orchestrator: can chain delegations to complete multi-step tasks.
        system.push_str(
            "\n\n## Multi-step completion rules\n\
             If the user asks for a multi-step outcome, you must complete all requested steps \
             before considering the task done.\n\
             When delegating a multi-step task, delegate **one step at a time**. For example, first \
             delegate artifact creation, then — once the artifact is returned — delegate delivery or \
             the next step as a separate delegation, passing along any artifact references.\n\
             Do not send the entire multi-step request to a single specialist.\n\
             Only use chat-only fallback when no suitable capability is available, and say that clearly.\n\
             After the user already confirmed an action in the current thread, do not ask the same \
             confirmation question again.",
        );
    } else {
        // Sub-agent: focus on your specialty and return.  The orchestrator
        // will handle subsequent steps (delivery, storage, etc.).
        system.push_str(
            "\n\n## Task completion rules\n\
             Focus on completing your part of the task with the tools available to you.\n\
             Once you have created the requested artifact or completed your step, return your \
             result immediately. Include any artifact references in your reply.\n\
             If the original request includes steps you cannot perform with your available tools, \
             do NOT retry or loop — simply return what you have accomplished and briefly note \
             what remains. The orchestrator will handle the rest.",
        );
    }

    messages.push(ChatMessage::system(system));

    // ── User profile (always-on, unconditional) ────────────────────────────────
    // All profile facts are injected into every turn — no BM25 gate, no search.
    // The agent always knows who the user is.
    match super::profile::build_context_block(db, user_id).await {
        Ok(Some(ctx)) => {
            messages.push(ChatMessage::system(ctx));
        }
        Ok(None) => {}
        Err(e) => {
            debug!("Profile loading failed (non-fatal): {e}");
        }
    }

    // ── Behavioral lessons (self-improvement) ─────────────────────────────────
    match super::improvement::build_context_block(db, &agent.id, user_id).await {
        Ok(Some(ctx)) => {
            messages.push(ChatMessage::system(ctx));
        }
        Ok(None) => {}
        Err(e) => {
            debug!("Behavioral lessons loading failed (non-fatal): {e}");
        }
    }

    // ── Conversation history ──────────────────────────────────────────────────
    // If a thread_id is provided, pull messages for that specific thread.
    // This gives each thread its own isolated conversation history while
    // sharing personality knowledge across all sessions.
    // If no thread_id (web chat, delegation), fall back to pipe+session history.
    let history = if let Some(tid) = thread_id {
        sqlx::query(
            "SELECT role, content, tool_call_id, tool_calls_json FROM messages
             WHERE thread_id = ? AND pipe_id = ? AND compacted = 0
             ORDER BY created_at DESC, rowid DESC
             LIMIT 50",
        )
        .bind(tid)
        .bind(pipe_id)
        .fetch_all(db)
        .await?
    } else {
        sqlx::query(
            "SELECT role, content, tool_call_id, tool_calls_json FROM messages
             WHERE pipe_id = ? AND (session_id = ? OR session_id IS NULL)
               AND thread_id IS NULL AND compacted = 0
             ORDER BY created_at DESC, rowid DESC
             LIMIT 50",
        )
        .bind(pipe_id)
        .bind(session_id)
        .fetch_all(db)
        .await?
    };

    // First pass: collect history messages in chronological order.
    let mut history_msgs: Vec<ChatMessage> = Vec::new();
    for row in history.into_iter().rev() {
        let role: &str = row.get("role");
        let content: &str = row.get("content");
        let tool_call_id: Option<&str> = row.get("tool_call_id");
        let tool_calls_json: Option<&str> = row.get("tool_calls_json");

        match role {
            "user" => history_msgs.push(ChatMessage::user(content)),
            "assistant" => {
                if let Some(tc_json) = tool_calls_json {
                    if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(tc_json) {
                        history_msgs.push(ChatMessage::assistant_with_tool_calls(content, calls));
                        continue;
                    }
                }
                history_msgs.push(ChatMessage::assistant(content));
            }
            "tool" => {
                if let Some(tc_id) = tool_call_id {
                    history_msgs.push(ChatMessage::tool_result(tc_id, content));
                }
            }
            _ => {}
        }
    }

    // Second pass: sanitize tool_use / tool_result pairing.
    // The LLM API requires every tool_use block to be followed by a matching
    // tool_result. Orphaned entries arise when:
    //   - A delegated agent loads thread history that includes the parent's
    //     in-progress tool_use (persisted incrementally) before its result exists.
    //   - Cross-session history has incomplete interactions.
    //
    // Strategy: two scans.
    //   1. Collect all tool_result IDs present in the history.
    //   2. Keep assistant+tool_calls only if ALL their call IDs have a result.
    //      Keep tool_results only if a matching tool_use was kept.

    // Scan 1: gather every tool_result ID in the history.
    let result_ids: std::collections::HashSet<String> = history_msgs
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Scan 2: filter messages, tracking which tool_call IDs we actually kept.
    let mut kept_tool_call_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut sanitized: Vec<ChatMessage> = Vec::with_capacity(history_msgs.len());
    for msg in history_msgs {
        if !msg.tool_calls.is_empty() {
            // Assistant message with tool calls — keep only if every call
            // has a corresponding result in the history.
            let all_have_results = msg.tool_calls.iter().all(|tc| result_ids.contains(&tc.id));
            if all_have_results {
                for tc in &msg.tool_calls {
                    kept_tool_call_ids.insert(tc.id.clone());
                }
                sanitized.push(msg);
            } else {
                debug!(
                    tool_ids = ?msg.tool_calls.iter().map(|tc| &tc.id).collect::<Vec<_>>(),
                    "Dropped assistant message with orphaned tool_use(s) from history (missing tool_result)"
                );
            }
        } else if let Some(ref tc_id) = msg.tool_call_id {
            // Tool result — only keep if we kept the matching tool_use.
            if kept_tool_call_ids.contains(tc_id) {
                sanitized.push(msg);
            } else {
                debug!(
                    tool_call_id = %tc_id,
                    "Dropped orphaned tool_result from history (no matching tool_use)"
                );
            }
        } else {
            sanitized.push(msg);
        }
    }
    messages.extend(sanitized);

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::current_time_context_block;
    use chrono::TimeZone;

    #[test]
    fn current_time_block_uses_timezone_for_relative_dates() {
        let now_utc = chrono::Utc
            .with_ymd_and_hms(2026, 3, 26, 22, 30, 0)
            .unwrap();
        let block = current_time_context_block("Europe/Berlin", "de", now_utc);

        assert!(block.contains("Current user timezone: Europe/Berlin"));
        assert!(block.contains("Today: 2026-03-26"));
        assert!(block.contains("Tomorrow: 2026-03-27"));
        assert!(block.contains("Yesterday: 2026-03-25"));
    }

    #[test]
    fn current_time_block_falls_back_to_utc_for_invalid_timezone() {
        let now_utc = chrono::Utc
            .with_ymd_and_hms(2026, 3, 26, 22, 30, 0)
            .unwrap();
        let block = current_time_context_block("Not/AZone", "en", now_utc);

        assert!(block.contains("Current user timezone: Not/AZone"));
        assert!(block.contains("Today: 2026-03-26"));
        assert!(block.contains("Tomorrow: 2026-03-27"));
        assert!(block.contains("Yesterday: 2026-03-25"));
    }
}
