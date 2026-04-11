/// Model discovery, sync, and default resolution.
use crate::config::auth::{self, AuthProvider};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

const BUILTIN_MODELS_JSON: &str = include_str!("models.catalog.json");

/// A discovered model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub source: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

/// Agent mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Rush,
    Smart,
    Deep,
}

impl AgentMode {
    /// Cycle to next mode.
    pub fn next(self) -> Self {
        match self {
            Self::Rush => Self::Smart,
            Self::Smart => Self::Deep,
            Self::Deep => Self::Rush,
        }
    }

    /// Display name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rush => "rush",
            Self::Smart => "smart",
            Self::Deep => "deep",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    models: Vec<ModelEntry>,
}

fn snapshot_path() -> PathBuf {
    dirs_home().join(".config").join("luma").join("models.json")
}

fn dirs_home() -> PathBuf {
    super::home_dir()
}

/// Load cached models snapshot.
pub(crate) fn load_snapshot() -> Option<Snapshot> {
    let raw = fs::read_to_string(snapshot_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn builtin_models() -> Vec<ModelEntry> {
    serde_json::from_str(BUILTIN_MODELS_JSON).unwrap_or_default()
}

fn overlay_metadata(models: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let meta: BTreeMap<(String, String), ModelEntry> = builtin_models()
        .into_iter()
        .map(|m| ((m.source.clone(), m.id.clone()), m))
        .collect();

    models
        .into_iter()
        .map(|mut model| {
            if let Some(extra) = meta.get(&(model.source.clone(), model.id.clone())) {
                if model.display_name.is_none() {
                    model.display_name = extra.display_name.clone();
                }
                if model.context_window.is_none() {
                    model.context_window = extra.context_window;
                }
                if model.max_output_tokens.is_none() {
                    model.max_output_tokens = extra.max_output_tokens;
                }
            }
            model
        })
        .collect()
}

fn normalize_models(models: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut seen = BTreeSet::new();
    let mut models: Vec<_> = models
        .into_iter()
        .filter(|m| seen.insert((m.source.clone(), m.id.clone())))
        .collect();
    models.sort_by(|a, b| a.source.cmp(&b.source).then(a.id.cmp(&b.id)));
    models
}

/// Whether models have been synced before.
pub fn has_synced() -> bool {
    snapshot_path().exists()
}

/// All known models.
pub fn all_models() -> Vec<ModelEntry> {
    load_snapshot()
        .map(|s| s.models)
        .unwrap_or_else(builtin_models)
}

/// Context window for a model. Currently a constant default until per-model
/// data is populated from the bundled catalog.
pub fn context_window(_model_id: &str) -> u64 {
    all_models()
        .into_iter()
        .find(|m| m.id == _model_id)
        .and_then(|m| m.context_window)
        .unwrap_or(200_000)
}

/// Resolve default model for a mode.
pub fn resolve_default(mode: AgentMode) -> Option<ModelEntry> {
    let models = all_models();

    // Check saved per-mode preference first
    let prefs = crate::config::prefs::load_mode_prefs(mode);
    if let Some(saved_id) = &prefs.model
        && let Some(m) = models.iter().find(|m| &m.id == saved_id)
    {
        return Some(m.clone());
    }

    let rules: &[(&[&str], &str)] = match mode {
        AgentMode::Rush => &[(&["haiku"], "anthropic"), (&["sonnet"], "anthropic")],
        AgentMode::Smart => &[(&["opus"], "anthropic"), (&["sonnet"], "anthropic")],
        AgentMode::Deep => &[(&["gpt-5.4"], "codex"), (&["opus"], "anthropic")],
    };

    for (keywords, source) in rules {
        let mut matches: Vec<_> = models
            .iter()
            .filter(|m| {
                m.source == *source && keywords.iter().all(|kw| m.id.to_lowercase().contains(kw))
            })
            .collect();
        matches.sort_by(|a, b| b.id.cmp(&a.id));
        if let Some(m) = matches.first() {
            return Some((*m).clone());
        }
    }
    None
}

/// Sync models from provider APIs, then overlay bundled metadata.
pub async fn sync() -> Result<usize> {
    let (anthropic, codex) = tokio::join!(scan_anthropic(), scan_codex());

    let mut models = Vec::new();
    match anthropic {
        Ok(found) => models.extend(found),
        Err(_) => models.extend(
            builtin_models()
                .into_iter()
                .filter(|m| m.source == "anthropic"),
        ),
    }
    match codex {
        Ok(found) => models.extend(found),
        Err(_) => models.extend(builtin_models().into_iter().filter(|m| m.source == "codex")),
    }
    let snapshot = Snapshot {
        models: normalize_models(overlay_metadata(models)),
    };

    let path = snapshot_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&snapshot)?)?;

    let count = snapshot.models.len();
    Ok(count)
}

async fn scan_anthropic() -> Result<Vec<ModelEntry>> {
    let auth = auth::resolve(AuthProvider::Anthropic).await?;
    let client = reqwest::Client::new();
    let res = client
        .get("https://api.anthropic.com/v1/models")
        .header("Authorization", format!("Bearer {}", auth.token))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await?;

    if !res.status().is_success() {
        anyhow::bail!("Anthropic: {}", res.status());
    }

    let data: serde_json::Value = res.json().await?;
    Ok(data["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(ModelEntry {
                        id: m["id"].as_str()?.to_owned(),
                        display_name: None,
                        source: "anthropic".into(),
                        context_window: None,
                        max_output_tokens: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn scan_codex() -> Result<Vec<ModelEntry>> {
    let auth = auth::resolve(AuthProvider::OpenAI).await?;
    let client = reqwest::Client::new();
    let res = client
        .get("https://chatgpt.com/backend-api/codex/models?client_version=1.0.0")
        .header("Authorization", format!("Bearer {}", auth.token))
        .send()
        .await?;

    if !res.status().is_success() {
        anyhow::bail!("Codex: {}", res.status());
    }

    let data: serde_json::Value = res.json().await?;
    Ok(data["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let slug = m["slug"].as_str()?;
                    let visibility = m["visibility"].as_str().unwrap_or("list");
                    if visibility != "list" {
                        return None;
                    }
                    Some(ModelEntry {
                        id: slug.to_owned(),
                        display_name: None,
                        source: "codex".into(),
                        context_window: m["context_window"].as_u64(),
                        max_output_tokens: m["max_output_tokens"].as_u64(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycle() {
        assert_eq!(AgentMode::Rush.next(), AgentMode::Smart);
        assert_eq!(AgentMode::Deep.next(), AgentMode::Rush);
    }

    #[test]
    fn mode_as_str() {
        assert_eq!(AgentMode::Smart.as_str(), "smart");
    }

    #[test]
    fn builtin_catalog_loads() {
        let models = builtin_models();
        assert!(models.iter().any(|m| m.source == "anthropic"));
        assert!(models.iter().any(|m| m.source == "codex"));
        assert!(models.iter().all(|m| m.context_window.is_some()));
    }

    #[test]
    fn overlay_metadata_fills_missing_fields_only() {
        let models = overlay_metadata(vec![ModelEntry {
            id: "gpt-5.4".into(),
            display_name: None,
            source: "codex".into(),
            context_window: None,
            max_output_tokens: Some(123),
        }]);
        let model = &models[0];
        assert_eq!(model.display_name.as_deref(), Some("GPT-5.4"));
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(123));
    }
}
