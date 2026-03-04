/// Bundled default agent that is always available regardless of what is installed.
///
/// The default agent's definition is embedded into the binary at compile time
/// using `include_str!()`, so it works even with an empty installed agents
/// directory.
use crate::agents::loader::AgentDefinition;
use crate::permissions::tool_policy::{
    self, BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_EMIT_EVENT, BUILTIN_SET_REMINDER,
    BUILTIN_STORE_DELETE, BUILTIN_STORE_GET, BUILTIN_STORE_LIST, BUILTIN_STORE_SET, ToolPolicy,
};

const DEFAULT_AGENT_YAML: &str = include_str!("../../../../agents/default/agent.yaml");
const DEFAULT_AGENT_MD: &str = include_str!("../../../../agents/default/agent.md");

/// Build the bundled default agent definition.
///
/// This agent cannot be uninstalled. It is always injected into the in-memory
/// agents map at startup, overriding any installed agent with id "default".
pub fn default_agent() -> AgentDefinition {
    let mut agent: AgentDefinition = serde_yaml::from_str(DEFAULT_AGENT_YAML)
        .expect("bundled default agent.yaml is invalid — this is a build error");
    agent.system_prompt = DEFAULT_AGENT_MD.to_string();
    agent
}

/// Ensure the default agent has a row in the `agents` DB table.
///
/// This is called at startup to make sure the DB knows about the bundled
/// default agent (for model assignment queries etc.).
pub async fn ensure_db_row(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO agents (id, display_name, version, is_bundled, setup_complete) \
         VALUES ('default', 'Selu Assistant', '0.1.0', 1, 1) \
         ON CONFLICT(id) DO UPDATE SET is_bundled = 1",
    )
    .execute(db)
    .await?;

    Ok(())
}

/// Ensure the bundled default agent has global tool policies for its built-in
/// tools.
///
/// The bundled agent never goes through the setup wizard, so it won't get
/// global policies automatically.  This seeds sensible defaults (Allow for
/// all built-in tools) and also fills in any missing policies from upgrades
/// (e.g. when a new built-in tool like `set_reminder` is added).
pub async fn ensure_global_policies(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let required_builtins = vec![
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_DELEGATE.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_EMIT_EVENT.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_STORE_GET.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_STORE_SET.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_STORE_DELETE.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_STORE_LIST.to_string(),
            ToolPolicy::Allow,
        ),
        (
            BUILTIN_CAPABILITY_ID.to_string(),
            BUILTIN_SET_REMINDER.to_string(),
            ToolPolicy::Allow,
        ),
    ];

    // Check which built-in tools are missing from the default agent's policies
    let existing = tool_policy::get_global_policies_for_agent(db, "default").await?;
    let existing_set: std::collections::HashSet<(String, String)> = existing
        .into_iter()
        .map(|r| (r.capability_id, r.tool_name))
        .collect();

    let missing: Vec<_> = required_builtins
        .into_iter()
        .filter(|(cap_id, tool_name, _)| {
            !existing_set.contains(&(cap_id.clone(), tool_name.clone()))
        })
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    tracing::info!(
        "Seeding {} missing built-in tool policy/policies for default agent",
        missing.len()
    );
    tool_policy::set_global_policies(db, "default", &missing).await?;
    Ok(())
}

/// Ensure all installed agents have policies for newly added built-in tools.
///
/// When a new built-in tool (like `set_reminder`) is introduced, existing
/// agents that were installed before the upgrade won't have a policy for it.
/// This function adds the missing policies with a default of `Allow`.
pub async fn backfill_builtin_policies(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    // Get all agent IDs that have at least one global policy (i.e. they were set up)
    let agent_ids: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT agent_id FROM global_tool_policies")
            .fetch_all(db)
            .await?;

    let required_builtins: Vec<(&str, &str)> = vec![
        (BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE),
        (BUILTIN_CAPABILITY_ID, BUILTIN_EMIT_EVENT),
        (BUILTIN_CAPABILITY_ID, BUILTIN_STORE_GET),
        (BUILTIN_CAPABILITY_ID, BUILTIN_STORE_SET),
        (BUILTIN_CAPABILITY_ID, BUILTIN_STORE_DELETE),
        (BUILTIN_CAPABILITY_ID, BUILTIN_STORE_LIST),
        (BUILTIN_CAPABILITY_ID, BUILTIN_SET_REMINDER),
    ];

    for agent_id in &agent_ids {
        let existing = tool_policy::get_global_policies_for_agent(db, agent_id).await?;
        let existing_set: std::collections::HashSet<(String, String)> = existing
            .into_iter()
            .map(|r| (r.capability_id, r.tool_name))
            .collect();

        let missing: Vec<(String, String, ToolPolicy)> = required_builtins
            .iter()
            .filter(|(cap_id, tool_name)| {
                !existing_set.contains(&(cap_id.to_string(), tool_name.to_string()))
            })
            .map(|(cap_id, tool_name)| {
                (cap_id.to_string(), tool_name.to_string(), ToolPolicy::Allow)
            })
            .collect();

        if !missing.is_empty() {
            tracing::info!(
                agent_id = %agent_id,
                count = missing.len(),
                "Backfilling missing built-in tool policies"
            );
            tool_policy::set_global_policies(db, agent_id, &missing).await?;
        }
    }

    Ok(())
}
