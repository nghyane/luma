/// Model discovery, sync, and default resolution.
use crate::config::auth::{self, AuthVendor};
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
    pub source: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// Capability flags. Currently recognizes `"vision"`. Default empty
    /// = text-only, so catalog entries without an explicit annotation
    /// err on the safe side (tools fall back to metadata text).
    #[serde(default)]
    pub capabilities: Vec<String>,
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
    /// Unix seconds at which this snapshot was written. Missing on
    /// pre-2026-04 snapshots — `synced_at_or_zero` treats those as
    /// "ancient" so auto-sync picks them up on next start.
    #[serde(default)]
    synced_at: u64,
    /// luma version that wrote the snapshot. Used to force a re-sync
    /// when upgrading catalogs or adding new ModelEntry fields.
    #[serde(default)]
    luma_version: String,
}

impl Snapshot {
    fn is_stale(&self, now: u64, ttl_secs: u64) -> bool {
        if self.luma_version != env!("CARGO_PKG_VERSION") {
            return true;
        }
        self.synced_at == 0 || now.saturating_sub(self.synced_at) > ttl_secs
    }
}

/// Snapshot age before `all_models()` callers should kick off a
/// background re-sync. One week — long enough that normal use doesn't
/// pay for a network call per launch, short enough that newly rolled
/// provider models show up without requiring `luma sync`.
const SNAPSHOT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

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

/// Add custom models to the snapshot. Merges with existing models,
/// deduplicating by `(source, id)`.
pub fn add_custom_models(new_models: Vec<ModelEntry>) {
    let mut models = all_models();
    models.extend(new_models);
    let snapshot = Snapshot {
        models: normalize_models(models),
        synced_at: now_unix(),
        luma_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let path = snapshot_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    )
    .ok();
}

/// Whether the cached snapshot is missing, older than
/// [`SNAPSHOT_TTL_SECS`], or was written by a different luma version.
/// Callers surface this to decide whether to kick off a background sync.
pub fn should_auto_sync() -> bool {
    match load_snapshot() {
        Some(s) => s.is_stale(now_unix(), SNAPSHOT_TTL_SECS),
        None => true,
    }
}

