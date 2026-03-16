use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::agents::delegation;
use crate::agents::loader::AgentDefinition;
use crate::agents::memory;
use crate::agents::personality;
use crate::capabilities::manifest::CapabilityManifest;
use crate::llm::provider::ChatMessage;

/// Builds the LLM context window for a turn:
///   [system prompt + capability prompts (own + inlined) + agent registry (delegated only)]
///   [user personality facts (if any)]
///   [recent N messages from this thread (or session/pipe if no thread)]
pub async fn build(
    db: &SqlitePool,
    agent: &AgentDefinition,
    pipe_id: &str,
    session_id: &str,
    thread_id: Option<&str>,
    user_id: &str,
    user_language: &str,
    latest_message: &str,
    inlined_manifests: &HashMap<String, CapabilityManifest>,
    delegated_agents: Option<&HashMap<String, Arc<AgentDefinition>>>,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    // ── System prompt ─────────────────────────────────────────────────────────
    let mut system = agent.system_prompt.clone();

    // Append each of the agent's own capability prompts (if non-empty) so the
    // LLM knows what tools are available and how to use them.
    for manifest in agent.capability_manifests.values() {
        if !manifest.prompt.is_empty() {
            system.push_str("\n\n## Capability: ");
            system.push_str(&manifest.id);
            system.push('\n');
            system.push_str(&manifest.prompt);
        }
    }

    // Append capability prompts from inlined agents — their tools are directly
    // available on this agent's tool list, so the LLM needs usage instructions.
    for manifest in inlined_manifests.values() {
        if !manifest.prompt.is_empty() {
            system.push_str("\n\n## Capability: ");
            system.push_str(&manifest.id);
            system.push('\n');
            system.push_str(&manifest.prompt);
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

    // ── Current date & time (per-turn) ─────────────────────────────────────
    // Injected fresh every turn so the agent always knows the current date
    // and time — even when a user picks up a thread days later.
    let now = chrono::Local::now();
    system.push_str(&format!(
        "\n\n## Current date and time\n{}",
        now.format("%A, %B %-d, %Y, %H:%M (%Z)")
    ));

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
         Use `memory_remember` to save non-personality information that may help in future interactions. \
         Examples: recurring project context, preferred report formats for this agent, and stable workflow notes. \
         Use `memory_search` or `memory_list` when you need to inspect memory explicitly, and `memory_forget` when asked to remove a memory.",
    );

    system.push_str(
        "\n\n## Tool approval copy\n\
         When you call a tool, also include `_approval_message` in the tool arguments. \
         This must be a short, plain-language sentence describing what you want to do \
         from the user's perspective (no JSON, no field names, no technical wording). \
         Example (German): `Ich möchte den Artikel Spülmaschinensalz von deiner Liste entfernen.` \
         Keep it under 140 characters.",
    );

    messages.push(ChatMessage::system(system));

    // ── User personality ──────────────────────────────────────────────────────
    // Load persistent personality facts for this user and inject them so the
    // agent knows about the user from the very first message.
    match personality::get_facts(db, user_id).await {
        Ok(facts) if !facts.is_empty() => {
            debug!(
                count = facts.len(),
                "Injecting personality facts into context"
            );
            let mut ctx = String::from("## What you know about this user\n\n");
            ctx.push_str("The following facts have been learned about the user over time. ");
            ctx.push_str("Use this information naturally in your responses. ");
            ctx.push_str("Do NOT repeat these facts back to the user unless relevant.\n\n");

            // Group by category
            let categories = ["personal", "preferences", "location", "work", "other"];
            for cat in &categories {
                let cat_facts: Vec<_> = facts.iter().filter(|f| f.category == *cat).collect();
                if !cat_facts.is_empty() {
                    let label = match *cat {
                        "personal" => "Personal",
                        "preferences" => "Preferences",
                        "location" => "Location",
                        "work" => "Work & Skills",
                        _ => "Other",
                    };
                    ctx.push_str(&format!("### {}\n", label));
                    for fact in cat_facts {
                        ctx.push_str(&format!("- {}\n", fact.fact));
                    }
                    ctx.push('\n');
                }
            }
            messages.push(ChatMessage::system(ctx));
        }
        Ok(_) => {} // No personality facts yet
        Err(e) => {
            debug!("Personality loading failed (non-fatal): {e}");
        }
    }

    // ── Agent memory (BM25 retrieval) ────────────────────────────────────────
    match memory::search_for_context(db, &agent.id, user_id, latest_message, 5).await {
        Ok(hits) if !hits.is_empty() => {
            let mut ctx = String::from("## Relevant memory for this user\n\n");
            ctx.push_str(
                "These notes were saved for this specific agent from previous interactions. ",
            );
            ctx.push_str(
                "Use them when relevant, but do not repeat them unless they help answer the user.\n\n",
            );

            for hit in hits {
                if hit.tags.trim().is_empty() {
                    ctx.push_str(&format!("- {}\n", hit.memory));
                } else {
                    ctx.push_str(&format!("- {} (tags: {})\n", hit.memory, hit.tags));
                }
            }

            messages.push(ChatMessage::system(ctx));
        }
        Ok(_) => {}
        Err(e) => {
            debug!("Agent memory loading failed (non-fatal): {e}");
        }
    }

    // ── Conversation history ──────────────────────────────────────────────────
    // If a thread_id is provided, pull messages for that specific thread.
    // This gives each thread its own isolated conversation history while
    // sharing personality knowledge across all sessions.
    // If no thread_id (web chat, delegation), fall back to pipe+session history.
    if let Some(tid) = thread_id {
        let history = sqlx::query!(
            r#"SELECT role, content FROM messages
               WHERE thread_id = ? AND pipe_id = ?
               ORDER BY created_at DESC
               LIMIT 30"#,
            tid,
            pipe_id,
        )
        .fetch_all(db)
        .await?;

        for row in history.into_iter().rev() {
            match row.role.as_str() {
                "user" => messages.push(ChatMessage::user(&row.content)),
                "assistant" => messages.push(ChatMessage::assistant(&row.content)),
                _ => {}
            }
        }
    } else {
        let history = sqlx::query!(
            r#"SELECT role, content FROM messages
               WHERE pipe_id = ? AND (session_id = ? OR session_id IS NULL)
                 AND thread_id IS NULL
               ORDER BY created_at DESC
               LIMIT 30"#,
            pipe_id,
            session_id,
        )
        .fetch_all(db)
        .await?;

        for row in history.into_iter().rev() {
            match row.role.as_str() {
                "user" => messages.push(ChatMessage::user(&row.content)),
                "assistant" => messages.push(ChatMessage::assistant(&row.content)),
                _ => {}
            }
        }
    };

    Ok(messages)
}
