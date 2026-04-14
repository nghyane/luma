use crate::core::audit::audit_show;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub route: String,
    pub confidence: String,
    pub target_layers: Vec<String>,
    pub reason: String,
    pub note: String,
}

/// Build a first-pass heuristic proposal from one audited session.
pub fn propose_from_session(session_id: &str) -> Option<Proposal> {
    let detail = audit_show(session_id)?;

    let has = |name: &str| detail.failure_types.iter().any(|f| f == name);

    if has("mixed_local_remote_source") || has("premature_external_research") {
        return Some(Proposal {
            route: "patch".into(),
            confidence: "low".into(),
            target_layers: vec!["base prompt".into(), "tool boundary descriptions".into()],
            reason: "Local file reads and remote/external lookups appeared in the same local-repo-focused session.".into(),
            note: "Heuristic suggestion only. Confirm from the session trace before editing prompt or tool text.".into(),
        });
    }

    if has("edited_without_verify") {
        return Some(Proposal {
            route: "agents_or_prompt".into(),
            confidence: "low".into(),
            target_layers: vec!["AGENTS.md".into(), "smart prompt verification guidance".into()],
            reason: "The session appears to contain file edits without a recognized verification command.".into(),
            note: "This heuristic can produce false positives if verification happened outside detected command patterns.".into(),
        });
    }

    Some(Proposal {
        route: "no_action".into(),
        confidence: "low".into(),
        target_layers: vec![],
        reason: "No current heuristic route matched this session.".into(),
        note: "Inspect the session manually or extend the taxonomy before making changes.".into(),
    })
}
