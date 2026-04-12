/// Preferences — persisted mode, model per mode, thinking level, last session.
use crate::config::models::AgentMode;
use crate::core::types::ThinkingLevel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn prefs_path() -> PathBuf {
    super::home_dir()
        .join(".config")
        .join("luma")
        .join("preferences.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct Prefs {
    mode: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    modes: HashMap<String, ModePrefs>,
    #[serde(default)]
    workspaces: HashMap<String, WorkspacePrefs>,
}

/// Per-mode preferences.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModePrefs {
    pub model: Option<String>,
}

/// Per-workspace preferences.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspacePrefs {
    pub last_session: Option<String>,
}

/// Load the active mode.
pub fn load_mode() -> AgentMode {
    let raw = fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
        .map(|p| p.mode)
        .unwrap_or_else(|| "smart".into());
    match raw.as_str() {
        "rush" => AgentMode::Rush,
        "deep" => AgentMode::Deep,
        _ => AgentMode::Smart,
    }
}

/// Load thinking level.
pub fn load_thinking() -> ThinkingLevel {
    let raw = fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
        .map(|p| p.thinking)
        .unwrap_or_default();
    match raw.as_str() {
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "max" => ThinkingLevel::Max,
        _ => ThinkingLevel::Off,
    }
}

/// Load per-mode preferences.
pub fn load_mode_prefs(mode: AgentMode) -> ModePrefs {
    fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
        .and_then(|p| p.modes.get(mode.as_str()).cloned())
        .unwrap_or_default()
}

/// Save active mode.
pub fn save_mode(mode: AgentMode) {
    update_prefs(|p| p.mode = mode.as_str().to_owned());
}

/// Save model for a mode.
pub fn save_mode_model(mode: AgentMode, model: &str) {
    update_prefs(|p| {
        p.modes.entry(mode.as_str().to_owned()).or_default().model = Some(model.to_owned());
    });
}

/// Save thinking level (global).
pub fn save_thinking(level: ThinkingLevel) {
    let label = match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Max => "max",
    };
    update_prefs(|p| p.thinking = label.to_owned());
}

/// Load last session for current workspace.
pub fn load_last_session() -> Option<String> {
    let cwd = std::env::current_dir().ok()?.to_string_lossy().into_owned();
    fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
        .and_then(|p| p.workspaces.get(&cwd)?.last_session.clone())
}

/// Save last session for current workspace.
pub fn save_last_session(session_id: &str) {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    update_prefs(|p| {
        p.workspaces.entry(cwd).or_default().last_session = Some(session_id.to_owned());
    });
}

fn update_prefs(f: impl FnOnce(&mut Prefs)) {
    let path = prefs_path();
    let mut prefs: Prefs = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Prefs {
            mode: "smart".into(),
            thinking: String::new(),
            modes: HashMap::new(),
            workspaces: HashMap::new(),
        });

    f(&mut prefs);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&prefs).unwrap_or_default(),
    )
    .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_prefs_default() {
        let prefs = ModePrefs::default();
        assert!(prefs.model.is_none());
    }

    #[test]
    fn workspace_prefs_default() {
        let prefs = WorkspacePrefs::default();
        assert!(prefs.last_session.is_none());
    }
}
