use crate::core::session::Session;
use crate::core::types::{ContentBlock, Role};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

const DETECTOR_VERSION: &str = "v1";
const PACKET_EXCERPT_LIMIT: usize = 3;

/// A single heuristic incident detected in a saved session.
#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub session_id: String,
    pub title: String,
    pub failure_type: String,
    pub severity: String,
    pub task_family: String,
    pub detector_version: String,
    pub reviewer_eligibility: String,
    pub source_of_truth_classification: String,
    pub subsystem: String,
}

/// Detailed heuristic findings for one session.
#[derive(Debug, Clone, Serialize)]
pub struct IncidentDetail {
    pub session_id: String,
    pub title: String,
    pub task_preview: String,
    pub failure_types: Vec<String>,
    pub tool_uses: Vec<String>,
    pub representative_local_read: Option<String>,
    pub representative_remote_use: Option<String>,
    pub representative_edit: Option<String>,
    pub representative_verify: Option<String>,
    pub task_family: String,
    pub detector_version: String,
    pub reviewer_eligibility: String,
    pub source_of_truth_classification: String,
    pub severity: String,
}

/// Top-level metrics from a local session scan.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditSummary {
    pub sessions_scanned: usize,
    pub sessions_with_project_instructions: usize,
    pub sessions_with_skill_loads: usize,
    pub wrong_source_sessions: usize,
    pub premature_external_research_sessions: usize,
    pub missing_verification_sessions: usize,
    pub bash_verify_commands: usize,
    pub shell_local_read_commands: usize,
    pub shell_file_counting_commands: usize,
    pub shell_verify_output_slicing_commands: usize,
    pub shell_patch_style_search_commands: usize,
}

/// Compact audit packet used as reviewer input.
#[derive(Debug, Clone, Serialize)]
pub struct EvidencePacket {
    pub session_id: String,
    pub title: String,
    pub task_preview: String,
    pub task_family: String,
    pub detector_version: String,
    pub failure_types: Vec<String>,
    pub severity: String,
    pub reviewer_eligibility: String,
    pub source_of_truth_classification: String,
    pub tool_sequence_summary: Vec<String>,
    pub representative_excerpts: Vec<String>,
    pub representative_spans: Vec<PacketSpanRef>,
    pub supporting_counts: PacketCounts,
}

/// Reference back into the raw session trace for packet rehydration.
#[derive(Debug, Clone, Serialize)]
pub struct PacketSpanRef {
    pub message_index: usize,
    pub block_index: usize,
    pub kind: String,
    pub preview: String,
}

/// Aggregated counts attached to an evidence packet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PacketCounts {
    pub tool_uses: usize,
    pub local_reads: usize,
    pub remote_uses: usize,
    pub edits: usize,
    pub verify_signals: usize,
}

/// Deterministic cluster of similar incidents.
#[derive(Debug, Clone, Serialize)]
pub struct IncidentCluster {
    pub cluster_key: String,
    pub failure_type: String,
    pub task_family: String,
    pub subsystem: String,
    pub detector_version: String,
    pub session_ids: Vec<String>,
    pub count: usize,
    pub highest_severity: String,
}

#[derive(Debug, Clone, Default)]
struct SessionFacts {
    has_project_instructions: bool,
    has_skill_load: bool,
    used_remote: bool,
    used_local_read: bool,
    edited: bool,
    verified: bool,
    local_task: bool,
    task_family: String,
    source_of_truth_classification: String,
    subsystem: String,
    tool_uses: Vec<String>,
    representative_local_read: Option<String>,
    representative_remote_use: Option<String>,
    representative_edit: Option<String>,
    representative_verify: Option<String>,
    local_read_count: usize,
    remote_use_count: usize,
    edit_count: usize,
    verify_count: usize,
    representative_excerpts: Vec<String>,
    representative_spans: Vec<PacketSpanRef>,
}

