//! Context planner — build the message sequence sent to the provider.
//!
//! The planner is the single choke point between `Session.messages` (the
//! canonical transcript) and `provider.stream()`. It runs once per turn
//! iteration and owns:
//!
//! * which messages to include,
//! * whether to inject evidence blobs pulled from the evidence store, and
//! * how to shape the prelude (system prompt, handoff snapshot) so the
//!   cache prefix stays stable across turns.
//!
//! ## Evidence selection
//!
//! Evidence injection targets the duplicate-tool-call loop: the agent
//! calls `Read` on the same file repeatedly because the transcript only
//! carries short summaries (e.g. `"src/parser.rs (520 lines, stored as
//! evidence)"`). Without the planner, a session that `Read`s the same
//! file three times creates three evidence blobs and three tool
//! round-trips.
//!
//! Observed distribution (ses_19d802b2734): duplicate reads cluster
//! *inside a single assistant turn*, not across turns. The planner
//! therefore anchors on the trailing user message of the transcript
//! even when it only carries `tool_result` blocks — that is the sole
//! insertion point visible to the provider on the very next
//! `stream()` call within the same tool loop.
//!
//! ## Injection shape
//!
//! Evidence text is wrapped in `<system-reminder>…</system-reminder>`
//! and smooshed into the **content of the tool_result block whose
//! `tool_use_id` produced it**. Two invariants drive this:
//!
//! 1. **Claude expects this shape.** Claude Code uses the same
//!    primitives (`src/utils/messages.ts`:
//!    `smooshIntoToolResult` / `wrapInSystemReminder`). A raw Text
//!    sibling after a `tool_result` looks to the model like
//!    user-authored content (defensive prompt-injection reflex —
//!    agent reads files with `limit=302` instead of trusting the
//!    content) **and** renders on the wire as
//!    `</function_results>\n\nHuman:<…>`, teaching the model to
//!    emit `Human:` at bare tails (Claude Code A/B
//!    `sai-20260310-161901` went from 92% to 0% after the switch).
//!
//! 2. **Cache stability.** Claude prompt cache matches by byte
//!    prefix. Siting each chunk at the tool_result that produced
//!    its record means `msgs[M]` has deterministic bytes across
//!    every subsequent iteration: iter N+1 smooshes any new
//!    evidence into a *different* message further down, leaving
//!    `msgs[0..N]` byte-identical to what iter N sent. Phase A v1
//!    sited every chunk at the *trailing anchor*, which shifts
//!    every iter — scan of ses_19d80798d45 showed only 72-84% cache
//!    hit in the first 15 iters (25K+ chars invalidated per
//!    request). The tool_use_id pin keeps hit above 95% once the
//!    evidence store is populated (§2.6).
//!
//! When the anchor is a plain user-text turn (no tool_use ahead
//! of it), there is no tool_result to smoosh into and no evidence
//! to inject at that site — the planner just passes through.
//!
//! ## Selection rule (RFC §9, one rule only)
//!
//! 1. Consider records whose `turn_index` falls in the recent window
//!    (last [`RECENT_TURN_WINDOW`] assistant turns).
//! 2. Group by `related_files` — keep only the latest record per file
//!    so the same file does not appear twice.
//! 3. Rank by `turn_index` descending (most recent first).
//! 4. Greedy fit under [`EVIDENCE_INJECTION_BUDGET_CHARS`] — skip any
//!    record that would overflow rather than truncating it.
//! 5. Skip records already injected in the transcript (detected by
//!    the stable header `# Retrieved evidence: {id}`). Idempotent
//!    across tool-loop iterations.

use crate::core::evidence::{EvidenceRecord, EvidenceStore};
use crate::core::types::{ContentBlock, Message, Role};
use std::collections::HashSet;
use std::path::Path;

/// How many turns back the planner considers when scanning evidence.
///
/// Picked from the feasibility session (ses_19d802b2734): duplicate
/// reads clustered within ~10–20 turns. 15 covers the common window
/// without loading stale evidence from earlier task phases.
pub const RECENT_TURN_WINDOW: usize = 15;

