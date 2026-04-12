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
//! `stream()` call within the same tool loop. Anchoring on a
//! user-text message only (the original design) would miss this case
//! entirely; phase A as initially shipped never fired in practice.
//!
//! The rule (RFC §9, one rule only):
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
//!
//! Injected evidence is prepended as `ContentBlock::Text` entries to
//! the anchor message's content. The evidence store itself is
//! unchanged.

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
/// no assets dir, no user anchor). See module doc for selection rules.
///
/// Evidence text is prepended as additional `ContentBlock::Text` entries
/// to the trailing user message — not inserted as separate messages.
/// Anthropic requires strict user/assistant alternation; inserting a
/// second user message would fail at the wire layer. The anchor may be
/// a user-text turn *or* a `tool_result`-only user turn: the latter is
/// the common case inside a multi-tool-call assistant turn, which is
/// where duplicate reads actually happen.
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

    let mut blocks = Vec::with_capacity(selected.len());
    for rec in selected {
        match load_evidence_block(assets_dir, rec) {
            Ok(block) => blocks.push(block),
            Err(e) => {
                crate::dbg_log!("context_plan: skip evidence {}: {}", rec.id, e);
            }
        }
    }
    if blocks.is_empty() {
        return out;
    }

    // Prepend evidence blocks so the anchor's original content (user
    // text or tool_result blocks) remains after the retrieved context.
    let original = std::mem::take(&mut out[anchor].content);
    let mut merged = Vec::with_capacity(blocks.len() + original.len());
    merged.extend(blocks);
    merged.extend(original);
    out[anchor].content = merged;
    out
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

/// Collect evidence ids already materialised as text blocks in the
/// transcript. Used to skip double-injection across tool-loop
/// iterations — the header `# Retrieved evidence: {id}` is the stable
/// marker [`load_evidence_block`] writes.
fn collect_injected_ids(msgs: &[Message]) -> HashSet<String> {
    const PREFIX: &str = "# Retrieved evidence: ";
    let mut ids = HashSet::new();
    for m in msgs {
        for b in &m.content {
            let ContentBlock::Text { text } = b else {
                continue;
            };
            let Some(rest) = text.strip_prefix(PREFIX) else {
                continue;
            };
            // Header format: "{id} ({summary})\n\n…". Id is everything
            // up to the first space or '('.
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(rest.len());
            if end > 0 {
                ids.insert(rest[..end].to_owned());
            }
        }
    }
    ids
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

/// Load an evidence blob and wrap it as a single `Text` content block.
///
/// Format is stable (prefix line identifies the record) so repeated
/// injections produce identical bytes — preserves the provider's
/// prompt-cache prefix.
fn load_evidence_block(assets_dir: &Path, rec: &EvidenceRecord) -> std::io::Result<ContentBlock> {
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
    Ok(ContentBlock::Text {
        text: format!("{header}{body}"),
    })
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
        // tool_result user message is the tail. This is the hot path
        // for duplicate reads — the planner must decorate this user
        // message so the next stream() call sees the evidence.
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
        // "fix this" user turn untouched — only the tool_result tail is
        // decorated.
        assert_eq!(out[0].content.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "fix this"),
            _ => panic!(),
        }
        // Tail user message: evidence block, then original tool_result.
        let tail = &out[2];
        assert_eq!(tail.role, Role::User);
        assert_eq!(tail.content.len(), 2);
        match &tail.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Retrieved evidence: ev_1"));
                assert!(text.contains("fn main()"));
            }
            _ => panic!("evidence must be prepended"),
        }
        match &tail.content[1] {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                assert_eq!(tool_use_id, "tc_1")
            }
            _ => panic!("tool_result must survive"),
        }
    }

    #[test]
    fn idempotent_across_tool_loop_iters() {
        // Simulate iter N → iter N+1 of the same tool loop. After iter
        // N the transcript carries an injected header for ev_1; iter
        // N+1 must not duplicate that injection.
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
        // Iter N: evidence injected once.
        let iter_n_tail = iter_n.last().unwrap();
        assert_eq!(iter_n_tail.content.len(), 2);
        let evidence_occurrences_n: usize = iter_n_tail
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => {
                    Some(text.matches("Retrieved evidence: ev_1").count())
                }
                _ => None,
            })
            .sum();
        assert_eq!(evidence_occurrences_n, 1);

        // Iter N+1: feed the injected transcript back in, same store.
        // Planner must recognise ev_1 is already present and skip.
        let iter_np1 = build_prepared_messages(PlanInput {
            transcript: &iter_n,
            evidence: &store,
            assets_dir: Some(tmp.path()),
        });
        let iter_np1_tail = iter_np1.last().unwrap();
        assert_eq!(
            iter_np1_tail.content.len(),
            2,
            "no new block — still [evidence, tool_result]"
        );
        let evidence_occurrences_np1: usize = iter_np1_tail
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => {
                    Some(text.matches("Retrieved evidence: ev_1").count())
                }
                _ => None,
            })
            .sum();
        assert_eq!(
            evidence_occurrences_np1, 1,
            "injection is idempotent across iters"
        );
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
