use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::agents::delegation;
use crate::agents::loader::AgentDefinition;
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
    latest_message: &str,
    inlined_manifests: &HashMap<String, CapabilityManifest>,
    delegated_agents: Option<&HashMap<String, Arc<AgentDefinition>>>,
) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    let _ = latest_message; // reserved for future use

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

    messages.push(ChatMessage::system(system));

    // ── User personality ──────────────────────────────────────────────────────
    // Load persistent personality facts for this user and inject them so the
    // agent knows about the user from the very first message.
    match personality::get_facts(db, user_id).await {
        Ok(facts) if !facts.is_empty() => {
            debug!(count = facts.len(), "Injecting personality facts into context");
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
                "user"      => messages.push(ChatMessage::user(&row.content)),
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
                "user"      => messages.push(ChatMessage::user(&row.content)),
                "assistant" => messages.push(ChatMessage::assistant(&row.content)),
                _ => {}
            }
        }
    };

    Ok(messages)
}