/// Total char budget for injected evidence blobs per turn.
///
/// Anchored to p90 observed blob size (~10K) times ~3: enough for three
/// typical reads without dominating the prompt. Exceeded budget means
/// a record is skipped, never partially loaded — partial evidence is
/// worse than none.
pub const EVIDENCE_INJECTION_BUDGET_CHARS: usize = 32_000;

/// Inputs to the planner. Everything the planner needs is passed
/// explicitly so the function stays pure and testable.
pub struct PlanInput<'a> {
    pub transcript: &'a [Message],
    pub evidence: &'a EvidenceStore,
    /// Session asset root (`sessions/{id}/`). The planner resolves
    /// `EvidenceRecord.blob_path` against this. `None` disables
    /// evidence injection — used by tests that don't write blobs and
    /// by the passthrough fallback.
    pub assets_dir: Option<&'a Path>,
}

/// Build the prepared message sequence for a single provider call.
///
/// Passthrough semantics when no evidence can be injected (empty store,
/// no assets dir, no user anchor). Evidence is wrapped in
/// `<system-reminder>` and either smooshed into the last tool_result's
/// content (mid tool-loop) or prepended as a Text block (plain user
/// turn). See module doc for the rationale.
/// Build the prepared message sequence for a single provider call.
///
/// Passthrough semantics when no evidence can be injected (empty store,
/// no assets dir, no user anchor). Evidence is wrapped in
/// `<system-reminder>` and smooshed into the tool_result block whose
/// `tool_use_id` produced it — pinning the injection site to a
/// deterministic message keeps prompt-cache prefixes stable across
/// tool-loop iterations (see §2.6 — phase A v1 mutated the trailing
/// anchor, which shifted every iter and invalidated 25K+ chars per
/// request).
pub fn build_prepared_messages(input: PlanInput<'_>) -> Vec<Message> {
    let mut out: Vec<Message> = input.transcript.to_vec();

    let Some(assets_dir) = input.assets_dir else {
        return out;
    };
    if find_injection_anchor(&out).is_none() {
        return out;
    }
    let already = collect_injected_ids(&out);
    let current_turn = out.len();
    let selected = select_evidence(&input.evidence.records, current_turn, &already);
    if selected.is_empty() {
        return out;
    }

    // Build the chunk + target pairs before mutating so a failing blob
    // load doesn't leave half-injected state.
    struct Injection<'a> {
        rec: &'a EvidenceRecord,
        chunk: String,
    }
    let mut injections: Vec<Injection<'_>> = Vec::with_capacity(selected.len());
    for rec in selected {
        match load_evidence_text(assets_dir, rec) {
            Ok(text) => injections.push(Injection {
                rec,
                chunk: wrap_system_reminder(&text),
            }),
            Err(e) => {
                crate::dbg_log!("context_plan: skip evidence {}: {}", rec.id, e);
            }
        }
    }
    if injections.is_empty() {
        return out;
    }

    // Site each chunk at the tool_result that produced its record.
    // Deterministic: a record at turn T always smooshes into msg index
    // M where msg[M] carries the matching tool_use_id. Byte content of
    // every msg < M stays identical across iterations → cache stable.
    for inj in injections {
        match find_tool_result_site(&out, &inj.rec.tool_use_id) {
            Some((msg_idx, block_idx)) => {
                if let ContentBlock::ToolResult { content, .. } =
                    &mut out[msg_idx].content[block_idx]
                {
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str(&inj.chunk);
                }
            }
            None => {
                crate::dbg_log!(
                    "context_plan: no tool_result site for evidence {} \
                     (tool_use_id={}); skipping",
                    inj.rec.id,
                    inj.rec.tool_use_id
                );
            }
        }
    }
    out
}

