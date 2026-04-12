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
//! and smooshed into the **content of the last tool_result block**
//! when the anchor is a tool_result user message. This mirrors Claude
//! Code's own pattern (`src/utils/messages.ts`: `smooshIntoToolResult`
//! / `wrapInSystemReminder`). Two reasons beyond "it's what Claude
//! expects":
//!
//! 1. **Prompt-injection hygiene.** A raw Text sibling after a
//!    `tool_result` looks to the model like user-authored content
//!    (Claude transcribes user text verbatim). Without the wrapper
//!    the model treats retrieved evidence as a potential attack and
//!    defends — e.g. reads files with `limit=302` instead of trusting
//!    the content. The `<system-reminder>` tag is a trained signal
//!    that marks metadata as system-authored and trustworthy.
//!
//! 2. **`Human:` drift.** Any sibling after a `tool_result` renders
//!    on the wire as `</function_results>\n\nHuman:<…>`. Repeated
//!    mid-conversation, this teaches the model to emit `Human:` at
//!    bare tails — a known training-drift failure mode that Claude
//!    Code's A/B (`sai-20260310-161901`) documented going from 92%
//!    to 0% after switching to smoosh.
//!
//! When the anchor is a plain user-text turn (no tool_result to
//! smoosh into), the evidence is prepended as a Text block — safe
//! because no assistant tool_use precedes the turn.
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
pub fn build_prepared_messages(input: PlanInput<'_>) -> Vec<Message> {
    let mut out: Vec<Message> = input.transcript.to_vec();

    let Some(assets_dir) = input.assets_dir else {
        return out;
    };
    let Some(anchor) = find_injection_anchor(&out) else {
        return out;
    };
    let already = collect_injected_ids(&out);
    let current_turn = out.len();
    let selected = select_evidence(&input.evidence.records, current_turn, &already);
    if selected.is_empty() {
        return out;
    }

    let mut chunks = Vec::with_capacity(selected.len());
    for rec in selected {
        match load_evidence_text(assets_dir, rec) {
            Ok(text) => chunks.push(wrap_system_reminder(&text)),
            Err(e) => {
                crate::dbg_log!("context_plan: skip evidence {}: {}", rec.id, e);
            }
        }
    }
    if chunks.is_empty() {
        return out;
    }

    inject_into_anchor(&mut out[anchor], chunks);
    out
}

