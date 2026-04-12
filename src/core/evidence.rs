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
//! Ingestion is crash-safe via tmp→fsync→rename (see [`ingest`]): the
//! record is only appended after the blob is durable, so any crash leaves
//! either nothing at all or a full blob with a referencing record.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

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

/// Threshold above which a tool result is promoted to evidence.
///
/// Derived from the feasibility scan in `Session::rfc_feasibility_scan`:
/// 8K captures 13.7% of tool results and shrinks transcript tool_result
/// bytes by ~73%. 16K leaves too much inline; 4K promotes too many small
/// outputs.
pub const EVIDENCE_PROMOTION_THRESHOLD: usize = 8_000;

/// Max chars retained in the in-transcript summary when a blob is spilled.
///
/// Large enough to carry one useful line (a filename, an exit code, the
/// first error) but small enough that a transcript full of evidence refs
/// stays compact.
pub const EVIDENCE_SUMMARY_CHARS: usize = 200;

/// A ready-to-persist record, separated from [`EvidenceRecord`] so that
/// [`classify`] stays a pure function over `(tool_name, args, result)` and
/// [`ingest`] owns id generation, blob I/O, and turn bookkeeping.
#[derive(Debug, Clone)]
pub struct EvidenceDraft {
    pub kind: EvidenceKind,
    pub summary: String,
    /// Full blob to persist. `None` means "keep inline" — the caller should
    /// not have invoked ingest.
    pub blob: String,
    pub related_files: Vec<String>,
}

impl EvidenceStore {
    /// Persist `draft` for `session_id`'s turn `turn_index` and append the
    /// resulting record.
    ///
    /// Write order is tmp → fsync → rename → append record, so a crash at
    /// any point leaves the store consistent: either no record exists
    /// (blob may be orphaned, harmless) or the blob is durable before the
    /// record references it.
    ///
    /// `evidence_dir` is the session's `evidence/` directory; created if
    /// absent. Returns the new record's id so the caller can wire it into
    /// the tool_result block.
    pub fn ingest(
        &mut self,
        evidence_dir: &Path,
        turn_index: usize,
        tool_use_id: &str,
        draft: EvidenceDraft,
    ) -> std::io::Result<String> {
        fs::create_dir_all(evidence_dir)?;
        let id = next_evidence_id(self);
        let filename = format!("{id}.txt");
        let final_path = evidence_dir.join(&filename);
        let tmp_path = evidence_dir.join(format!("{id}.txt.tmp"));

        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(draft.blob.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;

        let chars = draft.blob.chars().count();
        self.records.push(EvidenceRecord {
            id: id.clone(),
            kind: draft.kind,
            tool_use_id: tool_use_id.to_owned(),
            summary: draft.summary,
            blob_path: Some(format!("evidence/{filename}")),
            chars,
            turn_index,
            related_files: draft.related_files,
        });
        Ok(id)
    }
}

/// Generate a monotonic id for a new record.
///
/// Unix-ms alone collides when several tools finish inside the same
/// millisecond (parallel execution in `run_turn` makes this common), so a
/// suffix counter advances past the latest id in the store.
fn next_evidence_id(store: &EvidenceStore) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let base = format!("ev_{ts:x}");
    if !store.records.iter().any(|r| r.id.starts_with(&base)) {
        return base;
    }
    for n in 1.. {
        let candidate = format!("{base}_{n}");
        if !store.records.iter().any(|r| r.id == candidate) {
            return candidate;
        }
    }
    unreachable!("u64 counter exhausted")
}

/// Decide whether and how to persist a tool result.
///
/// Returns `None` when the result is small enough to keep inline. The
/// caller (turn loop) owns the promotion threshold — this function only
/// shapes the evidence draft.
pub fn classify(tool_name: &str, args: &serde_json::Value, result: &str) -> Option<EvidenceDraft> {
    let kind = match tool_name {
        "Read" => EvidenceKind::ReadExcerpt,
        "Grep" | "GhSearch" => EvidenceKind::GrepResult,
        "Bash" | "exec_command" | "shell" => {
            if is_build_command(args) {
                EvidenceKind::BuildLog
            } else {
                EvidenceKind::BashLog
            }
        }
        _ => EvidenceKind::Other,
    };
    Some(EvidenceDraft {
        kind,
        summary: build_summary(tool_name, args, result),
        blob: result.to_owned(),
        related_files: extract_related_files(tool_name, args),
    })
}

/// Whether a shell command is a verification invocation (build/test/lint).
///
/// Matches the first token past common prefixes so `cargo test …` or
/// `sh -c 'cargo build'` both register.
fn is_build_command(args: &serde_json::Value) -> bool {
    let Some(cmd) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let lower = cmd.to_ascii_lowercase();
    const VERIFIERS: &[&str] = &[
        "cargo build",
        "cargo check",
        "cargo test",
        "cargo clippy",
        "cargo run",
        "npm test",
        "npm run build",
        "pnpm test",
        "pnpm build",
        "pytest",
        "go test",
        "go build",
        "make test",
        "make build",
    ];
    VERIFIERS.iter().any(|v| lower.contains(v))
}

/// Extract file paths mentioned in tool args.
///
/// Drives `files_in_play` intersection when the planner picks evidence to
/// reload; returning an empty vec is fine — the planner falls back to
/// recency.
fn extract_related_files(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let field = match tool_name {
        "Read" | "Edit" | "Write" => "path",
        "Grep" => "path",
        _ => return Vec::new(),
    };
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| vec![s.to_owned()])
        .unwrap_or_default()
}