/// Locate the `(msg_index, block_index)` of the tool_result carrying
/// `tool_use_id`. Returns `None` if the transcript does not contain
/// such a block — either the record is stale across a session format
/// change or the tool_result was pruned.
fn find_tool_result_site(msgs: &[Message], tool_use_id: &str) -> Option<(usize, usize)> {
    for (i, m) in msgs.iter().enumerate() {
        if m.role != Role::User {
            continue;
        }
        for (j, b) in m.content.iter().enumerate() {
            if let ContentBlock::ToolResult {
                tool_use_id: id, ..
            } = b
                && id == tool_use_id
            {
                return Some((i, j));
            }
        }
    }
    None
}

/// Wrap evidence text in a Claude-recognised system-reminder envelope.
///
/// The tags are a trained signal: the model treats the contents as
/// system-authored metadata instead of user-authored text, which
/// suppresses the prompt-injection defensive reflex observed in
/// sessions before the wrapper landed.
fn wrap_system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// Return the index of the trailing user message — the injection point.
///
/// Anchors on *any* user message at the tail (text, paste, or
/// tool_result). Skips when the tail is an assistant message: a
/// streamed-out assistant turn without pending tool calls is already
/// the final wire payload; there is nothing to inject before. Also
/// skips when the tail is system (boot state) or the transcript is
/// empty.
///
/// A user-text turn mid-transcript (e.g. the turn that started the
/// current assistant work) is deliberately not anchored: rewriting it
/// while the assistant is still executing tool calls would invalidate
/// the cache prefix and mis-align evidence with tool_uses already
/// satisfied.
fn find_injection_anchor(msgs: &[Message]) -> Option<usize> {
    let last = msgs.len().checked_sub(1)?;
    if msgs[last].role != Role::User {
        return None;
    }
    Some(last)
}

/// Collect evidence ids already materialised in the transcript.
///
/// Used to skip double-injection across tool-loop iterations. The
/// stable marker is `# Retrieved evidence: {id}` — it appears either
/// inside a `ContentBlock::Text.text` (plain user-turn anchor) or
/// inside a `ContentBlock::ToolResult.content` string (tool_result
/// anchor after smoosh). Scan both.
fn collect_injected_ids(msgs: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for m in msgs {
        for b in &m.content {
            match b {
                ContentBlock::Text { text } | ContentBlock::Paste { text } => {
                    collect_ids_from_haystack(text, &mut ids);
                }
                ContentBlock::ToolResult { content, .. } => {
                    collect_ids_from_haystack(content, &mut ids);
                }
                _ => {}
            }
        }
    }
    ids
}

/// Scan `haystack` for every `# Retrieved evidence: {id}` occurrence
/// and record the id. Multiple evidence blocks may share one string
/// (several chunks smooshed into the same tool_result content).
fn collect_ids_from_haystack(haystack: &str, ids: &mut HashSet<String>) {
    const MARKER: &str = "# Retrieved evidence: ";
    let mut cursor = 0;
    while let Some(rel) = haystack[cursor..].find(MARKER) {
        let start = cursor + rel + MARKER.len();
        let end = haystack[start..]
            .find(|c: char| c.is_whitespace() || c == '(')
            .map(|n| start + n)
            .unwrap_or(haystack.len());
        if end > start {
            ids.insert(haystack[start..end].to_owned());
        }
        cursor = end;
    }
}