/// Show detailed heuristic findings for a single session id.
pub fn audit_show(session_id: &str) -> Option<IncidentDetail> {
    let session = load_recent_sessions(usize::MAX)
        .into_iter()
        .find(|s| s.id == session_id)?;
    let facts = inspect_session(&session);
    let failure_types = detect_failure_types(&facts);

    let task_preview = session_task_preview(&session);
    let reviewer_eligibility = reviewer_eligibility(&facts).to_owned();
    let severity = session_severity(&facts).to_owned();

    Some(IncidentDetail {
        session_id: session.id,
        title: session.title,
        task_preview,
        failure_types,
        tool_uses: facts.tool_uses,
        representative_local_read: facts.representative_local_read,
        representative_remote_use: facts.representative_remote_use,
        representative_edit: facts.representative_edit,
        representative_verify: facts.representative_verify,
        task_family: facts.task_family,
        detector_version: DETECTOR_VERSION.into(),
        reviewer_eligibility,
        source_of_truth_classification: facts.source_of_truth_classification,
        severity,
    })
}

/// List heuristic incidents from the most recent `limit` saved sessions.
pub fn audit_incidents(limit: usize) -> Vec<Incident> {
    let mut incidents = Vec::new();
    for session in load_recent_sessions(limit) {
        let facts = inspect_session(&session);
        for failure_type in detect_failure_types(&facts) {
            incidents.push(Incident {
                session_id: session.id.clone(),
                title: session.title.clone(),
                failure_type,
                severity: severity_for_failure(&facts).into(),
                task_family: facts.task_family.clone(),
                detector_version: DETECTOR_VERSION.into(),
                reviewer_eligibility: reviewer_eligibility(&facts).into(),
                source_of_truth_classification: facts.source_of_truth_classification.clone(),
                subsystem: facts.subsystem.clone(),
            });
        }
    }
    incidents
}

/// Build compact evidence packets from recent sessions with detected incidents.
pub fn audit_packets(limit: usize) -> Vec<EvidencePacket> {
    let mut packets = Vec::new();
    for session in load_recent_sessions(limit) {
        let facts = inspect_session(&session);
        let failure_types = detect_failure_types(&facts);
        if failure_types.is_empty() {
            continue;
        }
        packets.push(EvidencePacket {
            session_id: session.id.clone(),
            title: session.title.clone(),
            task_preview: session_task_preview(&session),
            task_family: facts.task_family.clone(),
            detector_version: DETECTOR_VERSION.into(),
            failure_types,
            severity: session_severity(&facts).into(),
            reviewer_eligibility: reviewer_eligibility(&facts).into(),
            source_of_truth_classification: facts.source_of_truth_classification.clone(),
            tool_sequence_summary: summarize_tool_sequence(&facts.tool_uses),
            representative_excerpts: facts.representative_excerpts,
            representative_spans: facts.representative_spans,
            supporting_counts: PacketCounts {
                tool_uses: facts.tool_uses.len(),
                local_reads: facts.local_read_count,
                remote_uses: facts.remote_use_count,
                edits: facts.edit_count,
                verify_signals: facts.verify_count,
            },
        });
    }
    packets
}

/// Deterministically cluster incidents by failure type, task family, subsystem, and detector version.
pub fn audit_clusters(limit: usize) -> Vec<IncidentCluster> {
    let mut groups: BTreeMap<(String, String, String, String), Vec<Incident>> = BTreeMap::new();
    for incident in audit_incidents(limit) {
        let key = (
            incident.failure_type.clone(),
            incident.task_family.clone(),
            incident.subsystem.clone(),
            incident.detector_version.clone(),
        );
        groups.entry(key).or_default().push(incident);
    }

    groups
        .into_iter()
        .map(
            |((failure_type, task_family, subsystem, detector_version), incidents)| {
                let count = incidents.len();
                let highest_severity = incidents
                    .iter()
                    .map(|incident| incident.severity.as_str())
                    .max_by_key(|severity| severity_rank(severity))
                    .unwrap_or("low")
                    .to_owned();
                let session_ids = incidents
                    .into_iter()
                    .map(|incident| incident.session_id)
                    .collect::<Vec<_>>();
                IncidentCluster {
                    cluster_key: format!(
                        "{}|{}|{}|{}",
                        failure_type, task_family, subsystem, detector_version
                    ),
                    failure_type,
                    task_family,
                    subsystem,
                    detector_version,
                    count,
                    highest_severity,
                    session_ids,
                }
            },
        )
        .collect()
}

