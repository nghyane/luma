/// Session — persistent conversation with JSON storage.
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

/// Directory for storing session assets (images, etc).
fn session_assets_dir(session_id: &str) -> PathBuf {
    sessions_dir().join(session_id)
}

/// Save image bytes to `sessions/{session_id}/{filename}`. Returns filename.
pub fn save_image(session_id: &str, data: &[u8], ext: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("img_{ts:x}.{ext}");
    let dir = session_assets_dir(session_id);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(&filename), data);
    filename
}

/// Read image as base64 from deterministic path `sessions/{session_id}/{image_id}`.
pub fn read_image_base64(session_id: &str, image_id: &str) -> String {
    use base64::Engine;
    let path = session_assets_dir(session_id).join(image_id);
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
    }
}
