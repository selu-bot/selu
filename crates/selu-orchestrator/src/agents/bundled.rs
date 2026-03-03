/// Bundled default agent that is always available regardless of what is installed.
///
/// The default agent's definition is embedded into the binary at compile time
/// using `include_str!()`, so it works even with an empty installed agents
/// directory.
use crate::agents::loader::AgentDefinition;
use crate::permissions::tool_policy::{
    self, BUILTIN_CAPABILITY_ID, BUILTIN_DELEGATE, BUILTIN_EMIT_EVENT, ToolPolicy,
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
/// delegation and events) if no global policies exist yet.
pub async fn ensure_global_policies(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let existing = tool_policy::get_global_policies_for_agent(db, "default").await?;
    if !existing.is_empty() {
        return Ok(());
    }

    tracing::info!("Seeding global tool policies for bundled default agent");

    let policies = vec![
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
    ];

    tool_policy::set_global_policies(db, "default", &policies).await?;
    Ok(())
}