/// Scan the most recent `limit` saved sessions and compute lightweight metrics.
pub fn audit_sessions(limit: usize) -> AuditSummary {
    let mut summary = AuditSummary::default();
    for session in load_recent_sessions(limit) {
        summary.sessions_scanned += 1;
        let facts = inspect_session(&session);

        if facts.has_project_instructions {
            summary.sessions_with_project_instructions += 1;
        }
        if facts.has_skill_load {
            summary.sessions_with_skill_loads += 1;
        }
        if facts.used_local_read && facts.used_remote {
            summary.wrong_source_sessions += 1;
        }
        if facts.local_task && facts.used_local_read && facts.used_remote {
            summary.premature_external_research_sessions += 1;
        }
        if facts.edited && !facts.verified {
            summary.missing_verification_sessions += 1;
        }
        summary.bash_verify_commands += facts.verify_count;
        for tool_use in &facts.tool_uses {
            if let Some(command) = tool_use.strip_prefix("Bash ") {
                match classify_shell_inspection(command) {
                    ShellInspectionKind::LocalRead => summary.shell_local_read_commands += 1,
                    ShellInspectionKind::FileCounting => summary.shell_file_counting_commands += 1,
                    ShellInspectionKind::VerifyOutputSlicing => {
                        summary.shell_verify_output_slicing_commands += 1
                    }
                    ShellInspectionKind::PatchStyleSearch => {
                        summary.shell_patch_style_search_commands += 1
                    }
                    ShellInspectionKind::None => {}
                }
            }
        }
    }
    summary
}

