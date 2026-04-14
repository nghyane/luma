use crate::core::session::Session;
use crate::core::types::{ContentBlock, Role};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

/// A single heuristic incident detected in a saved session.
#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub session_id: String,
    pub title: String,
    pub failure_type: String,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncidentDetail {
    pub session_id: String,
    pub title: String,
    pub task_preview: String,
    pub failure_types: Vec<String>,
    pub tool_uses: Vec<String>,
}

/// Top-level metrics from a local session scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditSummary {
    pub sessions_scanned: usize,
    pub sessions_with_project_instructions: usize,
    pub sessions_with_skill_loads: usize,
    pub mixed_local_remote_source_sessions: usize,
    pub premature_external_research_sessions: usize,
    pub edited_without_verify_sessions: usize,
    pub bash_verify_commands: usize,
    pub bash_file_inspection_commands: usize,
}

/// Scan the most recent `limit` saved sessions and compute lightweight metrics.

/// List heuristic incidents from the most recent `limit` saved sessions.

/// Show detailed heuristic findings for a single session id.
pub fn audit_show(session_id: &str) -> Option<IncidentDetail> {
    let session = load_recent_sessions(usize::MAX)
        .into_iter()
        .find(|s| s.id == session_id)?;

    let mut failure_types = Vec::new();
    let mut tool_uses = Vec::new();
    let mut used_remote = false;
    let mut used_local_read = false;
    let mut edited = false;
    let mut verified = false;
    let mut local_task = false;

    for msg in &session.messages {
        if msg.role == Role::User {
            let t = msg.text().to_lowercase();
            if !(t.contains("http://") || t.contains("https://") || t.contains("github.com") || t.contains("latest") || t.contains("current") || t.contains("news")) {
                local_task = true;
            }
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                tool_uses.push(match name.as_str() {
                    "Bash" | "exec_command" => format!("{} {}", name, input["command"].as_str().unwrap_or("")),
                    _ => format!("{} {}", name, input),
                });
                match name.as_str() {
                    "Read" => {
                        let path = input["path"].as_str().unwrap_or("");
                        if !path.is_empty() && !path.starts_with("artifact://") {
                            used_local_read = true;
                        }
                    }
                    "GhFile" | "GhLs" | "GhSearch" | "WebFetch" | "WebSearch" => used_remote = true,
                    "Edit" | "MultiEdit" | "Write" | "apply_patch" => edited = true,
                    "Bash" | "exec_command" => {
                        let command = input["command"].as_str().unwrap_or("");
                        if is_verify_command(command) {
                            verified = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if used_local_read && used_remote {
        failure_types.push("mixed_local_remote_source".into());
    }
    if local_task && used_remote {
        failure_types.push("premature_external_research".into());
    }
    if edited && !verified {
        failure_types.push("edited_without_verify".into());
    }

    Some(IncidentDetail {
        session_id: session.id,
        title: session.title,
        task_preview: session
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.text().lines().next().unwrap_or_default().to_owned())
            .unwrap_or_default(),
        failure_types,
        tool_uses,
    })
}

pub fn audit_incidents(limit: usize) -> Vec<Incident> {
    let mut incidents = Vec::new();
    for session in load_recent_sessions(limit) {
        let mut has_project_instructions = false;
        let mut has_skill_load = false;
        let mut used_remote = false;
        let mut used_local_read = false;
        let mut edited = false;
        let mut verified = false;
        let mut local_task = false;

        for msg in &session.messages {
            if msg.role == Role::System && msg.text().contains("<project_instructions>") {
                has_project_instructions = true;
            }
            if msg.role == Role::User {
                let t = msg.text().to_lowercase();
                if !(t.contains("http://") || t.contains("https://") || t.contains("github.com") || t.contains("latest") || t.contains("current") || t.contains("news")) {
                    local_task = true;
                }
            }
            for block in &msg.content {
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    match name.as_str() {
                        "Read" => {
                            let path = input["path"].as_str().unwrap_or("");
                            if path.starts_with("artifact://skill/") {
                                has_skill_load = true;
                            } else if !path.is_empty() && !path.starts_with("artifact://") {
                                used_local_read = true;
                            }
                        }
                        "GhFile" | "GhLs" | "GhSearch" | "WebFetch" | "WebSearch" => {
                            used_remote = true;
                        }
                        "Edit" | "MultiEdit" | "Write" | "apply_patch" => {
                            edited = true;
                        }
                        "Bash" | "exec_command" => {
                            let command = input["command"].as_str().unwrap_or("");
                            if is_verify_command(command) {
                                verified = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if used_local_read && used_remote {
            incidents.push(Incident {
                session_id: session.id.clone(),
                title: session.title.clone(),
                failure_type: "mixed_local_remote_source".into(),
                severity: "medium".into(),
            });
        }
        if local_task && used_remote {
            incidents.push(Incident {
                session_id: session.id.clone(),
                title: session.title.clone(),
                failure_type: "premature_external_research".into(),
                severity: "medium".into(),
            });
        }
        if edited && !verified {
            incidents.push(Incident {
                session_id: session.id.clone(),
                title: session.title.clone(),
                failure_type: "edited_without_verify".into(),
                severity: "low".into(),
            });
        }
        if has_project_instructions && !has_skill_load && local_task {
            // Weak signal for future routing; do not classify as high severity.
        }
    }
    incidents
}

pub fn audit_sessions(limit: usize) -> AuditSummary {
    let mut summary = AuditSummary::default();
    for session in load_recent_sessions(limit) {
        summary.sessions_scanned += 1;

        let mut has_project_instructions = false;
        let mut has_skill_load = false;
        let mut used_remote = false;
        let mut used_local_read = false;
        let mut edited = false;
        let mut verified = false;
        let mut local_task = false;

        for msg in &session.messages {
            if msg.role == Role::System && msg.text().contains("<project_instructions>") {
                has_project_instructions = true;
            }
            if msg.role == Role::User {
                let t = msg.text().to_lowercase();
                if !(t.contains("http://") || t.contains("https://") || t.contains("github.com") || t.contains("latest") || t.contains("current") || t.contains("news")) {
                    local_task = true;
                }
            }
            for block in &msg.content {
                match block {
                    ContentBlock::ToolUse { name, input, .. } => {
                        match name.as_str() {
                            "Read" => {
                                let path = input["path"].as_str().unwrap_or("");
                                if path.starts_with("artifact://skill/") {
                                    has_skill_load = true;
                                } else if !path.is_empty() && !path.starts_with("artifact://") {
                                    used_local_read = true;
                                }
                            }
                            "GhFile" | "GhLs" | "GhSearch" | "WebFetch" | "WebSearch" => {
                                used_remote = true;
                            }
                            "Edit" | "MultiEdit" | "Write" | "apply_patch" => {
                                edited = true;
                            }
                            "Bash" | "exec_command" => {
                                let command = input["command"].as_str().unwrap_or("");
                                if is_verify_command(command) {
                                    summary.bash_verify_commands += 1;
                                    verified = true;
                                }
                                if is_file_inspection_command(command) {
                                    summary.bash_file_inspection_commands += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        if has_project_instructions {
            summary.sessions_with_project_instructions += 1;
        }
        if has_skill_load {
            summary.sessions_with_skill_loads += 1;
        }
        if used_local_read && used_remote {
            summary.mixed_local_remote_source_sessions += 1;
        }
        if local_task && used_remote {
            summary.premature_external_research_sessions += 1;
        }
        if edited && !verified {
            summary.edited_without_verify_sessions += 1;
        }
    }
    summary
}

fn load_recent_sessions(limit: usize) -> Vec<Session> {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<Session> = entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            let raw = fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<Session>(&raw).ok()
        })
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.truncate(limit);
    sessions
}

fn sessions_dir() -> PathBuf {
    crate::config::home_dir()
        .join(".config")
        .join("luma")
        .join("sessions")
}

fn is_verify_command(command: &str) -> bool {
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "pytest",
        "npm test",
        "pnpm test",
        "yarn test",
        "bun test",
        "go test",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn is_file_inspection_command(command: &str) -> bool {
    ["cat ", "head ", "tail ", "sed ", "awk ", "ls", "tree", "find ", "rg ", "grep ", "wc -"]
        .iter()
        .any(|needle| command.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_command_detection() {
        assert!(is_verify_command("cargo check && cargo test"));
        assert!(is_verify_command("pytest -q"));
        assert!(!is_verify_command("git status"));
    }

    #[test]
    fn file_inspection_detection() {
        assert!(is_file_inspection_command("rg prompt src"));
        assert!(is_file_inspection_command("cat Cargo.toml"));
        assert!(is_file_inspection_command("wc -c RULES.md"));
        assert!(!is_file_inspection_command("cargo check"));
    }
}
