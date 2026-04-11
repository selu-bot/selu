use anyhow::Result;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyLevel {
    Low,
    Medium,
    High,
}

impl AutonomyLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    pub max_tool_loop_iterations: u32,
    pub max_delegation_hops: i32,
}

pub fn limits_for_autonomy(level: AutonomyLevel) -> RuntimeLimits {
    match level {
        AutonomyLevel::Low => RuntimeLimits {
            max_tool_loop_iterations: 24,
            max_delegation_hops: 1,
        },
        AutonomyLevel::Medium => RuntimeLimits {
            max_tool_loop_iterations: 64,
            max_delegation_hops: 5,
        },
        AutonomyLevel::High => RuntimeLimits {
            max_tool_loop_iterations: 200,
            max_delegation_hops: 6,
        },
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveRuntimeSettings {
    pub autonomy_level: AutonomyLevel,
    pub use_advanced_limits: bool,
    pub max_tool_loop_iterations: u32,
    pub max_delegation_hops: i32,
    pub agent_default_level: AutonomyLevel,
    pub agent_default_limits: RuntimeLimits,
}

pub async fn resolve_effective_runtime_settings(
    db: &sqlx::SqlitePool,
    user_id: &str,
    agent_id: &str,
) -> Result<EffectiveRuntimeSettings> {
    let agent_defaults = sqlx::query(
        "SELECT autonomy_level, max_tool_loop_iterations, max_delegation_hops
         FROM agents
         WHERE id = ?",
    )
    .bind(agent_id)
    .fetch_optional(db)
    .await?;

    let agent_default_level = agent_defaults
        .as_ref()
        .and_then(|r| {
            r.try_get::<Option<String>, _>("autonomy_level")
                .ok()
                .flatten()
        })
        .as_deref()
        .and_then(AutonomyLevel::parse)
        .unwrap_or(AutonomyLevel::Medium);
    let mut agent_default_limits = limits_for_autonomy(agent_default_level);
    if let Some(row) = agent_defaults.as_ref() {
        if let Ok(v) = row.try_get::<i64, _>("max_tool_loop_iterations") {
            agent_default_limits.max_tool_loop_iterations =
                sanitize_steps(v, agent_default_limits.max_tool_loop_iterations);
        }
        if let Ok(v) = row.try_get::<i64, _>("max_delegation_hops") {
            agent_default_limits.max_delegation_hops =
                sanitize_hops(v, agent_default_limits.max_delegation_hops);
        }
    }

    let user_row = sqlx::query(
        "SELECT autonomy_level, use_advanced_limits, max_tool_loop_iterations, max_delegation_hops
         FROM user_agent_runtime_settings
         WHERE user_id = ? AND agent_id = ?",
    )
    .bind(user_id)
    .bind(agent_id)
    .fetch_optional(db)
    .await?;

    let user_level = user_row
        .as_ref()
        .and_then(|r| {
            r.try_get::<Option<String>, _>("autonomy_level")
                .ok()
                .flatten()
        })
        .as_deref()
        .and_then(AutonomyLevel::parse);
    let autonomy_level = user_level.unwrap_or(agent_default_level);

    let base_limits = limits_for_autonomy(autonomy_level);
    let mut max_tool_loop_iterations = base_limits.max_tool_loop_iterations;
    let mut max_delegation_hops = base_limits.max_delegation_hops;
    let use_advanced_limits = user_row
        .as_ref()
        .and_then(|r| r.try_get::<i64, _>("use_advanced_limits").ok())
        .map(|v| v != 0)
        .unwrap_or(false);

    if use_advanced_limits {
        if let Some(row) = user_row.as_ref() {
            if let Ok(v) = row.try_get::<i64, _>("max_tool_loop_iterations") {
                max_tool_loop_iterations = sanitize_steps(v, max_tool_loop_iterations);
            }
            if let Ok(v) = row.try_get::<i64, _>("max_delegation_hops") {
                max_delegation_hops = sanitize_hops(v, max_delegation_hops);
            }
        } else {
            max_tool_loop_iterations = agent_default_limits.max_tool_loop_iterations;
            max_delegation_hops = agent_default_limits.max_delegation_hops;
        }
    }

    Ok(EffectiveRuntimeSettings {
        autonomy_level,
        use_advanced_limits,
        max_tool_loop_iterations,
        max_delegation_hops,
        agent_default_level,
        agent_default_limits,
    })
}

pub fn sanitize_steps(value: i64, fallback: u32) -> u32 {
    if value < 0 {
        return fallback;
    }
    if value > u32::MAX as i64 {
        return u32::MAX;
    }
    value as u32
}

pub fn sanitize_hops(value: i64, fallback: i32) -> i32 {
    if value < 0 {
        return fallback;
    }
    if value > i32::MAX as i64 {
        return i32::MAX;
    }
    value as i32
}
