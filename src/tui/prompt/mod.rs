/// Input prompt — struct definitions, public API, private helpers.
pub mod buffer;
mod completion;
mod keys;
mod render;

use buffer::PromptBuffer;
use completion::Completion;

pub use completion::Command;

/// Prompt result after handling a key or paste.
pub enum PromptAction {
    None,
    Redraw,
    Submit(
        Vec<crate::core::types::ContentBlock>,
        Vec<(String, Vec<u8>)>,
    ),
    ToggleThinking,
}

/// Input prompt state.
pub struct PromptState {
    pub(super) buf: PromptBuffer,
    pub(super) history: Vec<String>,
    pub(super) history_idx: Option<usize>,
    pub(super) comp: Completion,
}

impl PromptState {
    /// Create an empty prompt.
    pub fn new() -> Self {
        Self {
            buf: PromptBuffer::new(),
            history: Vec::new(),
            history_idx: None,
            comp: Completion::new(),
        }
    }

    /// Register a slash command.
    pub fn add_command(&mut self, name: impl Into<String>, desc: impl Into<String>) {
        self.comp.commands.push(Command {
            name: name.into(),
            desc: desc.into(),
            visible: true,
        });
    }

    /// Toggle visibility of a registered slash command.
    pub fn set_command_visible(&mut self, name: &str, visible: bool) {
        if let Some(cmd) = self.comp.commands.iter_mut().find(|c| c.name == name) {
            cmd.visible = visible;
        }
    }

    /// Attach an image at cursor position.
    pub fn attach_image(&mut self, media_type: String, data: Vec<u8>) {
        self.buf.attach_image(media_type, data);
    }

    /// Cursor column position for the renderer.
    pub fn cursor_column(&self) -> usize {
        self.buf.cursor_display_col()
    }

    // ── Dropdown helpers ──

    /// Whether the autocomplete dropdown is currently visible.
    pub fn has_dropdown(&self) -> bool {
        if let Some(q) = self.at_file_query() {
            return !self.comp.file_matches(&q).is_empty();
        }
        if self.buf.is_command() {
            let matches = self.get_matches();
            if matches.is_empty() {
                return false;
            }
            if matches.len() == 1 && matches[0].name == self.command_query() {
                return false;
            }
            return true;
        }
        false
    }

    pub(super) fn dropdown_count(&self) -> usize {
        if let Some(q) = self.at_file_query() {
            return self.comp.file_matches(&q).len().min(8);
        }
        if self.buf.is_command() {
            return self.get_matches().len();
        }
        0
    }

    /// Accept the currently selected dropdown item (Enter).
    pub(super) fn accept_dropdown(&mut self) {
        if let Some(query) = self.at_file_query() {
            let matches = self.comp.file_matches(&query);
            if let Some(path) = matches.get(self.comp.dropdown_idx) {
                let text = self.buf.text();
                let gpos = self.buf.text_pos();
                let before: String = text.chars().take(gpos).collect();
                if let Some(at_pos) = before.rfind('@') {
                    let after: String = text.chars().skip(gpos).collect();
                    let new = format!("{}@{} {}", &before[..at_pos], path, after);
                    let new_pos = before[..at_pos].chars().count() + 1 + path.chars().count() + 1;
                    self.buf.set_text(&new);
                    self.buf.pos = new_pos;
                }
            }
        } else if self.buf.is_command() {
            let matches = self.get_matches();
            if let Some(cmd) = matches.get(self.comp.dropdown_idx) {
                self.buf.set_text(&format!("/{}", cmd.name));
            }
        }
        self.comp.dropdown_idx = 0;
    }

    /// Tab-complete: fill the highlighted item into buffer without closing dropdown.
    pub(super) fn tab_fill_dropdown(&mut self) {
        if let Some(query) = self.at_file_query() {
            let matches = self.comp.file_matches(&query);
            if let Some(path) = matches.get(self.comp.dropdown_idx) {
                let text = self.buf.text();
                let gpos = self.buf.text_pos();
                let before: String = text.chars().take(gpos).collect();
                if let Some(at_pos) = before.rfind('@') {
                    let after: String = text.chars().skip(gpos).collect();
                    let new = format!("{}@{}{}", &before[..at_pos], path, after);
                    let new_pos = before[..at_pos].chars().count() + 1 + path.chars().count();
                    self.buf.set_text(&new);
                    self.buf.pos = new_pos;
                }
            }
        } else if self.buf.is_command() {
            let matches = self.get_matches();
            if let Some(cmd) = matches.get(self.comp.dropdown_idx) {
                self.buf.set_text(&format!("/{}", cmd.name));
            }
        }
    }

