/// Session — persistent conversation with JSON storage.
use crate::core::evidence::EvidenceStore;
use crate::core::types::Message;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Last-turn token snapshot — represents current context window usage.
/// Not cumulative across turns; each turn's response replaces this.
///
/// Uses `u64` (not `Option<u64>`) for cache fields because this is persisted
/// state: once written, the absence of cache data is represented as 0, not
/// `None`. Contrast with [`crate::core::types::Usage`] which uses `Option`
/// because a provider response may genuinely omit cache fields mid-stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A persisted conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub usage: SessionUsage,
    #[serde(default)]
    pub turn_durations: Vec<f64>,
    /// Oversized tool outputs promoted out of the transcript.
    ///
    /// Scaffolded empty until the ingest path lands; see
    /// `docs/rfcs/evidence-backed-handoff.md` and [`crate::core::evidence`].
    #[serde(default)]
    pub evidence: EvidenceStore,
}

/// Summary for listing sessions (no messages loaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: usize,
    pub last_preview: String,
}

impl Session {
    /// Create a new empty session.
    pub fn new() -> Self {
        let id = generate_id();
        let now = now_iso();
        Self {
            id,
            title: String::new(),
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
            usage: SessionUsage::default(),
            turn_durations: Vec::new(),
            evidence: EvidenceStore::default(),
        }
    }

    /// Auto-title from first user message if untitled.
    pub fn auto_title(&mut self) {
        if !self.title.is_empty() {
            return;
        }
        if let Some(msg) = self
            .messages
            .iter()
            .find(|m| m.role == crate::core::types::Role::User)
        {
            self.title = preview_text_n(msg.display_text(), 60);
        }
    }

    /// Save to disk. Skips empty sessions (no user messages).
    pub fn save(&mut self) {
        let has_user_msg = self
            .messages
            .iter()
            .any(|m| m.role == crate::core::types::Role::User);
        if !has_user_msg {
            return;
        }
        self.updated_at = now_iso();
        self.auto_title();
        let dir = sessions_dir();
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", self.id));
        if let Ok(json) = serde_json::to_string(self) {
            let _ = fs::write(path, json);
        }
    }

    /// Load a session by ID.
    pub fn load(id: &str) -> Option<Self> {
        let path = sessions_dir().join(format!("{id}.json"));
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

/// List all sessions sorted by updated_at (newest first).
pub fn list_sessions() -> Vec<SessionMeta> {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut sessions: Vec<SessionMeta> = entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            let raw = fs::read_to_string(e.path()).ok()?;
            let session: Session = serde_json::from_str(&raw).ok()?;
            let last_preview = session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == crate::core::types::Role::User)
                .map(|m| preview_text_n(m.display_text(), 50))
                .unwrap_or_default();
            Some(SessionMeta {
                id: session.id,
                title: session.title,
                created_at: session.created_at,
                updated_at: session.updated_at,
                message_count: session.messages.len(),
                last_preview,
            })
        })
        .collect();

    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

/// First meaningful line of user text, truncated to `max` chars.
fn preview_text_n(text: &str, max: usize) -> String {
    text.lines()
        .find(|l| !l.starts_with('<') && !l.trim().is_empty())
        .map(|l| l.chars().take(max).collect())
        .unwrap_or_default()
}

fn sessions_dir() -> PathBuf {
    crate::config::home_dir()
        .join(".config")
        .join("luma")
        .join("sessions")
}

/// Root directory for assets belonging to one session.
///
/// Layout is segmented by asset kind so new kinds (evidence blobs) can live
/// alongside images without collision:
///
/// ```text
/// sessions/
///   {id}.json
///   {id}/
///     images/{image_id}
///     evidence/{evidence_id}.txt
/// ```
fn session_assets_dir(session_id: &str) -> PathBuf {
    sessions_dir().join(session_id)
}

/// Subdirectory holding image attachments for a session.
fn session_images_dir(session_id: &str) -> PathBuf {
    session_assets_dir(session_id).join("images")
}

/// Subdirectory holding evidence blobs for a session.
pub fn session_evidence_dir(session_id: &str) -> PathBuf {
    session_assets_dir(session_id).join("evidence")
}

tokio::task_local! {
    /// Id of the session whose turn is currently executing. Propagated
    /// via [`scope_current_session`] so tools running deep in the stack
    /// can resolve session-scoped URIs (`artifact://ev_xxx`) without
    /// changing the `Tool` trait.
    ///
    /// Unset outside a turn — callers must handle `try_with` failure
    /// gracefully (e.g. reject URIs that need a session context).
    static CURRENT_SESSION: String;
}