/// Append evidence chunks to the anchor message.
///
/// If any block is a `tool_result`, smoosh every chunk into the **last**
/// tool_result's content (Claude Code pattern). Otherwise the anchor is
/// a plain user turn and we prepend the chunks as Text blocks.
fn inject_into_anchor(msg: &mut Message, chunks: Vec<String>) {
    let last_tr = msg
        .content
        .iter()
        .rposition(|b| matches!(b, ContentBlock::ToolResult { .. }));

    if let Some(idx) = last_tr
        && let ContentBlock::ToolResult { content, .. } = &mut msg.content[idx]
    {
        for chunk in chunks {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&chunk);
        }
        return;
    }

    // Plain user turn — prepend as Text blocks so the user's own text
    // remains last in the content vector.
    let original = std::mem::take(&mut msg.content);
    let mut merged = Vec::with_capacity(chunks.len() + original.len());
    for chunk in chunks {
        merged.push(ContentBlock::Text { text: chunk });
    }
    merged.extend(original);
    msg.content = merged;
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
/// Dedups by file (latest record per file wins), filters by the recent
/// turn window, skips records already injected (idempotent across
/// tool-loop iterations), ranks most-recent-first for budget decisions,
/// then returns in chronological order so injection reads top-down.
fn select_evidence<'a>(
    records: &'a [EvidenceRecord],
    current_turn: usize,
    already_injected: &HashSet<String>,
) -> Vec<&'a EvidenceRecord> {
    let window_start = current_turn.saturating_sub(RECENT_TURN_WINDOW);
    let candidates_vec: Vec<&EvidenceRecord> = records
        .iter()
        .filter(|r| {
            r.blob_path.is_some()
                && r.turn_index >= window_start
                && !r.related_files.is_empty()
                && !already_injected.contains(&r.id)
        })
        .collect();

    // Latest turn seen per file — drives dedup.
    let mut latest_turn_by_file: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for rec in &candidates_vec {
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
    // Keep a record if it is the latest for at least one of its files.
    let mut deduped: Vec<&EvidenceRecord> = candidates_vec
        .into_iter()
        .filter(|rec| {
            rec.related_files.iter().any(|f| {
                latest_turn_by_file
                    .get(f.as_str())
                    .is_some_and(|t| *t == rec.turn_index)
            })
        })
        .collect();

    // Two distinct records at the same turn_index that both claim
    // "latest" for different files are both kept; dedup purely by id.
    let mut seen: HashSet<&str> = HashSet::new();
    deduped.retain(|r| seen.insert(r.id.as_str()));

    // Budget pass: most-recent first, so newer evidence wins when
    // something has to be dropped.
    deduped.sort_by(|a, b| b.turn_index.cmp(&a.turn_index));
    let mut picked = Vec::new();
    let mut used = 0usize;
    for rec in deduped {
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
        // Only a system message exists — no user anchor yet (first turn
        // boot state).
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
    fn injects_one_evidence_into_user_text_tail() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "fn main() {}");
        let msgs = vec![
            Message::user("start"),
            Message::assistant("working"),
            Message::user("what next"),
        ];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 1, &["src/main.rs"], 12, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        // Same message count — evidence merged into anchor, not inserted.
        assert_eq!(out.len(), 3);
        let anchor = &out[2];
        assert_eq!(anchor.role, Role::User);
        assert_eq!(
            anchor.content.len(),
            2,
            "evidence block + original user text"
        );
        match &anchor.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Retrieved evidence: ev_1"));
                assert!(text.contains("fn main()"));
            }
            _ => panic!("expected evidence text block first"),
        }
        match &anchor.content[1] {
            ContentBlock::Text { text } => assert_eq!(text, "what next"),
            _ => panic!("user text should remain after evidence"),
        }
    }

    #[test]
    fn dedup_keeps_only_latest_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_old", "old");
        write_blob(tmp.path(), "ev_new", "new");
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![
                rec("ev_old", 2, &["a.rs"], 100, true),
                rec("ev_new", 5, &["a.rs"], 100, true),
            ],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
        let anchor = &out[0];
        assert_eq!(anchor.content.len(), 2);
        match &anchor.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("ev_new"));
                assert!(!text.contains("ev_old"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn injects_multiple_when_different_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_a", "A");
        write_blob(tmp.path(), "ev_b", "B");
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![
                rec("ev_a", 3, &["a.rs"], 100, true),
                rec("ev_b", 5, &["b.rs"], 100, true),
            ],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
        let anchor = &out[0];
        assert_eq!(anchor.content.len(), 3, "ev_a + ev_b + user text");
        // Chronological injection order: older turn first.
        match &anchor.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("ev_a")),
            _ => panic!(),
        }
        match &anchor.content[1] {
            ContentBlock::Text { text } => assert!(text.contains("ev_b")),
            _ => panic!(),
        }
        match &anchor.content[2] {
            ContentBlock::Text { text } => assert_eq!(text, "q"),
            _ => panic!("user text must remain last"),
        }
    }

    #[test]
    fn skips_evidence_outside_recent_window() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_stale", "stale");
        write_blob(tmp.path(), "ev_fresh", "fresh");
        // 20 messages so RECENT_TURN_WINDOW=15 cuts off turn 2.
        let mut msgs = Vec::new();
        for _ in 0..20 {
            msgs.push(Message::user("x"));
        }
        let store = EvidenceStore {
            records: vec![
                rec("ev_stale", 2, &["a.rs"], 100, true),
                rec("ev_fresh", 18, &["b.rs"], 100, true),
            ],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 20);
        let anchor = out.last().unwrap();
        let joined: String = anchor
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(joined.contains("ev_fresh"));
        assert!(!joined.contains("ev_stale"));
    }

    #[test]
    fn skips_evidence_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_big", "X");
        write_blob(tmp.path(), "ev_small", "Y");
        let msgs = vec![Message::user("q")];
        // ev_big alone is over budget; ev_small fits.
        let store = EvidenceStore {
            records: vec![
                rec("ev_small", 1, &["a.rs"], 100, true),
                rec(
                    "ev_big",
                    2,
                    &["b.rs"],
                    EVIDENCE_INJECTION_BUDGET_CHARS + 1,
                    true,
                ),
            ],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1);
        let anchor = &out[0];
        assert_eq!(anchor.content.len(), 2, "only ev_small + user text");
        match &anchor.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("ev_small"));
                assert!(!text.contains("ev_big"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn skips_records_without_blob_path() {
        let tmp = tempfile::tempdir().unwrap();
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![rec("ev_inline", 1, &["a.rs"], 50, false)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1, "no blob → nothing to inject");
    }

    #[test]
    fn skips_missing_blob_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        // Record says blob exists, but file is missing on disk.
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![rec("ev_ghost", 1, &["a.rs"], 50, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 1, "missing blob is skipped gracefully");
    }

    #[test]
    fn passthrough_when_tail_is_assistant() {
        // Assistant tail means the previous turn just finished; there
        // is no pending user input to decorate.
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
    fn injects_into_tool_result_user_message() {
        // Mid tool-loop: assistant just emitted a tool_use and the
        // tool_result user message is the tail. Evidence smooshes
        // into the tool_result's content, wrapped in
        // <system-reminder>. The content count stays at 1 —
        // retrieved context rides inside the tool_result, not as a
        // sibling block.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "fn main() {}");
        let msgs = vec![
            Message::user("fix this"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tc_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                origin: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".into(),
                    content: "summary".into(),
                    is_error: false,
                    evidence_id: Some("ev_1".into()),
                }],
                origin: None,
            },
        ];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 1, &["a.rs"], 12, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].content.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "fix this"),
            _ => panic!(),
        }
        let tail = &out[2];
        assert_eq!(tail.role, Role::User);
        assert_eq!(
            tail.content.len(),
            1,
            "evidence smooshes inside tool_result"
        );
        match &tail.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "tc_1");
                assert!(content.starts_with("summary"), "original result preserved");
                assert!(content.contains("<system-reminder>"));
                assert!(content.contains("Retrieved evidence: ev_1"));
                assert!(content.contains("fn main()"));
                assert!(content.contains("</system-reminder>"));
            }
            _ => panic!("tail must still be a tool_result"),
        }
    }

    #[test]
    fn injects_into_last_tool_result_with_mixed_tail() {
        // Multi-tool batch: user message carries several tool_result
        // blocks plus trailing text. Evidence smooshes into the LAST
        // tool_result's content — not into the trailing text, not
        // into the first tool_result.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "body");
        let tail = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tc_1".into(),
                    content: "r1".into(),
                    is_error: false,
                    evidence_id: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tc_2".into(),
                    content: "r2".into(),
                    is_error: false,
                    evidence_id: None,
                },
                ContentBlock::Text {
                    text: "and one more thing".into(),
                },
            ],
            origin: None,
        };
        let msgs = vec![
            Message::user("start"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "tc_1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "a.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "tc_2".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "b.rs"}),
                    },
                ],
                origin: None,
            },
            tail,
        ];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 1, &["a.rs"], 12, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let tail = out.last().unwrap();
        // Block shape unchanged — [tool_result, tool_result, text].
        assert_eq!(tail.content.len(), 3);
        // First tool_result untouched.
        match &tail.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "tc_1");
                assert_eq!(content, "r1", "first tool_result untouched");
            }
            _ => panic!(),
        }
        // Last tool_result carries the evidence.
        match &tail.content[1] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "tc_2");
                assert!(content.starts_with("r2"));
                assert!(content.contains("Retrieved evidence: ev_1"));
                assert!(content.contains("<system-reminder>"));
            }
            _ => panic!(),
        }
        // Trailing user text untouched.
        match &tail.content[2] {
            ContentBlock::Text { text } => assert_eq!(text, "and one more thing"),
            _ => panic!(),
        }
    }

    #[test]
    fn idempotent_across_tool_loop_iters() {
        // After iter N the transcript carries the evidence marker
        // inside a tool_result's content. Iter N+1 must see it and
        // skip re-injection.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "body");
        let msgs = vec![
            Message::user("q"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tc_1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                origin: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".into(),
                    content: "summary".into(),
                    is_error: false,
                    evidence_id: Some("ev_1".into()),
                }],
                origin: None,
            },
        ];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 1, &["a.rs"], 50, true)],
        };
        let iter_n = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let iter_n_content = tool_result_content(iter_n.last().unwrap(), 0);
        assert_eq!(
            iter_n_content.matches("Retrieved evidence: ev_1").count(),
            1
        );

        let iter_np1 = build_prepared_messages(PlanInput {
            transcript: &iter_n,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let iter_np1_content = tool_result_content(iter_np1.last().unwrap(), 0);
        assert_eq!(
            iter_np1_content.matches("Retrieved evidence: ev_1").count(),
            1,
            "idempotent: no double smoosh"
        );
        assert_eq!(
            iter_np1_content, iter_n_content,
            "tool_result content byte-identical across iters"
        );
    }

    #[test]
    fn evidence_wrapped_in_system_reminder() {
        // Lock in the wrapper shape — it's the signal that tells the
        // model this text is system-authored metadata, not user input.
        // Wrapper drift breaks the anti-prompt-injection property.
        let tmp = tempfile::tempdir().unwrap();
        write_blob(tmp.path(), "ev_1", "payload");
        let msgs = vec![Message::user("q")];
        let store = EvidenceStore {
            records: vec![rec("ev_1", 0, &["a.rs"], 7, true)],
        };
        let out = build_prepared_messages(PlanInput {
            transcript: &msgs,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        // Plain user-text tail: evidence sits as a prepended Text block.
        let anchor = &out[0];
        match &anchor.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.starts_with("<system-reminder>\n"));
                assert!(text.ends_with("\n</system-reminder>"));
                assert!(text.contains("Retrieved evidence: ev_1"));
                assert!(text.contains("payload"));
            }
            _ => panic!("evidence must be a Text block before user text"),
        }
    }

    /// Read the `content` string of the `n`-th `ToolResult` block in a
    /// message. Panics if not enough tool_results exist — tests use
    /// this to assert on smooshed content.
    fn tool_result_content(msg: &Message, n: usize) -> &str {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .nth(n)
            .expect("tool_result block present")
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