    // ── Private ──

    pub(super) fn command_query(&self) -> String {
        let t = self.buf.text();
        t.strip_prefix('/').unwrap_or("").to_owned()
    }

    pub(super) fn get_matches(&self) -> Vec<&Command> {
        if !self.buf.is_command() {
            return Vec::new();
        }
        let q = self.command_query();
        self.comp.command_matches(&q)
    }

    pub(super) fn at_file_query(&self) -> Option<String> {
        let text = self.buf.text();
        let gpos = self.buf.text_pos();
        let before: String = text.chars().take(gpos).collect();
        let at_pos = before.rfind('@')?;
        if at_pos > 0 && !before.as_bytes()[at_pos - 1].is_ascii_whitespace() {
            return None;
        }
        let query = &before[at_pos + 1..];
        if query.contains(char::is_whitespace) {
            return None;
        }
        Some(query.to_owned())
    }

    pub(super) fn ghost(&self) -> String {
        if let Some(query) = self.at_file_query() {
            let matches = self.comp.file_matches(&query);
            if matches.len() == 1 && matches[0] != query {
                return matches[0][query.len()..].to_owned();
            }
            return String::new();
        }
        if !self.buf.is_command() {
            return String::new();
        }
        let matches = self.get_matches();
        if matches.len() != 1 {
            return String::new();
        }
        let q = self.command_query();
        if matches[0].name == q {
            return String::new();
        }
        matches[0].name[q.len()..].to_owned()
    }

    pub(super) fn apply_ghost(&mut self) {
        let g = self.ghost();
        if !g.is_empty() {
            self.buf.insert_str(&g);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termina::event::{KeyCode, KeyEvent, Modifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::ALT)
    }

    fn type_str(p: &mut PromptState, s: &str) {
        for c in s.chars() {
            p.handle_key(&key(KeyCode::Char(c)));
        }
    }

    fn submit_text(p: &mut PromptState) -> Option<String> {
        match p.handle_key(&key(KeyCode::Enter)) {
            PromptAction::Submit(content, _) => {
                Some(crate::core::types::Message::content_text(&content))
            }
            _ => None,
        }
    }

    #[test]
    fn type_and_submit() {
        let mut p = PromptState::new();
        type_str(&mut p, "hello");
        assert_eq!(submit_text(&mut p).unwrap(), "hello");
    }

    #[test]
    fn empty_enter_no_submit() {
        let mut p = PromptState::new();
        assert!(matches!(
            p.handle_key(&key(KeyCode::Enter)),
            PromptAction::Redraw
        ));
    }

    #[test]
    fn history_navigation() {
        let mut p = PromptState::new();
        type_str(&mut p, "first");
        p.handle_key(&key(KeyCode::Enter));
        type_str(&mut p, "second");
        p.handle_key(&key(KeyCode::Enter));
        p.handle_key(&key(KeyCode::Up));
        assert_eq!(p.buf.text(), "second");
        p.handle_key(&key(KeyCode::Up));
        assert_eq!(p.buf.text(), "first");
    }

    #[test]
    fn command_ghost() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        type_str(&mut p, "/mo");
        assert_eq!(p.ghost(), "del");
    }

    #[test]
    fn command_submit() {
        let mut p = PromptState::new();
        p.add_command("new", "new thread");
        type_str(&mut p, "/new");
        assert_eq!(submit_text(&mut p).unwrap(), "/new");
    }

    #[test]
    fn ctrl_c_passthrough() {
        // Ctrl+C handled by dispatch, prompt returns None
        let mut p = PromptState::new();
        type_str(&mut p, "hello");
        assert!(matches!(p.handle_key(&ctrl('c')), PromptAction::None));
    }

    #[test]
    fn ctrl_c_handled_by_dispatch() {
        let mut p = PromptState::new();
        // Ctrl+C intercepted by dispatch before reaching prompt
        assert!(matches!(p.handle_key(&ctrl('c')), PromptAction::None));
    }

    #[test]
    fn paste_short_inline() {
        let mut p = PromptState::new();
        type_str(&mut p, "pre ");
        p.handle_paste("hello".into());
        assert_eq!(p.buf.text(), "pre hello");
    }

    #[test]
    fn paste_long_becomes_block() {
        let mut p = PromptState::new();
        type_str(&mut p, "fix: ");
        p.handle_paste("1\n2\n3\n4\n5".into());
        let content = p.buf.to_content();
        assert!(
            content
                .iter()
                .any(|b| matches!(b, crate::core::types::ContentBlock::Paste { .. }))
        );
    }