/// Run `fut` with `session_id` exposed as the current session. The agent
/// loop wraps its tool execution in this scope so tools observe the
/// correct id even when executing concurrently across sessions (future
/// flow).
pub async fn scope_current_session<F, T>(session_id: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_SESSION.scope(session_id.to_owned(), fut).await
}

/// Return the session id currently in scope, or `None` if called outside
/// a `scope_current_session` frame. Tools that need session-scoped
/// storage (e.g. saving an attached image to `sessions/{id}/images/`)
/// MUST handle the `None` case and skip the operation.
pub fn current_session_id() -> Option<String> {
    CURRENT_SESSION.try_with(|s| s.clone()).ok()
}

/// Result of resolving a URI-style path to a filesystem location.
///
/// Separate from a bare `PathBuf` because some resource types need
/// post-read transformations that the caller must apply consistently —
/// e.g. `artifact://skill/{name}` strips the YAML frontmatter before
/// returning the body to the model, since the frontmatter is already
/// in the system prompt catalog.
#[derive(Debug)]
pub enum Resolved {
    /// Read the file verbatim.
    Path(PathBuf),
    /// Read the file, then drop a leading `---…---` YAML frontmatter
    /// block before returning content to the caller.
    PathStripFrontmatter(PathBuf),
}

/// Resolve a URI-style path to a concrete filesystem location.
///
/// Single-scheme registry (`artifact://`) with typed sub-resolvers so
/// the model only ever needs to remember one URL form. Currently
/// registered types:
///
/// * `artifact://ev/{id}` — re-read a stored evidence blob from the
///   current session's `evidence/` directory. Requires an active
///   [`scope_current_session`].
/// * `artifact://skill/{name}` — load a skill's `SKILL.md` body
///   (frontmatter stripped). Names come from the `<available_skills>`
///   catalog injected into the system prompt.
///
/// Non-URI strings pass through as plain filesystem paths so the vast
/// majority of tool calls take no penalty.
pub fn resolve_resource_path(path: &str) -> std::io::Result<Resolved> {
    let Some((scheme, rest)) = path.split_once("://") else {
        return Ok(Resolved::Path(PathBuf::from(path)));
    };
    if scheme != "artifact" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown URI scheme: {scheme}"),
        ));
    }
    let (kind, id) = rest.split_once('/').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact:// requires `{type}/{id}` (e.g. artifact://ev/ev_abc)",
        )
    })?;
    match kind {
        "ev" => resolve_evidence(id).map(Resolved::Path),
        "skill" => resolve_skill(id).map(Resolved::PathStripFrontmatter),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown artifact type: {other}"),
        )),
    }
}

fn is_safe_id_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
}

fn resolve_evidence(id: &str) -> std::io::Result<PathBuf> {
    if !is_safe_id_segment(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid evidence id: {id}"),
        ));
    }
    let session_id = CURRENT_SESSION.try_with(|s| s.clone()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "artifact://ev requires an active session",
        )
    })?;
    let path = session_evidence_dir(&session_id).join(format!("{id}.txt"));
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("artifact ev/{id} not found in session {session_id}"),
        ));
    }
    Ok(path)
}

fn resolve_skill(name: &str) -> std::io::Result<PathBuf> {
    if !is_safe_id_segment(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid skill name: {name}"),
        ));
    }
    // Discovery already walks project + user skill directories with
    // precedence rules — reuse it so resolution matches what the
    // catalog advertises byte-for-byte.
    let skills = crate::config::skills::discover();
    let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("skill not found: {name}"),
        )
    })?;
    Ok(skill.path.clone())
}

/// Save image bytes to `sessions/{session_id}/images/{filename}`. Returns filename.
pub fn save_image(session_id: &str, data: &[u8], ext: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("img_{ts:x}.{ext}");
    let dir = session_images_dir(session_id);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(&filename), data);
    filename
}

/// Read image as base64 from `sessions/{session_id}/images/{image_id}`.
///
/// Returns an empty string if the file is absent. Sessions created before
/// the `images/` subdirectory layout store attachments flat in the session
/// root and will not resolve — acceptable under the project's beta stance.
pub fn read_image_base64(session_id: &str, image_id: &str) -> String {
    use base64::Engine;
    let path = session_images_dir(session_id).join(image_id);
    match fs::read(&path) {
        Ok(data) => base64::engine::general_purpose::STANDARD.encode(&data),
        Err(_) => String::new(),
    }
}

