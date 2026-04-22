/// Centered modal dialog — accounts list with actions.
use crate::tui::text::{Line, Span};
use crate::tui::theme::{icon, palette};
use smallvec::smallvec;
use termina::event::{KeyCode, KeyEvent, Modifiers};

pub enum DialogAction {
    None,
    Redraw,
    Toggle(String),
    Remove(String),
    Close,
}

pub struct DialogItem {
    pub id: String,
    pub col1: String, // primary text (email or label)
    pub col2: String, // secondary text (provider  status)
    pub dim: bool,    // disabled account
}

pub struct Dialog {
    pub is_active: bool,
    title: String,
    items: Vec<DialogItem>,
    selected: usize,
}

impl Dialog {
    pub fn new() -> Self {
        Self {
            is_active: false,
            title: String::new(),
            items: Vec::new(),
            selected: 0,
        }
    }

    pub fn open(&mut self, title: impl Into<String>, items: Vec<DialogItem>) {
        self.title = title.into();
        self.selected = 0;
        self.items = items;
        self.is_active = true;
    }

    pub fn items_is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn close(&mut self) {
        self.is_active = false;
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> DialogAction {
        if !self.is_active {
            return DialogAction::None;
        }
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                DialogAction::Redraw
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
                DialogAction::Redraw
            }
            KeyCode::Enter => {
                if let Some(item) = self.items.get(self.selected) {
                    DialogAction::Toggle(item.id.clone())
                } else {
                    DialogAction::None
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some(item) = self.items.get(self.selected) {
                    let id = item.id.clone();
                    self.items.remove(self.selected);
                    self.selected = self.selected.min(self.items.len().saturating_sub(1));
                    DialogAction::Remove(id)
                } else {
                    DialogAction::None
                }
            }
            KeyCode::Escape | KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.is_active = false;
                DialogAction::Close
            }
            KeyCode::Char('c') if key.modifiers.contains(Modifiers::CONTROL) => {
                self.is_active = false;
                DialogAction::Close
            }
            _ => DialogAction::Redraw,
        }
    }

    /// Build lines for the centered dialog. `term_w` is full terminal width.
    pub fn lines(&self, term_w: u16) -> Vec<Line> {
        if !self.is_active || self.items.is_empty() {
            return Vec::new();
        }

        // box_w = total width including the two border chars (│...│)
        let box_w = (term_w as usize).clamp(44, 68);

        // content area = box_w - 2 borders - 2 pads each side = box_w - 6
        // Layout per item row:
        //   │  [▶] ● col1 <pad> col2  │
        //   ↑  ↑↑  ↑               ↑↑ ↑
        //   1  12  1               12  1  = 8 chars overhead (non-selected)
        //   selected adds ▶+space = +2 = 10 overhead, but replaces one space
        // We use 8 as the fixed overhead (non-selected baseline).
        let overhead = 8; // │  ● <space> ... <space><space>│
        let content_w = box_w.saturating_sub(overhead);

        let mut rows: Vec<Line> = Vec::new();

        // ┌─ title ──────┐
        let title_str = format!(" {} ", self.title);
        let dashes = box_w.saturating_sub(2 + title_str.len());
        let dl = dashes / 2;
        let dr = dashes - dl;
        rows.push(Line::new(smallvec![
            Span::new(format!("┌{}", "─".repeat(dl)), palette::BORDER),
            Span::new(title_str, palette::FG),
            Span::new(format!("{}┐", "─".repeat(dr)), palette::BORDER),
        ]));

        // blank padding row
        rows.push(full_border_row(box_w));

        // items
        for (i, item) in self.items.iter().enumerate() {
            let is_sel = i == self.selected;

            let dot = if item.dim { "○" } else { "●" };
            let dot_color = if item.dim {
                palette::MUTED
            } else {
                palette::ACCENT
            };
            let col1_color = if is_sel {
                palette::FG
            } else if item.dim {
                palette::MUTED
            } else {
                palette::DIM
            };

            // col2 is right-aligned; col1 fills the rest
            let col2 = &item.col2;
            // col1_max: content_w minus col2 length minus 1 separator space
            let col1_max = content_w.saturating_sub(col2.len() + 1);
            let col1 = truncate(&item.col1, col1_max);
            let pad = content_w.saturating_sub(col1.len() + col2.len() + 1);

            if is_sel {
                // │ ▶ ● col1 <pad> col2  │
                rows.push(Line {
                    spans: smallvec![
                        Span::new("│ ".to_owned(), palette::BORDER),
                        Span::new(format!("{} ", icon::PROMPT), palette::ACCENT),
                        Span::new(format!("{dot} "), dot_color),
                        Span::new(col1, col1_color),
                        Span::new(format!("{:pad$} ", "", pad = pad), palette::DIM),
                        Span::new(col2.clone(), palette::MUTED),
                        Span::new("  │".to_owned(), palette::BORDER),
                    ],
                    bg: Some(palette::SURFACE),
                    margin: false,
                    indent: 0,
                    bleed: 0,
                    deco: 0,
                });
            } else {
                // │  ● col1 <pad> col2  │
                rows.push(Line::new(smallvec![
                    Span::new("│  ".to_owned(), palette::BORDER),
                    Span::new(format!("{dot} "), dot_color),
                    Span::new(col1, col1_color),
                    Span::new(format!("{:pad$} ", "", pad = pad), palette::DIM),
                    Span::new(col2.clone(), palette::MUTED),
                    Span::new("  │".to_owned(), palette::BORDER),
                ]));
            }
        }

        // blank row + hint
        rows.push(full_border_row(box_w));
        let hint = "enter: toggle  r: remove  esc: close";
        let hint_w = content_w + overhead - 4; // hint spans between │  and  │
        let hint_str = truncate(hint, hint_w);
        let hint_pad = hint_w.saturating_sub(hint_str.len());
        rows.push(Line::new(smallvec![
            Span::new("│  ".to_owned(), palette::BORDER),
            Span::new(hint_str, palette::MUTED),
            Span::new(format!("{:pad$}", "", pad = hint_pad), palette::DIM),
            Span::new("  │".to_owned(), palette::BORDER),
        ]));

        // └──────────────┘
        rows.push(Line::new(smallvec![Span::new(
            format!("└{}┘", "─".repeat(box_w.saturating_sub(2))),
            palette::BORDER,
        ),]));

        rows
    }

    /// 1-indexed (row, col) to center the dialog on screen.
    pub fn position(&self, term_w: u16, term_h: u16) -> (u16, u16) {
        let box_w = (term_w as usize).clamp(44, 68) as u16;
        // top+blank+items+blank+hint+bottom = items+5
        let box_h = (self.items.len() + 5) as u16;
        let row = term_h.saturating_sub(box_h) / 2 + 1;
        let col = term_w.saturating_sub(box_w) / 2 + 1;
        (row, col)
    }
}

fn full_border_row(box_w: usize) -> Line {
    Line::new(smallvec![
        Span::new("│".to_owned(), palette::BORDER),
        Span::new(
            format!("{:w$}", "", w = box_w.saturating_sub(2)),
            palette::DIM
        ),
        Span::new("│".to_owned(), palette::BORDER),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
