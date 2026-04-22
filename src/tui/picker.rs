/// Interactive model picker overlay.
use crate::tui::text::{Line, Span};
use crate::tui::theme::{icon, palette};
use smallvec::smallvec;
use termina::event::{KeyCode, KeyEvent};

/// Picker result after handling a key.
pub enum PickerAction {
    None,
    Redraw,
    Select(String),
    Cancel,
}

/// An interactive overlay for selecting from a list.
pub struct Picker {
    items: Vec<String>,
    selected: usize,
    current: String,
    pub is_active: bool,
}

impl Picker {
    /// Create an inactive picker.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            current: String::new(),
            is_active: false,
        }
    }

    /// Open the picker with items and current selection.
    pub fn open(&mut self, items: Vec<String>, current: &str) {
        self.selected = items.iter().position(|s| s == current).unwrap_or(0);
        self.current = current.to_owned();
        self.items = items;
        self.is_active = true;
    }

    /// Handle a key event. Returns what the app should do.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PickerAction {
        if !self.is_active {
            return PickerAction::None;
        }
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PickerAction::Redraw
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
                PickerAction::Redraw
            }
            KeyCode::Enter => {
                let model = self.items.get(self.selected).cloned().unwrap_or_default();
                self.is_active = false;
                PickerAction::Select(model)
            }
            KeyCode::Escape => {
                self.is_active = false;
                PickerAction::Cancel
            }
            KeyCode::Char('c') if key.modifiers.contains(termina::event::Modifiers::CONTROL) => {
                self.is_active = false;
                PickerAction::Cancel
            }
            _ => PickerAction::Redraw,
        }
    }

    /// Render the picker overlay lines.
    pub fn lines(&self, max_height: usize) -> Vec<Line> {
        if !self.is_active || self.items.is_empty() {
            return Vec::new();
        }
        let max_visible = self.items.len().min(max_height.saturating_sub(2));
        let mut start = self.selected.saturating_sub(max_visible / 2);
        start = start.min(self.items.len().saturating_sub(max_visible));

        let mut lines = vec![Line::empty()];
        for i in start..start + max_visible {
            let model = &self.items[i];
            let is_current = *model == self.current;
            let is_selected = i == self.selected;
            let suffix = if is_current { "  ←" } else { "" };
            let prefix = format!("{}  ", icon::PROMPT);

            if is_selected {
                lines.push(Line {
                    spans: smallvec![
                        Span::deco(prefix.clone(), palette::ACCENT),
                        Span::bold(model.clone(), palette::ACCENT),
                        Span::new(suffix.to_owned(), palette::DIM),
                    ],
                    bg: Some(palette::SURFACE),
                    margin: false,
                    indent: 0,
                    bleed: 0,
                    deco: 0,
                });
            } else {
                lines.push(Line {
                    spans: smallvec![
                        Span::deco(prefix, palette::BORDER),
                        Span::new(
                            model.clone(),
                            if is_current {
                                palette::FG
                            } else {
                                palette::DIM
                            }
                        ),
                        Span::new(suffix.to_owned(), palette::MUTED),
                    ],
                    bg: None,
                    margin: false,
                    indent: 0,
                    bleed: 0,
                    deco: 0,
                });
            }
        }
        lines.push(Line::empty());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termina::event::Modifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, Modifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL)
    }

    #[test]
    fn open_and_select() {
        let mut p = Picker::new();
        let models = vec!["a".into(), "b".into(), "c".into()];
        p.open(models, "b");
        assert!(p.is_active);
        assert_eq!(p.selected, 1);
    }

    #[test]
    fn navigate_and_confirm() {
        let mut p = Picker::new();
        p.open(vec!["x".into(), "y".into(), "z".into()], "x");
        p.handle_key(&key(KeyCode::Down));
        assert_eq!(p.selected, 1);
        match p.handle_key(&key(KeyCode::Enter)) {
            PickerAction::Select(s) => assert_eq!(s, "y"),
            _ => panic!("expected Select"),
        }
        assert!(!p.is_active);
    }

    #[test]
    fn cancel() {
        let mut p = Picker::new();
        p.open(vec!["a".into()], "a");
        assert!(matches!(
            p.handle_key(&key(KeyCode::Escape)),
            PickerAction::Cancel
        ));
        assert!(!p.is_active);
    }

    #[test]
    fn cancel_ctrl_c() {
        let mut p = Picker::new();
        p.open(vec!["a".into()], "a");
        assert!(matches!(p.handle_key(&ctrl('c')), PickerAction::Cancel));
        assert!(!p.is_active);
    }

    #[test]
    fn lines_rendering() {
        let mut p = Picker::new();
        p.open(vec!["model-a".into(), "model-b".into()], "model-a");
        let lines = p.lines(20);
        assert!(lines.len() >= 3); // empty + 2 items + empty
        assert_eq!(lines[1].bleed, 0);
        assert_eq!(lines[2].bleed, 0);
    }

    #[test]
    fn bounds_clamping() {
        let mut p = Picker::new();
        p.open(vec!["only".into()], "only");
        p.handle_key(&key(KeyCode::Up)); // should stay at 0
        assert_eq!(p.selected, 0);
        p.handle_key(&key(KeyCode::Down)); // should stay at 0
        assert_eq!(p.selected, 0);
    }
}
