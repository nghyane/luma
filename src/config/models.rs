/// Model discovery, sync, and default resolution.
use crate::config::auth::{self, AuthProvider};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// A discovered model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub source: String,
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

/// Whether models have been synced before.
pub fn has_synced() -> bool {
    snapshot_path().exists()
}

/// All known models.
pub fn all_models() -> Vec<ModelEntry> {
    load_snapshot().map(|s| s.models).unwrap_or_default()
}

/// Context window for a model. Currently a constant default until per-model
/// data is populated from provider APIs.
pub fn context_window(_model_id: &str) -> u64 {
    200_000
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
        AgentMode::Deep => &[(&["codex"], "codex"), (&["opus"], "anthropic")],
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

/// Sync models from APIs. Returns number of models synced.
pub async fn sync() -> Result<usize> {
    let (anthropic, codex) = tokio::join!(scan_anthropic(), scan_codex());

    let mut models = Vec::new();
    if let Ok(m) = anthropic {
        models.extend(m);
    }
    if let Ok(m) = codex {
        models.extend(m);
    }

    let snapshot = Snapshot { models };

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
    let models = data["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(ModelEntry {
                        id: m["id"].as_str()?.to_owned(),
                        source: "anthropic".into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(models)
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
    let models = data["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(ModelEntry {
                        id: m["slug"].as_str()?.to_owned(),
                        source: "codex".into(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(models)
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
}