    #[test]
    fn paste_crlf_normalized() {
        let mut p = PromptState::new();
        p.handle_paste("a\r\nb\r\nc".into());
        assert_eq!(p.buf.text(), "a\nb\nc");
    }

    #[test]
    fn paste_cr_normalized() {
        let mut p = PromptState::new();
        p.handle_paste("a\rb\rc".into());
        assert_eq!(p.buf.text(), "a\nb\nc");
    }

    #[test]
    fn paste_too_large_rejected() {
        let mut p = PromptState::new();
        let huge = "x".repeat(2_000_000);
        assert!(p.handle_paste(huge).is_none());
        assert!(p.buf.is_empty());
    }

    #[test]
    fn image_inline_at_cursor() {
        use crate::core::types::ContentBlock;
        let mut p = PromptState::new();
        type_str(&mut p, "before");
        p.attach_image("image/png".into(), vec![1]);
        type_str(&mut p, "after");
        let content = p.buf.to_content();
        assert!(matches!(&content[0], ContentBlock::Text { text } if text == "before"));
        assert!(matches!(&content[1], ContentBlock::Image { .. }));
        assert!(matches!(&content[2], ContentBlock::Text { text } if text == "after"));
    }

    #[test]
    fn backspace_deletes_image() {
        let mut p = PromptState::new();
        type_str(&mut p, "a");
        p.attach_image("image/png".into(), vec![1]);
        p.handle_key(&key(KeyCode::Backspace));
        assert_eq!(p.buf.to_content().len(), 1);
    }

    #[test]
    fn alt_enter_newline() {
        let mut p = PromptState::new();
        type_str(&mut p, "line1");
        p.handle_key(&alt(KeyCode::Enter));
        type_str(&mut p, "line2");
        assert_eq!(p.buf.text(), "line1\nline2");
    }

    #[test]
    fn at_file_query_detected() {
        let mut p = PromptState::new();
        type_str(&mut p, "check @src/");
        assert_eq!(p.at_file_query(), Some("src/".into()));
    }

    #[test]
    fn at_file_query_email_ignored() {
        let mut p = PromptState::new();
        type_str(&mut p, "user@example.com ");
        assert_eq!(p.at_file_query(), None);
    }

    #[test]
    fn dropdown_arrow_navigates() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        assert!(p.has_dropdown());
        p.handle_key(&key(KeyCode::Down));
        assert_eq!(p.comp.dropdown_idx, 1);
        p.handle_key(&key(KeyCode::Up));
        assert_eq!(p.comp.dropdown_idx, 0);
    }

    #[test]
    fn dropdown_enter_accepts() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        p.handle_key(&key(KeyCode::Down));
        p.handle_key(&key(KeyCode::Enter));
        assert_eq!(p.buf.text(), "/new");
    }

    #[test]
    fn command_exact_no_dropdown() {
        let mut p = PromptState::new();
        p.add_command("new", "new thread");
        type_str(&mut p, "/new");
        assert!(!p.has_dropdown());
    }

    #[test]
    fn tab_fills_highlighted_item() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        // Tab fills first item text, dropdown stays open for further navigation
        p.handle_key(&key(KeyCode::Tab));
        assert_eq!(p.buf.text(), "/model");
    }

    #[test]
    fn tab_fills_navigated_item() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        p.handle_key(&key(KeyCode::Down));
        p.handle_key(&key(KeyCode::Tab));
        assert_eq!(p.buf.text(), "/new");
    }

    #[test]
    fn tab_does_not_reset_dropdown_idx() {
        let mut p = PromptState::new();
        p.add_command("model", "switch model");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        p.handle_key(&key(KeyCode::Down));
        p.handle_key(&key(KeyCode::Tab));
        // dropdown_idx preserved — Tab only fills, does not close
        assert_eq!(p.comp.dropdown_idx, 1);
    }

    #[test]
    fn set_command_visible_hides_from_dropdown() {
        let mut p = PromptState::new();
        p.add_command("resume", "resume last session");
        p.add_command("new", "new thread");
        type_str(&mut p, "/");
        assert_eq!(p.get_matches().len(), 2);
        p.buf.clear();
        p.set_command_visible("resume", false);
        type_str(&mut p, "/");
        assert_eq!(p.get_matches().len(), 1);
        assert_eq!(p.get_matches()[0].name, "new");
    }
}
