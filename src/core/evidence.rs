//! Evidence store — addressable overflow for long tool outputs.
//!
//! Large tool results (read excerpts, grep dumps, build logs) bloat the
//! transcript and hit the agent's safety cap before the model even sees
//! them. The evidence store promotes oversized results to external blobs
//! while keeping a short summary in the transcript, so:
//!
//! * the provider sees a bounded, cache-friendly history,
//! * the raw output is still available on disk for debugging, and
//! * a future context planner can reload an excerpt on demand.
//!
//! This module currently defines the data model only; ingestion
//! (classify + crash-safe blob write) lands in a follow-up commit per
//! `docs/rfcs/evidence-backed-handoff.md` §7.3. Records on disk today are
//! always fully persisted — there is no half-written state to track.

use serde::{Deserialize, Serialize};

/// Coarse classification of a persisted tool output.
///
/// Used by the planner to bias selection (e.g. prefer the latest `BuildLog`
/// when deciding what to surface alongside a failing verification). Kept
/// small and open-ended via `Other` so new tools can ingest without
/// changing the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// File read excerpt (`Read`).
    ReadExcerpt,
    /// Grep / search output (`Grep`, `GhSearch`).
    GrepResult,
    /// Shell command output (`Bash`, `exec_command`).
    BashLog,
    /// Build / test / clippy output — a `BashLog` whose command matches a
    /// verification invocation. Promoted separately because the planner
    /// treats verification as the highest-priority evidence.
    BuildLog,
    /// Anything else — GitHub fetch, web fetch, unrecognized tool.
    Other,
}

/// A persisted tool output that was too large to keep inline in the
/// transcript.
///
/// `blob_path` is a path relative to the session directory
/// (`sessions/{session_id}/evidence/{id}.txt`). Absent means the summary
/// alone carries the full content — reserved for tools that can express
/// their result compactly (not used yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub kind: EvidenceKind,
    /// Matches the originating `ContentBlock::ToolUse.id`.
    pub tool_use_id: String,
    /// Short human/model-facing synopsis (~200 chars) written into the
    /// transcript alongside the evidence reference.
    pub summary: String,
    /// Relative blob path under the session directory; `None` when the
    /// summary is the full content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_path: Option<String>,
    /// Byte length of the original (pre-summary) tool result.
    pub chars: usize,
    /// Index into `Session.messages` of the assistant turn that issued the
    /// tool call. Used by the planner to reason about recency.
    pub turn_index: usize,
    /// Paths mentioned by the tool call (e.g. `Read.path`, `Grep.path`).
    /// Drives `files_in_play` intersection when the planner selects
    /// evidence to load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_files: Vec<String>,
}

/// Per-session evidence store. Append-only; records are never mutated or
/// removed once persisted (GC is tied to session deletion).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceStore {
    #[serde(default)]
    pub records: Vec<EvidenceRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip() {
        let rec = EvidenceRecord {
            id: "ev_abc".into(),
            kind: EvidenceKind::ReadExcerpt,
            tool_use_id: "tc_1".into(),
            summary: "src/main.rs (180 lines)".into(),
            blob_path: Some("evidence/ev_abc.txt".into()),
            chars: 12_345,
            turn_index: 4,
            related_files: vec!["src/main.rs".into()],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: EvidenceRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ev_abc");
        assert_eq!(back.kind, EvidenceKind::ReadExcerpt);
        assert_eq!(back.blob_path.as_deref(), Some("evidence/ev_abc.txt"));
        assert_eq!(back.chars, 12_345);
    }

    #[test]
    fn store_defaults_to_empty() {
        let store = EvidenceStore::default();
        assert!(store.records.is_empty());
        let json = serde_json::to_string(&store).unwrap();
        let back: EvidenceStore = serde_json::from_str(&json).unwrap();
        assert!(back.records.is_empty());
    }

    #[test]
    fn missing_optional_fields_deserialize() {
        // blob_path and related_files are elided when empty.
        let minimal = r#"{
            "id": "ev_1",
            "kind": "bash_log",
            "tool_use_id": "tc_7",
            "summary": "exit 0",
            "chars": 42,
            "turn_index": 2
        }"#;
        let rec: EvidenceRecord = serde_json::from_str(minimal).unwrap();
        assert!(rec.blob_path.is_none());
        assert!(rec.related_files.is_empty());
    }
}
