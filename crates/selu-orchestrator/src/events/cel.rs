/// CEL (Common Expression Language) filter evaluation for event subscriptions.
///
/// Each subscription may have an optional `filter_expression` — a CEL
/// expression that must evaluate to `true` for the subscription to fire.
///
/// Variables available in every filter:
///   `event_type`  — string, e.g. `"calendar.reminder"`
///   `event`       — map, the full event payload
///
/// Examples:
///   `event_type == "calendar.reminder"`
///   `event_type.startsWith("home.") && event.severity == "high"`
///   `event.temperature > 25`
use anyhow::{Result, anyhow};
use cel_interpreter::{Context, Program, Value};
use std::collections::HashMap;
use std::sync::Arc;

use selu_core::types::AgentEvent;

/// Returns `true` if the event passes the CEL `expression`, or if
/// `expression` is `None` / empty (no filter = always pass).
pub fn matches(expression: Option<&str>, event: &AgentEvent) -> Result<bool> {
    let expr = match expression {
        None | Some("") => return Ok(true),
        Some(e) => e,
    };

    let program = Program::compile(expr)
        .map_err(|e| anyhow!("CEL compile error in filter {:?}: {}", expr, e))?;

    let mut context = Context::default();

    context
        .add_variable(
            "event_type",
            Value::String(Arc::new(event.event_type.clone())),
        )
        .map_err(|e| anyhow!("CEL context error: {}", e))?;

    context
        .add_variable("event", json_to_cel(&event.payload))
        .map_err(|e| anyhow!("CEL context error: {}", e))?;

    let result = program
        .execute(&context)
        .map_err(|e| anyhow!("CEL execution error: {}", e))?;

    match result {
        Value::Bool(b) => Ok(b),
        other => Err(anyhow!("CEL filter must return bool, got {:?}", other)),
    }
}

/// Recursively convert a `serde_json::Value` to a `cel_interpreter::Value`.
fn json_to_cel(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(Arc::new(s.clone())),
        serde_json::Value::Array(arr) => {
            Value::List(Arc::new(arr.iter().map(json_to_cel).collect()))
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<cel_interpreter::objects::Key, Value> = obj
                .iter()
                .map(|(k, v)| {
                    (
                        cel_interpreter::objects::Key::String(Arc::new(k.clone())),
                        json_to_cel(v),
                    )
                })
                .collect();
            Value::Map(cel_interpreter::objects::Map { map: Arc::new(map) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_event(event_type: &str, payload: serde_json::Value) -> AgentEvent {
        AgentEvent {
            id: Uuid::new_v4(),
            source_session_id: Uuid::new_v4(),
            source_agent_id: "test".into(),
            event_type: event_type.to_string(),
            payload,
            chain_depth: 0,
            emitted_at: chrono::Utc::now(),
            processed_at: None,
        }
    }

    #[test]
    fn no_filter_always_passes() {
        let event = make_event("foo", serde_json::json!({}));
        assert!(matches(None, &event).unwrap());
        assert!(matches(Some(""), &event).unwrap());
    }

    #[test]
    fn event_type_filter() {
        let event = make_event("calendar.reminder", serde_json::json!({}));
        assert!(matches(Some(r#"event_type == "calendar.reminder""#), &event).unwrap());
        assert!(!matches(Some(r#"event_type == "home.alert""#), &event).unwrap());
    }

    #[test]
    fn payload_field_filter() {
        let event = make_event("sensor", serde_json::json!({"temperature": 30}));
        assert!(matches(Some("event.temperature > 25"), &event).unwrap());
        assert!(!matches(Some("event.temperature < 25"), &event).unwrap());
    }

    #[test]
    fn boolean_payload_field() {
        let event = make_event("alert", serde_json::json!({"critical": true}));
        assert!(matches(Some("event.critical"), &event).unwrap());
    }

    #[test]
    fn non_bool_result_is_error() {
        let event = make_event("x", serde_json::json!({}));
        assert!(matches(Some("42"), &event).is_err());
    }
}