/// Select evidence records to inject, in chronological order.
///
/// Two lanes driven by whether a record carries `related_files`:
///
/// * **File-based lane** (Read/Edit/Write/Grep). Dedup latest-per-file
///   so the same file is not injected twice, then fall into the
///   shared budget.
/// * **No-file lane** (Bash/GhFile/WebFetch). No dedup key — each
///   invocation is its own artifact (two `git diff` calls produce
///   two unrelated diffs). Kept by recency only.
///
/// Both lanes share the same recent-window filter, idempotency
/// check, and char budget. The two-lane split was added after a
/// session where `Bash git diff` evidence was promoted but never
/// re-injected because the original single-lane filter required a
/// non-empty `related_files`, silently dropping the entire class
/// of tool outputs (ses §2.5).
fn select_evidence<'a>(
    records: &'a [EvidenceRecord],
    current_turn: usize,
    already_injected: &HashSet<String>,
) -> Vec<&'a EvidenceRecord> {
    let window_start = current_turn.saturating_sub(RECENT_TURN_WINDOW);
    let in_window: Vec<&EvidenceRecord> = records
        .iter()
        .filter(|r| {
            r.blob_path.is_some()
                && r.turn_index >= window_start
                && !already_injected.contains(&r.id)
        })
        .collect();

    // Lane A — file-based dedup.
    let file_records: Vec<&EvidenceRecord> = in_window
        .iter()
        .copied()
        .filter(|r| !r.related_files.is_empty())
        .collect();
    let mut latest_turn_by_file: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for rec in &file_records {
        for file in &rec.related_files {
            latest_turn_by_file
                .entry(file.as_str())
                .and_modify(|t| {
                    if rec.turn_index > *t {
                        *t = rec.turn_index;
                    }
                })
                .or_insert(rec.turn_index);
        }
    }
    let mut file_lane: Vec<&EvidenceRecord> = file_records
        .into_iter()
        .filter(|rec| {
            rec.related_files.iter().any(|f| {
                latest_turn_by_file
                    .get(f.as_str())
                    .is_some_and(|t| *t == rec.turn_index)
            })
        })
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    file_lane.retain(|r| seen.insert(r.id.as_str()));

    // Lane B — recency only.
    let no_file_lane: Vec<&EvidenceRecord> = in_window
        .into_iter()
        .filter(|r| r.related_files.is_empty())
        .collect();

    // Merge lanes and apply the shared budget. Most-recent-first so
    // newer evidence wins when something has to be dropped.
    let mut merged: Vec<&EvidenceRecord> = file_lane;
    merged.extend(no_file_lane);
    merged.sort_by(|a, b| b.turn_index.cmp(&a.turn_index));

    let mut picked = Vec::new();
    let mut used = 0usize;
    for rec in merged {
        if used + rec.chars > EVIDENCE_INJECTION_BUDGET_CHARS {
            continue;
        }
        used += rec.chars;
        picked.push(rec);
    }
    // Chronological injection so the model reads oldest evidence first.
    picked.sort_by_key(|r| r.turn_index);
    picked
}