fn inspect_session(session: &Session) -> SessionFacts {
    let mut facts = SessionFacts {
        task_family: infer_task_family(session),
        ..SessionFacts::default()
    };

    for (message_index, msg) in session.messages.iter().enumerate() {
        if msg.role == Role::System && msg.text().contains("<project_instructions>") {
            facts.has_project_instructions = true;
        }
        if msg.role == Role::User {
            facts.local_task = facts.local_task || is_local_task(&msg.text());
        }
        for (block_index, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                let tool_use = format_tool_use(name, input);
                facts.tool_uses.push(tool_use.clone());
                capture_representative_excerpt(
                    &mut facts.representative_excerpts,
                    tool_use.clone(),
                );
                capture_representative_span(
                    &mut facts.representative_spans,
                    PacketSpanRef {
                        message_index,
                        block_index,
                        kind: "tool_use".into(),
                        preview: tool_use.clone(),
                    },
                );
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    match name.as_str() {
                        "Read" => {
                            let path = input["path"].as_str().unwrap_or("");
                            // Detect skill reads (canonical URI or legacy
                            // `.../SKILL.md` path) with the shared parser
                            // so audit metrics match what turn.rs emits
                            // to the UI.
                            if crate::config::skills::parse_skill_read_path(path).is_some() {
                                facts.has_skill_load = true;
                            } else if !path.is_empty() && !path.starts_with("artifact://") {
                                facts.used_local_read = true;
                                facts.local_read_count += 1;
                                facts.subsystem = "local_file_tools".into();
                                if facts.representative_local_read.is_none() {
                                    facts.representative_local_read = Some(path.to_owned());
                                }
                            }
                        }
                        "GhFile" | "GhLs" | "GhSearch" | "WebFetch" | "WebSearch" => {
                            facts.used_remote = true;
                            facts.remote_use_count += 1;
                            if facts.subsystem.is_empty() {
                                facts.subsystem = name.to_lowercase();
                            }
                            if facts.representative_remote_use.is_none() {
                                facts.representative_remote_use =
                                    Some(format!("{} {}", name, input));
                            }
                        }
                        "Edit" | "MultiEdit" | "Write" | "apply_patch" => {
                            facts.edited = true;
                            facts.edit_count += 1;
                            if facts.subsystem.is_empty() {
                                facts.subsystem = "editing".into();
                            }
                            if facts.representative_edit.is_none() {
                                facts.representative_edit = Some(format!("{} {}", name, input));
                            }
                        }
                        "Bash" | "exec_command" => {
                            let command = input["command"].as_str().unwrap_or("");
                            if command.is_empty() {
                                continue;
                            }
                            if facts.subsystem.is_empty() {
                                facts.subsystem = "bash".into();
                            }
                            if is_verify_command(command) {
                                facts.verified = true;
                                facts.verify_count += 1;
                                if facts.representative_verify.is_none() {
                                    facts.representative_verify = Some(command.to_owned());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if facts.subsystem.is_empty() {
        facts.subsystem = "general".into();
    }
    facts.source_of_truth_classification = source_of_truth_classification(&facts).into();
    facts
}

fn detect_failure_types(facts: &SessionFacts) -> Vec<String> {
    let mut failure_types = Vec::new();
    if facts.used_local_read && facts.used_remote {
        failure_types.push("wrong_source".into());
    }
    if facts.local_task && facts.used_local_read && facts.used_remote {
        failure_types.push("premature_external_research".into());
    }
    if facts.edited && !facts.verified {
        failure_types.push("missing_verification".into());
    }
    failure_types
}

fn infer_task_family(session: &Session) -> String {
    let user_text = session
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.text().to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    let mut local_audit_score = 0_u8;
    let mut local_code_score = 0_u8;
    let mut docs_score = 0_u8;
    let mut verification_score = 0_u8;

    for marker in [
        "prompt build hiện tại",
        "cơ chế prompt build",
        "audit",
        "kiểm tra cơ chế",
        "inspect current",
        "review current system",
        "instruction",
        "tool usage",
        "session",
        "context",
        "smart prompt",
    ] {
        if user_text.contains(marker) {
            local_audit_score += 2;
        }
    }

    for marker in [
        "repo này",
        "dự án này",
        "code hiện tại",
        "src/",
        "refactor",
        "review code",
        "local",
        "cargo",
        "rust",
    ] {
        if user_text.contains(marker) {
            local_code_score += 2;
        }
    }

    for marker in ["rfc", "documentation", "docs/", "spec", "thiết kế"] {
        if user_text.contains(marker) {
            docs_score += 2;
        }
    }

    for marker in [
        "test",
        "verify",
        "regression",
        "cargo test",
        "cargo check",
        "clippy",
    ] {
        if user_text.contains(marker) {
            verification_score += 2;
        }
    }

    if is_local_task(&user_text) {
        local_code_score += 1;
        local_audit_score += 1;
    }

    let scored_families = [
        ("local_audit", local_audit_score),
        ("local_code", local_code_score),
        ("docs", docs_score),
        ("verification", verification_score),
    ];

    let (family, score) = scored_families
        .into_iter()
        .max_by_key(|(_, score)| *score)
        .unwrap_or(("general", 0));

    if score == 0 {
        "general".into()
    } else {
        family.into()
    }
}

fn session_task_preview(session: &Session) -> String {
    session
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .map(|message| message.text().lines().next().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

fn summarize_tool_sequence(tool_uses: &[String]) -> Vec<String> {
    tool_uses.iter().take(8).cloned().collect::<Vec<_>>()
}

fn capture_representative_excerpt(excerpts: &mut Vec<String>, excerpt: String) {
    if excerpts.len() < PACKET_EXCERPT_LIMIT {
        excerpts.push(excerpt);
    }
}

fn capture_representative_span(spans: &mut Vec<PacketSpanRef>, span: PacketSpanRef) {
    if spans.len() < PACKET_EXCERPT_LIMIT {
        spans.push(span);
    }
}

fn source_of_truth_classification(facts: &SessionFacts) -> &'static str {
    match (facts.used_local_read, facts.used_remote) {
        (true, false) => "local_only",
        (true, true) => "mixed",
        (false, true) => "external_required",
        (false, false) => "local_first",
    }
}

fn reviewer_eligibility(facts: &SessionFacts) -> &'static str {
    if facts.local_task && facts.used_remote {
        return "immediate";
    }
    if facts.edited && !facts.verified {
        return "batch";
    }
    "manual_only"
}

fn session_severity(facts: &SessionFacts) -> &'static str {
    if facts.local_task && facts.used_remote {
        return "high";
    }
    if facts.edited && !facts.verified {
        return "medium";
    }
    "low"
}

fn severity_for_failure(facts: &SessionFacts) -> &'static str {
    session_severity(facts)
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "high" => 3,
        "medium" => 2,
        _ => 1,
    }
}

fn format_tool_use(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "exec_command" => {
            format!("{} {}", name, input["command"].as_str().unwrap_or(""))
        }
        _ => format!("{} {}", name, input),
    }
}

fn is_local_task(text: &str) -> bool {
    let lower = text.to_lowercase();
    let external_markers = [
        "http://",
        "https://",
        "github.com",
        "latest",
        "current",
        "news",
        "oauth",
        "api",
        "provider",
        "docs",
        "documentation",
        "upstream",
        "kiro cli",
        "anthropic",
        "openai",
    ];
    let local_markers = [
        "repo này",
        "dự án này",
        "code hiện tại",
        "prompt build hiện tại",
        "src/",
        "refactor",
        "review code",
        "local",
        "cargo",
        "rust",
    ];
    let has_external = external_markers.iter().any(|marker| lower.contains(marker));
    let has_local = local_markers.iter().any(|marker| lower.contains(marker));
    has_local && !has_external
}

fn load_recent_sessions(limit: usize) -> Vec<Session> {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<Session> = entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let raw = fs::read_to_string(entry.path()).ok()?;
            serde_json::from_str::<Session>(&raw).ok()
        })
        .collect();
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    sessions.truncate(limit);
    sessions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellInspectionKind {
    None,
    LocalRead,
    FileCounting,
    VerifyOutputSlicing,
    PatchStyleSearch,
}

fn classify_shell_inspection(command: &str) -> ShellInspectionKind {
    let trimmed = command.trim();
    if (trimmed.contains("cargo test")
        || trimmed.contains("cargo check")
        || trimmed.contains("cargo clippy")
        || trimmed.contains("pytest"))
        && (trimmed.contains("| tail") || trimmed.contains("| head"))
    {
        return ShellInspectionKind::VerifyOutputSlicing;
    }
    if trimmed.contains("wc -")
        || trimmed.contains("find ")
        || trimmed.contains(" ls")
        || trimmed.starts_with("ls ")
        || trimmed.contains("tree")
    {
        return ShellInspectionKind::FileCounting;
    }
    if trimmed.contains("rg ") || trimmed.contains("grep ") {
        return ShellInspectionKind::PatchStyleSearch;
    }
    if trimmed.contains("cat ")
        || trimmed.contains("sed ")
        || trimmed.contains("head ")
        || trimmed.contains("tail ")
        || trimmed.contains("python3 - <<")
        || trimmed.contains("python - <<")
    {
        return ShellInspectionKind::LocalRead;
    }
    ShellInspectionKind::None
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::SessionUsage;
    use crate::core::types::Message;

    fn make_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            origin: None,
        }
    }

    fn make_session(messages: Vec<Message>) -> Session {
        Session {
            id: "s1".into(),
            title: "test session".into(),
            created_at: "2026-04-14T00:00:00Z".into(),
            updated_at: "2026-04-14T00:00:00Z".into(),
            messages,
            usage: SessionUsage::default(),
            turn_durations: Vec::new(),
            evidence: Default::default(),
            provider_state: Default::default(),
        }
    }

    #[test]
    fn verify_command_detection() {
        assert!(is_verify_command("cargo check && cargo test"));
        assert!(is_verify_command("pytest -q"));
        assert!(!is_verify_command("git status"));
    }

    #[test]
    fn shell_inspection_classification() {
        assert_eq!(
            classify_shell_inspection("rg prompt src"),
            ShellInspectionKind::PatchStyleSearch
        );
        assert_eq!(
            classify_shell_inspection("cat Cargo.toml"),
            ShellInspectionKind::LocalRead
        );
        assert_eq!(
            classify_shell_inspection("wc -c AGENTS.md"),
            ShellInspectionKind::FileCounting
        );
        assert_eq!(
            classify_shell_inspection("cargo check 2>&1 | tail -20"),
            ShellInspectionKind::VerifyOutputSlicing
        );
        assert_eq!(
            classify_shell_inspection("cargo check"),
            ShellInspectionKind::None
        );
    }

    #[test]
    fn detects_failure_types_from_session_facts() {
        let session = make_session(vec![make_message(
            Role::User,
            "review code hiện tại trong src/",
        )]);
        let mut facts = inspect_session(&session);
        facts.used_local_read = true;
        facts.used_remote = true;
        facts.edited = true;
        facts.verified = false;
        let failures = detect_failure_types(&facts);
        assert!(failures.iter().any(|failure| failure == "wrong_source"));
        assert!(
            failures
                .iter()
                .any(|failure| failure == "premature_external_research")
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure == "missing_verification")
        );
    }

    #[test]
    fn infers_local_task_family() {
        let session = make_session(vec![make_message(
            Role::User,
            "refactor code hiện tại trong src/",
        )]);
        assert_eq!(infer_task_family(&session), "local_code");
        assert!(is_local_task("review code hiện tại trong repo này"));
        assert!(!is_local_task("look up latest docs on github.com"));
    }

    #[test]
    fn infers_local_audit_task_family() {
        let session = make_session(vec![make_message(
            Role::User,
            "kiểm tra cơ chế prompt build hiện tại trong repo này",
        )]);
        assert_eq!(infer_task_family(&session), "local_audit");
    }

    #[test]
    fn local_audit_outranks_docs_when_both_signals_exist() {
        let session = make_session(vec![make_message(
            Role::User,
            "audit RFC prompt build hiện tại trong repo này và kiểm tra context hiện tại",
        )]);
        assert_eq!(infer_task_family(&session), "local_audit");
    }

    #[test]
    fn clusters_incidents_deterministically() {
        let incidents = vec![
            Incident {
                session_id: "a".into(),
                title: "one".into(),
                failure_type: "wrong_source".into(),
                severity: "high".into(),
                task_family: "local_code".into(),
                detector_version: DETECTOR_VERSION.into(),
                reviewer_eligibility: "immediate".into(),
                source_of_truth_classification: "mixed".into(),
                subsystem: "ghfile".into(),
            },
            Incident {
                session_id: "b".into(),
                title: "two".into(),
                failure_type: "wrong_source".into(),
                severity: "medium".into(),
                task_family: "local_code".into(),
                detector_version: DETECTOR_VERSION.into(),
                reviewer_eligibility: "batch".into(),
                source_of_truth_classification: "mixed".into(),
                subsystem: "ghfile".into(),
            },
        ];
        let mut groups: BTreeMap<(String, String, String, String), Vec<Incident>> = BTreeMap::new();
        for incident in incidents {
            let key = (
                incident.failure_type.clone(),
                incident.task_family.clone(),
                incident.subsystem.clone(),
                incident.detector_version.clone(),
            );
            groups.entry(key).or_default().push(incident);
        }
        assert_eq!(groups.len(), 1);
        let clustered = groups.into_values().next().unwrap_or_default();
        assert_eq!(clustered.len(), 2);
    }
}