/// Build an image resolver closure for a given session.
pub fn image_resolver(session_id: &str) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    let sid = session_id.to_owned();
    Box::new(move |image_id: &str| read_image_base64(&sid, image_id))
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("ses_{ts:x}")
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-ish format without chrono dep
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_has_id() {
        let s = Session::new();
        assert!(!s.id.is_empty());
        assert!(s.id.starts_with("ses_"));
    }

    #[test]
    fn auto_title_from_user_message() {
        let mut s = Session::new();
        s.messages
            .push(Message::user("Hello, can you help me with Rust?"));
        s.auto_title();
        assert_eq!(s.title, "Hello, can you help me with Rust?");
    }

    #[test]
    fn auto_title_truncates_long() {
        let mut s = Session::new();
        let long = "x".repeat(100);
        s.messages.push(Message::user(long));
        s.auto_title();
        assert_eq!(s.title.len(), 60);
    }

    #[test]
    fn list_sessions_empty() {
        // Just verify it doesn't panic
        let _ = list_sessions();
    }

    fn path_of(r: Resolved) -> PathBuf {
        match r {
            Resolved::Path(p) | Resolved::PathStripFrontmatter(p) => p,
        }
    }

    #[test]
    fn resolve_plain_path_passes_through() {
        let out = resolve_resource_path("/abs/path/file.rs").unwrap();
        assert!(matches!(out, Resolved::Path(_)));
        assert_eq!(path_of(out), PathBuf::from("/abs/path/file.rs"));
    }

    #[test]
    fn resolve_unknown_scheme_is_error() {
        let err = resolve_resource_path("weird://foo").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn resolve_artifact_without_type_is_error() {
        // artifact:// must be followed by {type}/{id}.
        let err = resolve_resource_path("artifact://ev_abc").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn resolve_artifact_unknown_type_is_error() {
        let err = resolve_resource_path("artifact://bogus/x").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn resolve_evidence_without_session_scope_fails() {
        let err = resolve_resource_path("artifact://ev/ev_abc").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn resolve_evidence_rejects_path_traversal() {
        scope_current_session("ses_nope", async {
            for bad in ["", "../etc", "a/b", "a\\b", ".."] {
                let uri = format!("artifact://ev/{bad}");
                let err = resolve_resource_path(&uri).unwrap_err();
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{uri}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn resolve_evidence_missing_blob_reports_not_found() {
        scope_current_session("ses_nope", async {
            let err = resolve_resource_path("artifact://ev/ev_does_not_exist").unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        })
        .await;
    }

    #[test]
    fn resolve_skill_rejects_path_traversal() {
        for bad in ["", "../etc", "a/b", "a\\b", ".."] {
            let uri = format!("artifact://skill/{bad}");
            let err = resolve_resource_path(&uri).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{uri}");
        }
    }

    #[test]
    fn resolve_skill_unknown_name_is_not_found() {
        // A name that can't reasonably match any discovered skill.
        let err = resolve_resource_path("artifact://skill/definitely_not_a_real_skill_xyz_42")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn deserialize_session() {
        let json = r#"{
            "id": "ses_test",
            "title": "test",
            "created_at": "123",
            "updated_at": "456",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "You are helpful."}]},
                {"role": "user", "content": [{"type": "text", "text": "\n<file path=\"test.rs\">\n```rs\nfn main() {}\n```\n</file>\n what is this"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "This is a Rust main function."}]}
            ],
            "usage": {"input_tokens": 0, "output_tokens": 0, "cache_read": 0, "cache_write": 0},
            "turn_durations": [1.5]
        }"#;
        let session: Session = serde_json::from_str(json).unwrap();
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[1].role, crate::core::types::Role::User);
        let text = session.messages[1].text();
        assert!(text.contains("fn main()"));
        assert!(text.contains("what is this"));
        // Sessions persisted before evidence landed must still deserialize.
        assert!(session.evidence.records.is_empty());
    }

    /// Feasibility scan for the evidence-backed handoff RFC.
    ///
    /// Reads every session under `~/.config/luma/sessions/`, verifies
    /// round-trip safety, and prints distributions that validate the
    /// proposal numerically:
    ///   - would Phase 1 serde migration be safe?
    ///   - how much would transcripts actually shrink?
    ///   - which tools dominate tool_result bytes?
    ///   - which `EvidenceKind` variants are non-negligible?
    ///
    /// Ignored so CI doesn't run it (CI has no real sessions). Invoke:
    ///   cargo test --release core::session::tests::rfc_feasibility_scan -- --ignored --nocapture
    #[test]
    #[ignore]
    fn rfc_feasibility_scan() {
        use crate::core::types::{ContentBlock, Role};
        use std::collections::BTreeMap;

        let dir = sessions_dir();
        let Ok(entries) = fs::read_dir(&dir) else {
            eprintln!("no session dir: {}", dir.display());
            return;
        };

        let mut scanned = 0usize;
        let mut deser_failed: Vec<(PathBuf, String)> = Vec::new();
        let mut round_trip_failed: Vec<PathBuf> = Vec::new();

        let mut session_bytes: Vec<u64> = Vec::new();
        let mut session_msgs: Vec<usize> = Vec::new();

        let mut tool_result_sizes: Vec<usize> = Vec::new();
        let mut tool_result_by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut tool_use_counts: BTreeMap<String, usize> = BTreeMap::new();

        let mut text_block_sizes: Vec<usize> = Vec::new();
        let mut block_kinds: BTreeMap<&'static str, usize> = BTreeMap::new();

        let mut already_truncated = 0usize;
        let mut tool_result_ge_32k = 0usize;
        let mut tool_result_ge_8k = 0usize;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let size = raw.len() as u64;

            let session: Session = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    deser_failed.push((path.clone(), e.to_string()));
                    continue;
                }
            };

            // Round-trip: reserialize and re-parse. Not byte-equal (field
            // order may differ) but must be stable on second pass.
            match serde_json::to_string(&session) {
                Ok(round) => {
                    if serde_json::from_str::<Session>(&round).is_err() {
                        round_trip_failed.push(path.clone());
                    }
                }
                Err(_) => round_trip_failed.push(path.clone()),
            }

            scanned += 1;
            session_bytes.push(size);
            session_msgs.push(session.messages.len());

            let mut use_id_to_name: BTreeMap<String, String> = BTreeMap::new();
            for msg in &session.messages {
                if msg.role != Role::Assistant {
                    continue;
                }
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, .. } = block {
                        use_id_to_name.insert(id.clone(), name.clone());
                        *tool_use_counts.entry(name.clone()).or_default() += 1;
                    }
                }
            }

            for msg in &session.messages {
                for block in &msg.content {
                    let kind = match block {
                        ContentBlock::Text { .. } => "text",
                        ContentBlock::Paste { .. } => "paste",
                        ContentBlock::Image { .. } => "image",
                        ContentBlock::ToolUse { .. } => "tool_use",
                        ContentBlock::ToolResult { .. } => "tool_result",
                        ContentBlock::Thinking { .. } => "thinking",
                        ContentBlock::RedactedThinking { .. } => "redacted_thinking",
                    };
                    *block_kinds.entry(kind).or_default() += 1;

                    match block {
                        ContentBlock::Text { text } | ContentBlock::Paste { text } => {
                            text_block_sizes.push(text.chars().count());
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            let text = content.as_text();
                            let chars = text.chars().count();
                            tool_result_sizes.push(chars);
                            if text.contains("[truncated]") || text.contains("middle truncated") {
                                already_truncated += 1;
                            }
                            if chars >= 32_000 {
                                tool_result_ge_32k += 1;
                            }
                            if chars >= 8_000 {
                                tool_result_ge_8k += 1;
                            }
                            let key = use_id_to_name
                                .get(tool_use_id)
                                .cloned()
                                .unwrap_or_else(|| "<orphan>".into());
                            tool_result_by_name.entry(key).or_default().push(chars);
                        }
                        _ => {}
                    }
                }
            }
        }

        println!("\n=== Feasibility scan: evidence-backed handoff ===");
        println!("scanned:         {scanned} sessions");
        println!("deser failed:    {}", deser_failed.len());
        for (p, e) in deser_failed.iter().take(5) {
            println!("  - {}: {}", p.display(), e);
        }
        println!("round-trip fail: {}", round_trip_failed.len());
        for p in round_trip_failed.iter().take(5) {
            println!("  - {}", p.display());
        }

        println!("\n--- session size ---");
        report_u64("bytes", &session_bytes);
        report_usize("messages", &session_msgs);

        println!("\n--- text/paste block chars ---");
        report_usize("chars", &text_block_sizes);

        println!("\n--- tool_result chars ---");
        report_usize("chars", &tool_result_sizes);
        println!("already marked truncated:               {already_truncated}");
        println!("tool_result >= 32K (agent cap today):   {tool_result_ge_32k}");
        println!("tool_result >=  8K (would be evidence): {tool_result_ge_8k}");

        println!("\n--- tool_result by tool (top 20 by total bytes) ---");
        let mut by_name: Vec<(&String, &Vec<usize>)> = tool_result_by_name.iter().collect();
        by_name.sort_by_key(|(_, v)| std::cmp::Reverse(v.iter().sum::<usize>()));
        for (name, sizes) in by_name.iter().take(20) {
            let total: usize = sizes.iter().sum();
            let max = sizes.iter().copied().max().unwrap_or(0);
            let ge8k = sizes.iter().filter(|&&s| s >= 8_000).count();
            let ge32k = sizes.iter().filter(|&&s| s >= 32_000).count();
            println!(
                "  {name:<20} n={:>5}  total={:>10}  max={:>7}  >=8K={:>4}  >=32K={:>4}",
                sizes.len(),
                total,
                max,
                ge8k,
                ge32k
            );
        }

        println!("\n--- block kinds ---");
        for (k, n) in &block_kinds {
            println!("  {k:<18} {n}");
        }

        println!("\n--- tool_use invocations (top 25) ---");
        let mut tu: Vec<(&String, &usize)> = tool_use_counts.iter().collect();
        tu.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (name, n) in tu.iter().take(25) {
            println!("  {name:<20} {n}");
        }

        println!("\n--- projected transcript savings ---");
        project_savings(&tool_result_sizes);

        // Round-trip must be safe today — otherwise Phase 1 migration has
        // a pre-existing bug to fix first.
        assert!(
            round_trip_failed.is_empty(),
            "{} sessions fail round-trip today",
            round_trip_failed.len()
        );
    }

    #[cfg(test)]
    fn report_u64(label: &str, v: &[u64]) {
        if v.is_empty() {
            println!("  {label}: (empty)");
            return;
        }
        let mut s = v.to_vec();
        s.sort_unstable();
        let sum: u64 = s.iter().sum();
        let mean = sum as f64 / s.len() as f64;
        println!(
            "  {label:<10} n={:<5} min={:<10} p50={:<10} p90={:<10} p99={:<10} max={:<10} mean={:.0}",
            s.len(),
            s[0],
            pct_u64(&s, 50),
            pct_u64(&s, 90),
            pct_u64(&s, 99),
            s[s.len() - 1],
            mean
        );
    }

    #[cfg(test)]
    fn report_usize(label: &str, v: &[usize]) {
        if v.is_empty() {
            println!("  {label}: (empty)");
            return;
        }
        let mut s = v.to_vec();
        s.sort_unstable();
        let sum: usize = s.iter().sum();
        let mean = sum as f64 / s.len() as f64;
        println!(
            "  {label:<10} n={:<6} min={:<8} p50={:<8} p90={:<8} p99={:<8} max={:<8} mean={:.0}",
            s.len(),
            s[0],
            pct_usize(&s, 50),
            pct_usize(&s, 90),
            pct_usize(&s, 99),
            s[s.len() - 1],
            mean
        );
    }

    #[cfg(test)]
    fn pct_u64(sorted: &[u64], p: u64) -> u64 {
        let idx = ((sorted.len() as u64 - 1) * p / 100) as usize;
        sorted[idx]
    }

    #[cfg(test)]
    fn pct_usize(sorted: &[usize], p: usize) -> usize {
        let idx = (sorted.len() - 1) * p / 100;
        sorted[idx]
    }

    #[cfg(test)]
    fn project_savings(tool_result_sizes: &[usize]) {
        const SUMMARY_CHARS: usize = 200;
        let total: usize = tool_result_sizes.iter().sum();
        for threshold in [4_000usize, 8_000, 16_000] {
            let mut replaced_chars = 0usize;
            let mut replaced_count = 0usize;
            for &s in tool_result_sizes {
                if s >= threshold {
                    replaced_chars += s.saturating_sub(SUMMARY_CHARS);
                    replaced_count += 1;
                }
            }
            let pct = if total == 0 {
                0.0
            } else {
                replaced_chars as f64 / total as f64 * 100.0
            };
            println!(
                "  threshold>={threshold:>6}: {replaced_count:>4} tool_results → evidence, \
                 transcript shrinks {replaced_chars} chars ({pct:.1}% of tool_result total)"
            );
        }
    }
}
