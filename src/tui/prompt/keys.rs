/// Key handling — keystrokes, paste text, history, dropdown.
use super::PromptAction;
use termina::event::{KeyCode, KeyEvent, Modifiers};

const PASTE_INLINE_THRESHOLD: usize = 5;
const PASTE_MAX_BYTES: usize = 1_048_576; // 1 MB

impl super::PromptState {
    /// Handle a key event.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PromptAction {
        if self.has_dropdown()
            && let Some(action) = self.handle_dropdown_key(key)
        {
            return action;
        }
        self.handle_normal_key(key)
    }

    /// Handle a bracketed paste of text. Returns None if paste exceeds size limit.
    pub fn handle_paste(&mut self, text: String) -> Option<PromptAction> {
        if text.len() > PASTE_MAX_BYTES {
            return None;
        }
        let normalized = normalize_newlines(&text);
        let line_count = count_lines(&normalized);
        if line_count < PASTE_INLINE_THRESHOLD {
            let trimmed = normalized.trim_end_matches('\n');
            self.buf.insert_str(trimmed);
        } else {
            self.buf.attach_paste(normalized);
        }
        Some(PromptAction::Redraw)
    }

    fn handle_dropdown_key(&mut self, key: &KeyEvent) -> Option<PromptAction> {
        match key.code {
            KeyCode::Up => {
                self.comp.dropdown_idx = self.comp.dropdown_idx.saturating_sub(1);
                Some(PromptAction::Redraw)
            }
            KeyCode::Down => {
                let count = self.dropdown_count();
                self.comp.dropdown_idx = (self.comp.dropdown_idx + 1).min(count.saturating_sub(1));
                Some(PromptAction::Redraw)
            }
            KeyCode::Tab => {
                self.tab_fill_dropdown();
                Some(PromptAction::Redraw)
            }
            KeyCode::Enter => {
                self.accept_dropdown();
                Some(PromptAction::Redraw)
            }
            KeyCode::Escape => {
                self.buf.clear();
                self.comp.dropdown_idx = 0;
                Some(PromptAction::Redraw)
            }
            _ => None,
        }
    }

    fn handle_normal_key(&mut self, key: &KeyEvent) -> PromptAction {
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let alt = key.modifiers.contains(Modifiers::ALT);

        match key.code {
            KeyCode::Enter if alt => {
                self.buf.insert('\n');
                PromptAction::Redraw
            }
            KeyCode::Enter => self.on_enter(),
            KeyCode::Tab => {
                self.apply_ghost();
                PromptAction::Redraw
            }
            // Ctrl+C handled by dispatch — should not reach here
            KeyCode::Char('c') if ctrl => PromptAction::None,
            KeyCode::Char('t') if ctrl => PromptAction::ToggleThinking,
            KeyCode::Char('a') if ctrl => {
                self.buf.home();
                PromptAction::Redraw
            }
            KeyCode::Char('e') if ctrl => {
                self.buf.end();
                PromptAction::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                self.buf.kill_before();
                PromptAction::Redraw
            }
            KeyCode::Escape => {
                if self.buf.is_command() || self.buf.line_count() > 1 {
                    self.buf.clear();
                    PromptAction::Redraw
                } else {
                    PromptAction::None
                }
            }
            KeyCode::Backspace => {
                self.buf.backspace();
                self.comp.dropdown_idx = 0;
                PromptAction::Redraw
            }
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::Left => {
                self.buf.left();
                PromptAction::Redraw
            }
            KeyCode::Right => {
                self.buf.right();
                PromptAction::Redraw
            }
            KeyCode::Char(c) => {
                self.buf.insert(c);
                self.comp.dropdown_idx = 0;
                if c == '@' {
                    self.comp.refresh_file_cache();
                }
                PromptAction::Redraw
            }
            _ => PromptAction::None,
        }
    }

    fn on_enter(&mut self) -> PromptAction {
        use crate::core::types::ContentBlock;
        if self.buf.is_command() {
            let g = self.ghost();
            if !g.is_empty() {
                self.buf.insert_str(&g);
                return PromptAction::Redraw;
            }
            let query = self.command_query();
            let found = self.comp.commands.iter().any(|c| c.name == query);
            self.buf.clear();
            if found {
                return PromptAction::Submit(vec![ContentBlock::Text {
                    text: format!("/{query}"),
                }]);
            }
            return PromptAction::Redraw;
        }
        if self.buf.is_empty() {
            return PromptAction::Redraw;
        }
        let content = self.buf.to_content();
        let flat = self.buf.trimmed_text();
        if !flat.is_empty() {
            self.history.push(flat);
        }
        self.history_idx = None;
        self.buf.clear();
        PromptAction::Submit(content)
    }

    fn history_prev(&mut self) -> PromptAction {
        if self.history.is_empty() || (self.history_idx.is_none() && !self.buf.is_empty()) {
            return PromptAction::Redraw;
        }
        let idx = self
            .history_idx
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_idx = Some(idx);
        self.buf
            .set_text(self.history.get(idx).map(|s| s.as_str()).unwrap_or(""));
        PromptAction::Redraw
    }

    fn history_next(&mut self) -> PromptAction {
        let Some(idx) = self.history_idx else {
            return PromptAction::Redraw;
        };
        let idx = (idx + 1).min(self.history.len());
        self.history_idx = Some(idx);
        self.buf
            .set_text(self.history.get(idx).map(|s| s.as_str()).unwrap_or(""));
        PromptAction::Redraw
    }
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.split('\n').count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_crlf() {
        assert_eq!(normalize_newlines("a\r\nb\r\nc"), "a\nb\nc");
    }

    #[test]
    fn normalize_cr() {
        assert_eq!(normalize_newlines("a\rb\rc"), "a\nb\nc");
    }

    #[test]
    fn count_lines_basic() {
        assert_eq!(count_lines("a\nb\nc"), 3);
        assert_eq!(count_lines(""), 0);
    }
}