/// Fire-and-forget sync on a tokio task. Safe to call on every startup
/// — noop if the snapshot is fresh and version-matched. Errors are
/// logged via `dbg_log!` and don't propagate; the caller keeps running
/// against the stale snapshot.
pub fn sync_in_background() {
    if !should_auto_sync() {
        return;
    }
    tokio::spawn(async {
        if let Err(e) = sync().await {
            crate::dbg_log!("background sync failed: {e}");
        }
    });
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    // Check saved per-mode preference first. Stored as `{source}/{id}`;
    // entries missing the separator are ignored so a malformed prefs
    // file doesn't silently route to a same-id model on the wrong gateway.
    let prefs = crate::config::prefs::load_mode_prefs(mode);
    if let Some(saved) = prefs.model.as_deref()
        && let Some((source, id)) = saved.split_once('/')
        && let Some(m) = models.iter().find(|m| m.source == source && m.id == id)
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
    let (anthropic, codex, kiro) = tokio::join!(scan_anthropic(), scan_codex(), scan_kiro());

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
    match kiro {
        Ok(found) => models.extend(found),
        Err(e) => {
            crate::dbg_log!("scan_kiro failed: {e}");
            models.extend(builtin_models().into_iter().filter(|m| m.source == "kiro"))
        }
    }
    // Alibaba Coding Plan currently has no dedicated list-models endpoint in
    // this codebase; ship a curated builtin set until a stable discovery API
    // is available.
    models.extend(
        builtin_models()
            .into_iter()
            .filter(|m| m.source == "alibaba"),
    );
    // OpenCode Go has no list-models endpoint; ship the builtin set.
    models.extend(
        builtin_models()
            .into_iter()
            .filter(|m| m.source == "opencode-go"),
    );
    // Preserve user-added custom models (e.g. Cloudflare) across syncs.
    if let Some(existing) = load_snapshot() {
        models.extend(
            existing
                .models
                .into_iter()
                .filter(|m| m.source == "cloudflare"),
        );
    }
    let snapshot = Snapshot {
        models: normalize_models(overlay_metadata(models)),
        synced_at: now_unix(),
        luma_version: env!("CARGO_PKG_VERSION").to_owned(),
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
    let auth = auth::resolve(AuthVendor::Anthropic).await?;
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
                        source: "anthropic".into(),
                        context_window: None,
                        max_output_tokens: None,
                        capabilities: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn scan_codex() -> Result<Vec<ModelEntry>> {
    let auth = auth::resolve(AuthVendor::OpenAI).await?;
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
                        source: "codex".into(),
                        context_window: m["context_window"].as_u64(),
                        max_output_tokens: m["max_output_tokens"].as_u64(),
                        capabilities: Vec::new(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn scan_kiro() -> Result<Vec<ModelEntry>> {
    let auth = auth::resolve(AuthVendor::Kiro).await?;
    let profile_arn = auth
        .profile_arn
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Kiro credential is missing profile_arn"))?;

    // AWS CodeWhisperer coral service — Bearer OAuth (no SigV4 needed for
    // this op), JSON/1.0 envelope, target in the header. Endpoint is the
    // same host as /generateAssistantResponse, different dispatch.
    let body = serde_json::json!({
        "profileArn": profile_arn,
        "origin": "KIRO_CLI",
    });
    let client = reqwest::Client::new();
    let res = client
        .post("https://q.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {}", auth.token))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header(
            "X-Amz-Target",
            "AmazonCodeWhispererService.ListAvailableModels",
        )
        .json(&body)
        .send()
        .await?;

    if !res.status().is_success() {
        anyhow::bail!("Kiro: {}", res.status());
    }

    let data: serde_json::Value = res.json().await?;
    Ok(data["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m["modelId"].as_str()?.to_owned();
                    let supported = m["supportedInputTypes"]
                        .as_array()
                        .map(|types| {
                            types
                                .iter()
                                .filter_map(|t| t.as_str())
                                .any(|t| t.eq_ignore_ascii_case("IMAGE"))
                        })
                        .unwrap_or(false);
                    let capabilities = if supported {
                        vec!["vision".to_owned()]
                    } else {
                        Vec::new()
                    };
                    Some(ModelEntry {
                        id,
                        source: "kiro".into(),
                        context_window: m["tokenLimits"]["maxInputTokens"].as_u64(),
                        max_output_tokens: m["tokenLimits"]["maxOutputTokens"].as_u64(),
                        capabilities,
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
        assert!(models.iter().any(|m| m.source == "alibaba"));
        assert!(models.iter().any(|m| m.source == "anthropic"));
        assert!(models.iter().any(|m| m.source == "codex"));
        assert!(models.iter().all(|m| m.context_window.is_some()));
    }

    #[test]
    fn overlay_metadata_fills_missing_fields_only() {
        let models = overlay_metadata(vec![ModelEntry {
            id: "gpt-5.4".into(),
            source: "codex".into(),
            context_window: None,
            max_output_tokens: Some(123),
            capabilities: Vec::new(),
        }]);
        let model = &models[0];
        // context_window filled from catalog, max_output_tokens preserved.
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(123));
    }

    #[test]
    fn snapshot_is_stale_on_version_mismatch_or_age() {
        let fresh = Snapshot {
            models: Vec::new(),
            synced_at: 1_000_000,
            luma_version: env!("CARGO_PKG_VERSION").into(),
        };
        // Same version, within TTL → fresh.
        assert!(!fresh.is_stale(1_000_100, SNAPSHOT_TTL_SECS));
        // Past TTL → stale.
        assert!(fresh.is_stale(1_000_000 + SNAPSHOT_TTL_SECS + 1, SNAPSHOT_TTL_SECS));

        // Mismatched version is always stale, regardless of age.
        let old_version = Snapshot {
            models: Vec::new(),
            synced_at: 1_000_000,
            luma_version: "0.0.0-ancient".into(),
        };
        assert!(old_version.is_stale(1_000_100, SNAPSHOT_TTL_SECS));

        // Default-deserialized (pre-metadata) snapshot has synced_at=0
        // and empty version → stale.
        let legacy: Snapshot = serde_json::from_str(r#"{"models":[]}"#).unwrap();
        assert!(legacy.is_stale(9_999_999_999, SNAPSHOT_TTL_SECS));
    }
}