/// Build the short summary that replaces the full result in the transcript.
///
/// Prefers structured signals (exit code for bash, line count for read)
/// over raw first-line excerpts because the summary is what the model will
/// actually see on replay.
fn build_summary(tool_name: &str, args: &serde_json::Value, result: &str) -> String {
    let lines = result.lines().count();
    let raw = match tool_name {
        "Read" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            format!("{path} ({lines} lines, stored as evidence)")
        }
        "Grep" | "GhSearch" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("grep {pattern:?}: {lines} lines, stored as evidence")
        }
        "Bash" | "exec_command" | "shell" => {
            let exit = result
                .rfind("[exit code: ")
                .and_then(|i| result[i + 12..].split(']').next())
                .unwrap_or("?");
            let cmd_preview: String = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            format!("$ {cmd_preview} → exit {exit}, {lines} lines, stored as evidence")
        }
        _ => format!("{tool_name}: {lines} lines, stored as evidence"),
    };
    truncate_summary(&raw)
}

/// Clamp a summary to `EVIDENCE_SUMMARY_CHARS` characters on a code-point
/// boundary.
fn truncate_summary(s: &str) -> String {
    if s.chars().count() <= EVIDENCE_SUMMARY_CHARS {
        return s.to_owned();
    }
    s.chars().take(EVIDENCE_SUMMARY_CHARS).collect()
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

    #[test]
    fn classify_read_is_read_excerpt() {
        let args = serde_json::json!({"path": "/tmp/x.rs"});
        let draft = classify("Read", &args, "line\n".repeat(100).as_str()).unwrap();
        assert_eq!(draft.kind, EvidenceKind::ReadExcerpt);
        assert_eq!(draft.related_files, vec!["/tmp/x.rs"]);
        assert!(draft.summary.contains("/tmp/x.rs"));
        assert!(draft.summary.contains("100 lines"));
    }

    #[test]
    fn classify_grep_is_grep_result() {
        let args = serde_json::json!({"pattern": "fn main", "path": "src/"});
        let draft = classify("Grep", &args, "a\nb\n").unwrap();
        assert_eq!(draft.kind, EvidenceKind::GrepResult);
        assert_eq!(draft.related_files, vec!["src/"]);
    }

    #[test]
    fn classify_bash_build_command_promotes_to_build_log() {
        let args = serde_json::json!({"command": "cargo test --all"});
        let draft = classify("Bash", &args, "ok\n[exit code: 0]").unwrap();
        assert_eq!(draft.kind, EvidenceKind::BuildLog);
    }

    #[test]
    fn classify_bash_plain_command_is_bash_log() {
        let args = serde_json::json!({"command": "ls /tmp"});
        let draft = classify("exec_command", &args, "a\nb\n[exit code: 0]").unwrap();
        assert_eq!(draft.kind, EvidenceKind::BashLog);
        assert!(draft.summary.contains("exit 0"));
    }

    #[test]
    fn classify_unknown_tool_is_other() {
        let args = serde_json::json!({});
        let draft = classify("WebFetch", &args, "payload").unwrap();
        assert_eq!(draft.kind, EvidenceKind::Other);
        assert!(draft.related_files.is_empty());
    }

    #[test]
    fn summary_is_bounded() {
        let args = serde_json::json!({"path": "a".repeat(1_000)});
        let draft = classify("Read", &args, "x").unwrap();
        assert!(draft.summary.chars().count() <= EVIDENCE_SUMMARY_CHARS);
    }

    #[test]
    fn ingest_writes_blob_and_appends_record() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = EvidenceStore::default();
        let draft = EvidenceDraft {
            kind: EvidenceKind::ReadExcerpt,
            summary: "src/main.rs (200 lines)".into(),
            blob: "line 1\nline 2\n".into(),
            related_files: vec!["src/main.rs".into()],
        };

        let id = store.ingest(tmp.path(), 3, "tc_1", draft).unwrap();

        assert_eq!(store.records.len(), 1);
        let rec = &store.records[0];
        assert_eq!(rec.id, id);
        assert_eq!(rec.turn_index, 3);
        assert_eq!(rec.tool_use_id, "tc_1");
        assert_eq!(rec.chars, 14);
        let blob_rel = rec.blob_path.as_deref().unwrap();
        assert!(blob_rel.starts_with("evidence/"));
        let blob = std::fs::read_to_string(tmp.path().join(format!("{id}.txt"))).unwrap();
        assert_eq!(blob, "line 1\nline 2\n");
    }

    #[test]
    fn ingest_generates_unique_ids_under_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = EvidenceStore::default();
        // Ingest twice fast enough that the millisecond timestamp collides
        // on most machines — the counter suffix must disambiguate.
        let d = || EvidenceDraft {
            kind: EvidenceKind::Other,
            summary: "s".into(),
            blob: "b".into(),
            related_files: vec![],
        };
        let a = store.ingest(tmp.path(), 0, "tc_a", d()).unwrap();
        let b = store.ingest(tmp.path(), 0, "tc_b", d()).unwrap();
        assert_ne!(a, b);
        assert_eq!(store.records.len(), 2);
    }

    #[test]
    fn ingest_leaves_no_tmp_file_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = EvidenceStore::default();
        let draft = EvidenceDraft {
            kind: EvidenceKind::Other,
            summary: "s".into(),
            blob: "b".into(),
            related_files: vec![],
        };
        store.ingest(tmp.path(), 0, "tc_1", draft).unwrap();
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "tmp file leaked: {leftover:?}");
    }
}
