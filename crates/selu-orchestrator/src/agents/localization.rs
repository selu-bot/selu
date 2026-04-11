use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

use crate::agents::loader::{AgentDefinition, AgentSchedulePreset, InstallStep};

fn default_locale() -> String {
    "en".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentI18nConfig {
    #[serde(default = "default_locale")]
    pub default_locale: String,
    #[serde(default)]
    pub supported_locales: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentLocaleBundle {
    #[serde(default)]
    pub agent: AgentLocaleText,
    #[serde(default)]
    pub install_steps: HashMap<String, InstallStepLocaleText>,
    #[serde(default)]
    pub automation: AutomationLocaleText,
    #[serde(default)]
    pub capabilities: HashMap<String, CapabilityLocaleText>,
    #[serde(default)]
    pub approval: ApprovalLocaleText,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentLocaleText {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub long_description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstallStepLocaleText {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutomationLocaleText {
    #[serde(default)]
    pub schedules: HashMap<String, ScheduleLocaleText>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScheduleLocaleText {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub cron_description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapabilityLocaleText {
    #[serde(default)]
    pub tools: HashMap<String, ToolLocaleText>,
    #[serde(default)]
    pub credentials: HashMap<String, CredentialLocaleText>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolLocaleText {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input: HashMap<String, InputFieldLocaleText>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputFieldLocaleText {
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialLocaleText {
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalLocaleText {
    #[serde(default)]
    pub tools: HashMap<String, ApprovalToolLocaleText>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalToolLocaleText {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

pub async fn load_locale_bundles(agent_dir: &Path) -> Result<HashMap<String, AgentLocaleBundle>> {
    let i18n_dir = agent_dir.join("i18n");
    if !i18n_dir.exists() {
        return Ok(HashMap::new());
    }

    let mut bundles = HashMap::new();
    let mut entries = fs::read_dir(&i18n_dir)
        .await
        .with_context(|| format!("Cannot read i18n dir: {}", i18n_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };

        let locale = if let Some(locale) = file_name.strip_suffix(".yaml") {
            locale
        } else if let Some(locale) = file_name.strip_suffix(".yml") {
            locale
        } else {
            continue;
        };

        let yaml = fs::read_to_string(&path)
            .await
            .with_context(|| format!("Failed to read locale file {}", path.display()))?;
        let bundle: AgentLocaleBundle = serde_yaml::from_str(&yaml)
            .with_context(|| format!("Invalid locale file {}", path.display()))?;
        bundles.insert(locale.to_string(), bundle);
    }

    Ok(bundles)
}

pub async fn load_localized_markdown_files(
    dir: &Path,
    stem: &str,
) -> Result<HashMap<String, String>> {
    if !dir.exists() {
        return Ok(HashMap::new());
    }

    let mut localized = HashMap::new();
    let mut entries = fs::read_dir(dir)
        .await
        .with_context(|| format!("Cannot read dir: {}", dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };

        let prefix = format!("{stem}.");
        if !file_name.starts_with(&prefix) || !file_name.ends_with(".md") {
            continue;
        }

        let locale = file_name
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(".md"));
        let Some(locale) = locale else {
            continue;
        };

        let content = fs::read_to_string(&path)
            .await
            .with_context(|| format!("Failed to read localized markdown {}", path.display()))?;
        localized.insert(locale.to_string(), content);
    }

    Ok(localized)
}

pub fn language_candidates(requested: &str, default_locale: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    let mut push = |lang: &str| {
        if lang.is_empty() || candidates.iter().any(|existing| existing == lang) {
            return;
        }
        candidates.push(lang.to_string());
    };

    push(requested);
    let requested_base = requested.split(['-', '_']).next().unwrap_or(requested);
    if requested_base != requested {
        push(requested_base);
    }

    push(default_locale);
    let default_base = default_locale
        .split(['-', '_'])
        .next()
        .unwrap_or(default_locale);
    if default_base != default_locale {
        push(default_base);
    }

    push("en");
    candidates
}

fn find_locale_bundle<'a>(
    agent: &'a AgentDefinition,
    requested: &str,
) -> Option<&'a AgentLocaleBundle> {
    for candidate in language_candidates(requested, &agent.i18n.default_locale) {
        if let Some(bundle) = agent.locales.get(&candidate) {
            return Some(bundle);
        }
    }
    None
}

fn find_string_map<'a>(
    map: &'a HashMap<String, String>,
    requested: &str,
    default_locale: &str,
) -> Option<&'a str> {
    for candidate in language_candidates(requested, default_locale) {
        if let Some(value) = map.get(&candidate) {
            return Some(value);
        }
    }
    None
}

fn first_non_empty(values: &[Option<&String>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

fn key_candidates(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |value: String| {
        if !value.is_empty() && !out.iter().any(|existing| existing == &value) {
            out.push(value);
        }
    };

    push(key.to_string());
    push(key.to_ascii_lowercase());
    push(key.replace('-', "_"));
    push(key.replace('_', "-"));
    push(key.replace('-', "_").to_ascii_lowercase());
    push(key.replace('_', "-").to_ascii_lowercase());

    out
}

fn find_by_key<'a, T>(map: &'a HashMap<String, T>, key: &str) -> Option<&'a T> {
    for candidate in key_candidates(key) {
        if let Some(value) = map.get(&candidate) {
            return Some(value);
        }
    }
    None
}

fn namespaced_key_candidates(capability_id: &str, tool_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for cap in key_candidates(capability_id) {
        for tool in key_candidates(tool_name) {
            let candidate = format!("{cap}__{tool}");
            if !out.iter().any(|existing| existing == &candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

fn find_approval_tool<'a>(
    tools: &'a HashMap<String, ApprovalToolLocaleText>,
    capability_id: &str,
    tool_name: &str,
) -> Option<&'a ApprovalToolLocaleText> {
    for candidate in namespaced_key_candidates(capability_id, tool_name) {
        if let Some(value) = tools.get(&candidate) {
            return Some(value);
        }
    }
    None
}

impl AgentDefinition {
    pub fn localized_name(&self, requested: &str) -> String {
        first_non_empty(&[
            find_locale_bundle(self, requested).and_then(|bundle| bundle.agent.name.as_ref()),
            Some(&self.name),
        ])
        .unwrap_or_else(|| self.id.clone())
    }

    pub fn localized_system_prompt(&self, requested: &str) -> &str {
        find_string_map(
            &self.localized_system_prompts,
            requested,
            &self.i18n.default_locale,
        )
        .unwrap_or(&self.system_prompt)
    }
}

pub fn localized_install_step_label(
    agent: &AgentDefinition,
    step: &InstallStep,
    requested: &str,
) -> String {
    first_non_empty(&[
        find_locale_bundle(agent, requested)
            .and_then(|bundle| bundle.install_steps.get(&step.id))
            .and_then(|step| step.label.as_ref()),
        Some(&step.label),
    ])
    .unwrap_or_else(|| step.id.clone())
}

pub fn localized_install_step_description(
    agent: &AgentDefinition,
    step: &InstallStep,
    requested: &str,
) -> String {
    first_non_empty(&[
        find_locale_bundle(agent, requested)
            .and_then(|bundle| bundle.install_steps.get(&step.id))
            .and_then(|step| step.description.as_ref()),
        Some(&step.description),
    ])
    .unwrap_or_default()
}

pub fn localized_schedule_label(
    agent: &AgentDefinition,
    schedule: &AgentSchedulePreset,
    requested: &str,
) -> String {
    first_non_empty(&[
        find_locale_bundle(agent, requested)
            .and_then(|bundle| bundle.automation.schedules.get(&schedule.id))
            .and_then(|schedule| schedule.label.as_ref()),
        Some(&schedule.label),
    ])
    .unwrap_or_else(|| schedule.id.clone())
}

pub fn localized_schedule_prompt(
    agent: &AgentDefinition,
    schedule: &AgentSchedulePreset,
    requested: &str,
) -> String {
    first_non_empty(&[
        find_locale_bundle(agent, requested)
            .and_then(|bundle| bundle.automation.schedules.get(&schedule.id))
            .and_then(|schedule| schedule.prompt.as_ref()),
        Some(&schedule.prompt),
    ])
    .unwrap_or_default()
}

pub fn localized_schedule_cron_description(
    agent: &AgentDefinition,
    schedule: &AgentSchedulePreset,
    requested: &str,
) -> String {
    first_non_empty(&[
        find_locale_bundle(agent, requested)
            .and_then(|bundle| bundle.automation.schedules.get(&schedule.id))
            .and_then(|schedule| schedule.cron_description.as_ref()),
        Some(&schedule.cron_description),
    ])
    .unwrap_or_default()
}

pub fn localized_tool_display_name(
    agent: &AgentDefinition,
    capability_id: &str,
    tool_name: &str,
    fallback: &str,
    requested: &str,
) -> String {
    let approval_label = find_locale_bundle(agent, requested)
        .and_then(|bundle| find_approval_tool(&bundle.approval.tools, capability_id, tool_name))
        .and_then(|tool| tool.label.as_ref());

    let capability_tool_label = find_locale_bundle(agent, requested)
        .and_then(|bundle| find_by_key(&bundle.capabilities, capability_id))
        .and_then(|cap| find_by_key(&cap.tools, tool_name))
        .and_then(|tool| tool.display_name.as_ref());

    first_non_empty(&[approval_label, capability_tool_label])
        .unwrap_or_else(|| fallback.to_string())
}

pub fn localized_tool_description(
    agent: &AgentDefinition,
    capability_id: &str,
    tool_name: &str,
    fallback: &str,
    requested: &str,
) -> String {
    first_non_empty(&[find_locale_bundle(agent, requested)
        .and_then(|bundle| find_by_key(&bundle.capabilities, capability_id))
        .and_then(|cap| find_by_key(&cap.tools, tool_name))
        .and_then(|tool| tool.description.as_ref())])
    .unwrap_or_else(|| fallback.to_string())
}

pub fn localized_credential_description(
    agent: &AgentDefinition,
    capability_id: &str,
    credential_name: &str,
    fallback: &str,
    requested: &str,
) -> String {
    first_non_empty(&[find_locale_bundle(agent, requested)
        .and_then(|bundle| find_by_key(&bundle.capabilities, capability_id))
        .and_then(|cap| find_by_key(&cap.credentials, credential_name))
        .and_then(|credential| credential.description.as_ref())])
    .unwrap_or_else(|| fallback.to_string())
}

pub fn localized_approval_label(
    agent: &AgentDefinition,
    capability_id: &str,
    tool_name: &str,
    fallback: &str,
    requested: &str,
) -> String {
    localized_tool_display_name(agent, capability_id, tool_name, fallback, requested)
}

pub fn localized_approval_message(
    agent: &AgentDefinition,
    capability_id: &str,
    tool_name: &str,
    requested: &str,
) -> Option<String> {
    first_non_empty(&[find_locale_bundle(agent, requested)
        .and_then(|bundle| find_approval_tool(&bundle.approval.tools, capability_id, tool_name))
        .and_then(|tool| tool.message.as_ref())])
}

#[cfg(test)]
mod tests {
    use super::{
        AgentI18nConfig, AgentLocaleBundle, AgentLocaleText, ApprovalLocaleText,
        ApprovalToolLocaleText, language_candidates, localized_approval_label,
    };
    use crate::agents::loader::{
        AgentAutomationConfig, AgentDefinition, ModelConfig, RoutingMode, SessionConfig,
    };
    use std::collections::HashMap;

    #[test]
    fn language_candidates_prefer_requested_then_default_then_en() {
        let candidates = language_candidates("de-DE", "fr");
        assert_eq!(candidates, vec!["de-DE", "de", "fr", "en"]);
    }

    #[test]
    fn language_candidates_support_underscore_locales() {
        let candidates = language_candidates("de_DE", "en_US");
        assert_eq!(candidates, vec!["de_DE", "de", "en_US", "en"]);
    }

    #[test]
    fn localized_approval_label_resolves_namespaced_key() {
        let mut locales = HashMap::new();
        let mut approval_tools = HashMap::new();
        approval_tools.insert(
            "weather-api__get_forecast".to_string(),
            ApprovalToolLocaleText {
                label: Some("Wetter abrufen".to_string()),
                message: None,
            },
        );
        locales.insert(
            "de".to_string(),
            AgentLocaleBundle {
                agent: AgentLocaleText::default(),
                install_steps: HashMap::new(),
                automation: Default::default(),
                capabilities: HashMap::new(),
                approval: ApprovalLocaleText {
                    tools: approval_tools,
                },
            },
        );

        let agent = AgentDefinition {
            id: "weather".to_string(),
            name: "Weather Assistant".to_string(),
            i18n: AgentI18nConfig {
                default_locale: "en".to_string(),
                supported_locales: vec!["en".to_string(), "de".to_string()],
            },
            model: Some(ModelConfig {
                provider: "openai".to_string(),
                model_id: "gpt-5-mini".to_string(),
                temperature: 0.7,
            }),
            routing: RoutingMode::default(),
            session: SessionConfig::default(),
            system_prompt: String::new(),
            capabilities: vec![],
            capability_manifests: HashMap::new(),
            install_steps: vec![],
            automation: AgentAutomationConfig::default(),
            locales,
            localized_system_prompts: HashMap::new(),
        };

        let label =
            localized_approval_label(&agent, "weather-api", "get_forecast", "Get forecast", "de");
        assert_eq!(label, "Wetter abrufen");
    }
}
