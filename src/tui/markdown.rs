/// Markdown → styled Line renderer: block-level state machine.
///
/// Delegates inline parsing, table rendering, and syntax highlighting
/// to submodules.
pub mod highlight;
mod inline;
mod table;

pub use inline::{parse_inline, parse_inline_streaming};
pub use table::{is_table_line, render_table};

use crate::tui::text::{Line, Span};
use crate::tui::theme::palette;
use smallvec::{SmallVec, smallvec};

/// Block-level parser state.
#[derive(Debug, Clone)]
pub enum BlockState {
    Normal,
    CodeFence {
        lang: String,
    },
    Table {
        alignments: Vec<table::Align>,
        widths: Vec<usize>,
    },
}

impl BlockState {
    /// Initial state.
    pub fn new() -> Self {
        Self::Normal
    }
}

/// Parse one line of markdown with current state.
pub fn parse_line(raw: &str, state: &BlockState) -> (Vec<Line>, BlockState) {
    if let Some(lang) = detect_fence(raw) {
        return match state {
            BlockState::CodeFence { .. } => (vec![Line::empty()], BlockState::Normal),
            _ => (
                vec![Line::empty()],
                BlockState::CodeFence {
                    lang: lang.to_owned(),
                },
            ),
        };
    }

    if let BlockState::CodeFence { lang } = state {
        let spans = highlight::highlight_code_with_lang(raw, Some(lang));
        return (vec![Line::new(spans)], state.clone());
    }

    if is_table_line(raw) {
        return table::parse_table_line(raw, state);
    }

    let new_state = if matches!(state, BlockState::Table { .. }) {
        BlockState::Normal
    } else {
        state.clone()
    };

    let trimmed = raw.trim_start();

    if is_horizontal_rule(trimmed) {
        return (vec![Line::empty()], new_state);
    }

    // Headers — parse inline markdown (bold, code, etc.) within header text
    if let Some(text) = trimmed.strip_prefix("### ") {
        let mut spans = parse_inline(text);
        for s in &mut spans {
            s.fg = palette::ACCENT;
        }
        return (vec![Line::new(spans)], new_state);
    }
    if let Some(text) = trimmed.strip_prefix("## ") {
        let mut spans = parse_inline(text);
        for s in &mut spans {
            s.fg = palette::ACCENT;
            s.bold = true;
        }
        return (vec![Line::new(spans)], new_state);
    }
    if let Some(text) = trimmed.strip_prefix("# ") {
        let mut spans = parse_inline(text);
        for s in &mut spans {
            s.fg = palette::ACCENT;
            s.bold = true;
        }
        return (vec![Line::new(spans)], new_state);
    }

    // Unordered lists
    if let Some(caps) = detect_list(raw) {
        let depth = caps.indent.len() / 2;
        let indent = "  ".repeat(depth);
        let spans = parse_inline(&caps.text);
        let mut all: SmallVec<[Span; 4]> =
            smallvec![Span::new(format!("{indent}  • "), palette::DIM)];
        all.extend(spans);
        return (vec![Line::new(all)], new_state);
    }

    // Numbered lists
    if let Some(caps) = detect_numbered_list(raw) {
        let depth = caps.indent.len() / 2;
        let indent = "  ".repeat(depth);
        let spans = parse_inline(&caps.text);
        let mut all: SmallVec<[Span; 4]> = smallvec![Span::new(
            format!("{indent}  {}. ", caps.number),
            palette::DIM
        )];
        all.extend(spans);
        return (vec![Line::new(all)], new_state);
    }

    // Blockquote
    if let Some(quoted) = trimmed
        .strip_prefix("> ")
        .or_else(|| trimmed.strip_prefix(">"))
    {
        let spans = parse_inline(quoted.trim_start());
        let mut all: SmallVec<[Span; 4]> = smallvec![Span::deco("│ ".to_owned(), palette::BORDER)];
        all.extend(spans.into_iter().map(|mut s| {
            s.fg = palette::DIM;
            s
        }));
        return (vec![Line::new(all)], new_state);
    }

    if trimmed.is_empty() {
        return (vec![Line::empty()], new_state);
    }

    let spans = parse_inline(trimmed);
    (vec![Line::new(spans)], new_state)
}

// ── Block-level helpers ──

fn detect_fence(raw: &str) -> Option<&str> {
    raw.trim().strip_prefix("```").map(|rest| rest.trim())
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let mut count = 0u32;
    let mut rule_char = None;
    for c in trimmed.chars() {
        if c == ' ' {
            continue;
        }
        match rule_char {
            None if c == '-' || c == '*' || c == '_' => {
                rule_char = Some(c);
                count += 1;
            }
            Some(rc) if c == rc => count += 1,
            _ => return false,
        }
    }
    count >= 3
}

struct ListCapture {
    indent: String,
    text: String,
    number: String,
}

fn detect_list(raw: &str) -> Option<ListCapture> {
    let mut indent = String::new();
    for (i, ch) in raw.char_indices() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
            continue;
        }
        if (ch == '-' || ch == '*') && raw.get(i + 1..i + 2) == Some(" ") {
            return Some(ListCapture {
                indent,
                text: raw[i + 2..].to_owned(),
                number: String::new(),
            });
        }
        return None;
    }
    None
}

fn detect_numbered_list(raw: &str) -> Option<ListCapture> {
    let trimmed = raw.trim_start();
    let indent: String = raw[..raw.len() - trimmed.len()].to_owned();
    let dot_pos = trimmed.find(". ")?;
    if trimmed[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
        Some(ListCapture {
            indent,
            number: trimmed[..dot_pos].to_owned(),
            text: trimmed[dot_pos + 2..].to_owned(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_fence_toggle() {
        let state = BlockState::new();
        let (_, new_state) = parse_line("```rust", &state);
        assert!(matches!(&new_state, BlockState::CodeFence { lang } if lang == "rust"));
        let (lines, state2) = parse_line("let x = 1;", &new_state);
        assert!(!lines.is_empty());
        assert!(matches!(state2, BlockState::CodeFence { .. }));
        let (_, state3) = parse_line("```", &state2);
        assert!(matches!(state3, BlockState::Normal));
    }

    #[test]
    fn header_parsing() {
        let (lines, _) = parse_line("# Title", &BlockState::new());
        let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Title"));
    }

    #[test]
    fn list_parsing() {
        let (lines, _) = parse_line("- item one", &BlockState::new());
        let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("•"));
        assert!(text.contains("item one"));
    }

    #[test]
    fn horizontal_rule() {
        let (lines, _) = parse_line("---", &BlockState::new());
        assert_eq!(lines[0].visible_width(), 0);
        let (lines, _) = parse_line("***", &BlockState::new());
        assert_eq!(lines[0].visible_width(), 0);
        let (lines, _) = parse_line("- - -", &BlockState::new());
        assert_eq!(lines[0].visible_width(), 0);
    }

    #[test]
    fn empty_line() {
        let (lines, _) = parse_line("", &BlockState::new());
        assert_eq!(lines[0].visible_width(), 0);
    }

    #[test]
    fn blockquote() {
        let (lines, _) = parse_line("> This is quoted", &BlockState::new());
        let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("This is quoted"));
        assert!(lines[0].spans[0].decoration);
    }

    #[test]
    fn table_state_transition() {
        let state = BlockState::new();
        let (_, state1) = parse_line("| Name | Age |", &state);
        assert!(matches!(state1, BlockState::Table { .. }));
        let (_, state2) = parse_line("|------|-----|", &state1);
        assert!(matches!(state2, BlockState::Table { .. }));
    }
}