/// Load an evidence blob and render it as inject-ready text (without
/// the system-reminder wrapper — that is applied by
/// [`wrap_system_reminder`]).
///
/// Format is stable (header line identifies the record) so repeated
/// injections produce identical bytes — preserves the provider's
/// prompt-cache prefix.
fn load_evidence_text(assets_dir: &Path, rec: &EvidenceRecord) -> std::io::Result<String> {
    let rel = rec
        .blob_path
        .as_ref()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no blob_path"))?;
    let path = assets_dir.join(rel);
    let body = std::fs::read_to_string(&path)?;
    let header = format!(
        "# Retrieved evidence: {id} ({summary})\n\n",
        id = rec.id,
        summary = rec.summary,
    );
    Ok(format!("{header}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::evidence::{EvidenceKind, EvidenceRecord, EvidenceStore};
    use std::fs;

    fn rec(id: &str, turn: usize, files: &[&str], chars: usize, has_blob: bool) -> EvidenceRecord {
        EvidenceRecord {
            id: id.into(),
            kind: EvidenceKind::ReadExcerpt,
            tool_use_id: format!("tc_{id}"),
            summary: format!("{id} summary"),
            blob_path: if has_blob {
                Some(format!("evidence/{id}.txt"))
            } else {
                None
            },
            chars,
            turn_index: turn,
            related_files: files.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn write_blob(root: &Path, id: &str, content: &str) {
        let dir = root.join("evidence");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{id}.txt")), content).unwrap();
    }

    /// Synthesize a minimal transcript where each evidence record has a
    /// matching `(assistant tool_use, user tool_result)` pair. The
    /// planner pins evidence chunks by `tool_use_id`, so any test that
    /// expects injection must place the record's matching tool_result
    /// in the transcript.
    ///
    /// The last tool_result is also the anchor — phase A only runs
    /// when the transcript tail is a user turn.
    fn tool_loop_transcript(records: &[&EvidenceRecord]) -> Vec<Message> {
        let mut msgs = vec![Message::user("start")];
        for r in records {
            msgs.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: r.tool_use_id.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({
                        "path": r.related_files.first().cloned().unwrap_or_default()
                    }),
                }],
                origin: None,
            });
            msgs.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: r.tool_use_id.clone(),
                    content: format!("{} summary", r.id),
                    is_error: false,
                    evidence_id: Some(r.id.clone()),
                }],
                origin: None,
            });
        }
        msgs
    }

    /// Find a tool_result in `msg` whose `tool_use_id` matches and
    /// return its content string. Panics if missing — tests use this
    /// to assert on smooshed bytes at a specific record's site.
    fn tool_result_content_for<'a>(msgs: &'a [Message], tool_use_id: &str) -> &'a str {
        for m in msgs {
            for b in &m.content {
                if let ContentBlock::ToolResult {
                    tool_use_id: id,
                    content,
                    ..
                } = b
                    && id == tool_use_id
                {
                    return content.as_str();
                }
            }
        }
        panic!("no tool_result with id {tool_use_id}");
    }

    #[test]
    fn passthrough_when_no_assets_dir() {
        let msgs = vec![Message::user("hi")];
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &EvidenceStore::default(),
            assets_dir: None,
        });
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn passthrough_when_store_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let msgs = vec![Message::user("hi")];
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &EvidenceStore::default(),
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn passthrough_when_system_only() {
        // Only a system message exists — no user anchor yet.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "body");
        let msgs = vec![Message::system("sys")];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 0, &["a.rs"], 100, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.len(), 1, "system unchanged");
    }

    #[test]
    fn passthrough_when_record_site_missing() {
        // Record references a tool_use_id that does not exist in the
        // transcript (stale record, pruned history). Planner must skip
        // silently.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_orphan", "body");
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![rec("ev_orphan", 0, &["a.rs"], 100, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.len(), 1, "user text untouched");
    }

    #[test]
    fn injects_into_matching_tool_result_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "fn main() {}");
        let r = rec("ev_1", 2, &["src/main.rs"], 12, true);
        let msgs = tool_loop_transcript(&[&r]);
        let store = EvidenceStore {
            records: vec![r.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        // Message count unchanged — evidence smooshed inside the
        // matching tool_result.
        assert_eq!(out.len(), msgs.len());
        let content = tool_result_content_for(&out, &r.tool_use_id);
        assert!(content.starts_with("ev_1 summary"), "original preserved");
        assert!(content.contains("<system-reminder>"));
        assert!(content.contains("Retrieved evidence: ev_1"));
        assert!(content.contains("fn main()"));
        assert!(content.contains("</system-reminder>"));
    }

    #[test]
    fn dedup_keeps_only_latest_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_old", "old");
        write_blob(tmp.path(), "ev_new", "new");
        let old = rec("ev_old", 2, &["a.rs"], 100, true);
        let new = rec("ev_new", 5, &["a.rs"], 100, true);
        let msgs = tool_loop_transcript(&[&old, &new]);
        let store = EvidenceStore {
            records: vec![old.clone(), new.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let old_site = tool_result_content_for(&out, &old.tool_use_id);
        let new_site = tool_result_content_for(&out, &new.tool_use_id);
        assert!(
            !old_site.contains("Retrieved evidence"),
            "older file evidence must be dropped"
        );
        assert!(new_site.contains("Retrieved evidence: ev_new"));
    }

    #[test]
    fn injects_multiple_when_different_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_a", "AA");
        write_blob(tmp.path(), "ev_b", "BB");
        let a = rec("ev_a", 3, &["a.rs"], 100, true);
        let b = rec("ev_b", 5, &["b.rs"], 100, true);
        let msgs = tool_loop_transcript(&[&a, &b]);
        let store = EvidenceStore {
            records: vec![a.clone(), b.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert!(tool_result_content_for(&out, &a.tool_use_id).contains("ev_a"));
        assert!(tool_result_content_for(&out, &b.tool_use_id).contains("ev_b"));
    }

    #[test]
    fn skips_evidence_outside_recent_window() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_stale", "stale");
        write_blob(tmp.path(), "ev_fresh", "fresh");
        let stale = rec("ev_stale", 2, &["a.rs"], 100, true);
        let fresh = rec("ev_fresh", 20, &["b.rs"], 100, true);
        // Build a transcript long enough that turn 2 sits outside
        // RECENT_TURN_WINDOW=15 from the tail.
        let mut msgs = tool_loop_transcript(&[&stale]);
        while msgs.len() < 20 {
            msgs.push(Message::user("x"));
        }
        msgs.extend(tool_loop_transcript(&[&fresh]).into_iter().skip(1));
        let store = EvidenceStore {
            records: vec![stale.clone(), fresh.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let stale_site = tool_result_content_for(&out, &stale.tool_use_id);
        let fresh_site = tool_result_content_for(&out, &fresh.tool_use_id);
        assert!(
            !stale_site.contains("Retrieved evidence"),
            "stale record outside window must not inject"
        );
        assert!(fresh_site.contains("Retrieved evidence: ev_fresh"));
    }

    #[test]
    fn skips_evidence_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_big", "X");
        write_blob(tmp.path(), "ev_small", "Y");
        // ev_big alone is over budget; ev_small fits. Ranking is
        // recent-first so big (turn 2, newer) is tried first and
        // skipped, leaving room for small (turn 1).
        let small = rec("ev_small", 1, &["a.rs"], 100, true);
        let big = rec(
            "ev_big",
            2,
            &["b.rs"],
            EVIDENCE_INJECTION_BUDGET_CHARS + 1,
            true,
        );
        let msgs = tool_loop_transcript(&[&small, &big]);
        let store = EvidenceStore {
            records: vec![small.clone(), big.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert!(tool_result_content_for(&out, &small.tool_use_id).contains("ev_small"));
        assert!(
            !tool_result_content_for(&out, &big.tool_use_id).contains("Retrieved evidence"),
            "oversize record must be skipped"
        );
    }

    #[test]
    fn skips_records_without_blob_path() {
        let tmp = tempfile::tempdir().unwrap();
        let r = rec("ev_inline", 1, &["a.rs"], 50, false);
        let msgs = tool_loop_transcript(&[&r]);
        let store = EvidenceStore {
            records: vec![r.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let site = tool_result_content_for(&out, &r.tool_use_id);
        assert!(
            !site.contains("Retrieved evidence"),
            "no blob → nothing to inject"
        );
    }

    #[test]
    fn skips_missing_blob_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let r = rec("ev_ghost", 1, &["a.rs"], 50, true);
        let msgs = tool_loop_transcript(&[&r]);
        let store = EvidenceStore {
            records: vec![r.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let site = tool_result_content_for(&out, &r.tool_use_id);
        assert!(
            !site.contains("Retrieved evidence"),
            "missing blob is skipped gracefully"
        );
    }

    #[test]
    fn injects_bash_evidence_without_related_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_diff", "diff --git a/x b/x\n@@\n-a\n+b");
        let r = rec("ev_diff", 1, &[], 40, true);
        let msgs = tool_loop_transcript(&[&r]);
        let store = EvidenceStore {
            records: vec![r.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let site = tool_result_content_for(&out, &r.tool_use_id);
        assert!(site.contains("Retrieved evidence: ev_diff"));
        assert!(site.contains("diff --git"));
    }

    #[test]
    fn no_file_lane_keeps_every_record_by_recency() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_status", "On branch master");
        write_blob(tmp.path(), "ev_log", "commit abc");
        let status = rec("ev_status", 1, &[], 20, true);
        let log = rec("ev_log", 2, &[], 20, true);
        let msgs = tool_loop_transcript(&[&status, &log]);
        let store = EvidenceStore {
            records: vec![status.clone(), log.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert!(tool_result_content_for(&out, &status.tool_use_id).contains("ev_status"));
        assert!(tool_result_content_for(&out, &log.tool_use_id).contains("ev_log"));
    }

    #[test]
    fn merged_lanes_share_budget() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_file", "F");
        write_blob(tmp.path(), "ev_bash", "B");
        let near_budget = EVIDENCE_INJECTION_BUDGET_CHARS - 10;
        let file = rec("ev_file", 1, &["a.rs"], near_budget, true);
        let bash = rec("ev_bash", 2, &[], 20, true);
        let msgs = tool_loop_transcript(&[&file, &bash]);
        let store = EvidenceStore {
            records: vec![file.clone(), bash.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        // Newer (bash) wins budget; file overflows and is dropped.
        assert!(tool_result_content_for(&out, &bash.tool_use_id).contains("ev_bash"));
        assert!(
            !tool_result_content_for(&out, &file.tool_use_id).contains("Retrieved evidence"),
            "file record overflows shared budget"
        );
    }

    #[test]
    fn passthrough_when_tail_is_assistant() {
        // Assistant tail — previous turn finished, no pending user.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "body");
        let msgs = vec![Message::user("q"), Message::assistant("answer")];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 0, &["a.rs"], 100, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 2);
        match &out[1].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "answer"),
            _ => panic!("assistant tail must not be touched"),
        }
    }

    #[test]
    fn idempotent_across_iters_keeps_cache_prefix_stable() {
        // The cache-stability invariant (§2.6): iter N+1 sees the same
        // bytes at every message index ≤ N that iter N did. Feeding
        // iter N's output back in must produce identical output.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "body1");
        write_blob(tmp.path(), "ev_2", "body2");
        let r1 = rec("ev_1", 2, &["a.rs"], 50, true);
        let r2 = rec("ev_2", 4, &["b.rs"], 50, true);

        // Iter N: both records in store, both sites present.
        let msgs_n = tool_loop_transcript(&[&r1, &r2]);
        let store = EvidenceStore {
            records: vec![r1.clone(), r2.clone()],
        };
        let iter_n = build_prepared_messages(PlanInput {
            transcript: &msgs_n,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });

        // Iter N+1: feed iter N's output back in, same store.
        let iter_np1 = build_prepared_messages(PlanInput {
            transcript: &iter_n,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });

        // Byte-identical content at both sites.
        assert_eq!(
            tool_result_content_for(&iter_n, &r1.tool_use_id),
            tool_result_content_for(&iter_np1, &r1.tool_use_id),
            "ev_1 site stable across iters"
        );
        assert_eq!(
            tool_result_content_for(&iter_n, &r2.tool_use_id),
            tool_result_content_for(&iter_np1, &r2.tool_use_id),
            "ev_2 site stable across iters"
        );
        // Each site has exactly one evidence marker — no double smoosh.
        assert_eq!(
            tool_result_content_for(&iter_np1, &r1.tool_use_id)
                .matches("Retrieved evidence: ev_1")
                .count(),
            1
        );
        assert_eq!(
            tool_result_content_for(&iter_np1, &r2.tool_use_id)
                .matches("Retrieved evidence: ev_2")
                .count(),
            1
        );
    }

    #[test]
    fn evidence_wrapped_in_system_reminder() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "payload");
        let r = rec("ev_1", 0, &["a.rs"], 7, true);
        let msgs = tool_loop_transcript(&[&r]);
        let store = EvidenceStore {
            records: vec![r.clone()],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let site = tool_result_content_for(&out, &r.tool_use_id);
        assert!(site.contains("<system-reminder>\n"));
        assert!(site.contains("\n</system-reminder>"));
        assert!(site.contains("payload"));
    }

    #[test]
    fn preserves_order_and_count_passthrough_input() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &EvidenceStore::default(),
            assets_dir: None,
        });
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[1].role, Role::User);
        assert_eq!(out[2].role, Role::Assistant);
    }
}
