/// Table parsing and rendering — pipe-delimited markdown tables.
use super::inline::parse_inline;
use crate::tui::text::{Line, Span, display_width};
use crate::tui::theme::palette;
use smallvec::{SmallVec, smallvec};

/// Table column alignment.
#[derive(Debug, Clone, Copy)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Check if a raw line is a table row.
///
/// Requires the line to start *and* end with `|` and contain at least 3 pipes
/// (i.e. two columns). This avoids false positives for tree diagrams like
/// `|   |-- lib.rs` which start with `|` but don't end with one, and single-cell
/// lines like `| box |` which have only 2 pipes.
pub fn is_table_line(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 3
}

/// Check if a raw line is a separator row (|---|---|).
pub fn is_separator_line(raw: &str) -> bool {
    let cells = split_table_cells(raw);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| c.trim().chars().all(|ch| ch == '-' || ch == ':'))
}

/// Parse a table line, returning state transition (no rendered lines).
pub fn parse_table_line(raw: &str, state: &super::BlockState) -> (Vec<Line>, super::BlockState) {
    let cells = split_table_cells(raw);
    if is_separator_line(raw) {
        let alignments: Vec<Align> = cells
            .iter()
            .map(|c| {
                let t = c.trim();
                if t.starts_with(':') && t.ends_with(':') {
                    Align::Center
                } else if t.ends_with(':') {
                    Align::Right
                } else {
                    Align::Left
                }
            })
            .collect();
        return (
            vec![],
            super::BlockState::Table {
                alignments,
                widths: vec![0; cells.len()],
            },
        );
    }
    let new_state = match state {
        super::BlockState::Table { alignments, widths } => super::BlockState::Table {
            alignments: alignments.clone(),
            widths: widths.clone(),
        },
        _ => super::BlockState::Table {
            alignments: vec![Align::Left; cells.len()],
            widths: vec![0; cells.len()],
        },
    };
    (vec![], new_state)
}

/// Render a batch of raw table lines into Lines with uniform column widths.
pub fn render_table(rows: &[String]) -> Vec<Line> {
    let mut all_cells: Vec<(Vec<String>, bool)> = Vec::new();
    let has_separator = rows.iter().any(|r| is_separator_line(r));
    let mut header_done = !has_separator; // No separator → no header styling

    for raw in rows {
        if is_separator_line(raw) {
            header_done = true;
            continue;
        }
        let cells: Vec<String> = split_table_cells(raw)
            .iter()
            .map(|c| c.trim().to_owned())
            .collect();
        all_cells.push((cells, !header_done));
    }

    let num_cols = all_cells.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; num_cols];
    for (cells, _) in &all_cells {
        for (i, cell) in cells.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(cell_display_width(cell));
            }
        }
    }

    let mut result = Vec::new();
    let sep = " │ ";

    for (r, (cells, is_header)) in all_cells.iter().enumerate() {
        let mut spans: SmallVec<[Span; 4]> = SmallVec::new();
        for (c, cell) in cells.iter().enumerate() {
            if c > 0 {
                spans.push(Span::deco(sep.to_owned(), palette::BORDER));
            }
            let rendered: SmallVec<[Span; 4]> = if *is_header {
                smallvec![Span::bold(strip_inline_syntax(cell), palette::ACCENT)]
            } else {
                parse_inline(cell)
            };
            let rendered_w = spans_display_width(&rendered);
            let col_w = col_widths.get(c).copied().unwrap_or(rendered_w);
            let pad = col_w.saturating_sub(rendered_w);
            spans.extend(rendered);
            if pad > 0 {
                spans.push(Span::new(" ".repeat(pad), palette::FG));
            }
        }
        result.push(Line::new(spans));

        if r == 0 && has_separator && all_cells.len() > 1 {
            let total_w: usize =
                col_widths.iter().sum::<usize>() + (num_cols.saturating_sub(1)) * sep.len();
            result.push(Line::new(smallvec![Span::new(
                "─".repeat(total_w),
                palette::BORDER
            )]));
        }
    }
    result
}

fn split_table_cells(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|s| s.to_owned()).collect()
}

fn strip_inline_syntax(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((pos, ch)) = chars.next() {
        if ch == '*' {
            if let Some(&(_, '*')) = chars.peek() {
                chars.next();
                if let Some(end) = text[pos + 2..].find("**") {
                    out.push_str(&text[pos + 2..pos + 2 + end]);
                    let skip_to = pos + 2 + end + 2;
                    while let Some(&(i, _)) = chars.peek() {
                        if i >= skip_to {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
                out.push_str("**");
                continue;
            }
            out.push('*');
            continue;
        }
        if ch == '`' {
            if let Some(end) = text[pos + 1..].find('`') {
                out.push_str(&text[pos + 1..pos + 1 + end]);
                let skip_to = pos + 1 + end + 1;
                while let Some(&(i, _)) = chars.peek() {
                    if i >= skip_to {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
            out.push('`');
            continue;
        }
        out.push(ch);
    }
    out
}

fn cell_display_width(text: &str) -> usize {
    display_width(&strip_inline_syntax(text))
}

fn spans_display_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| display_width(&s.text)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_header_and_data() {
        let rows = vec![
            "| Name  | Age |".to_owned(),
            "|-------|-----|".to_owned(),
            "| Alice | 30  |".to_owned(),
            "| Bob   | 25  |".to_owned(),
        ];
        let lines = render_table(&rows);
        assert_eq!(lines.len(), 4);
        for l in &lines {
            assert!(l.visible_width() > 0);
        }
    }

    #[test]
    fn table_vietnamese() {
        let rows = vec![
            "| Tiết | Thứ 2 |".to_owned(),
            "|------|-------|".to_owned(),
            "| Tiết 1 (07:00) | Toán |".to_owned(),
        ];
        let lines = render_table(&rows);
        assert_eq!(lines.len(), 3);
        let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Tiết"));
    }

    #[test]
    fn tree_not_detected_as_table() {
        // Box-drawing tree chars
        assert!(!is_table_line("│   ├── src/"));
        assert!(!is_table_line("├── main.rs"));
        // ASCII pipe tree (nested has multiple |)
        assert!(!is_table_line("|-- main.rs"));
        assert!(!is_table_line("|   |-- lib.rs"));
        assert!(!is_table_line("|   |   |-- mod.rs"));
        // Single-pipe text
        assert!(!is_table_line("| just text"));
        // Single-cell (only 2 pipes)
        assert!(!is_table_line("| box |"));
        // Actual tables (>= 3 pipes, starts and ends with |)
        assert!(is_table_line("| col1 | col2 |"));
        assert!(is_table_line("|a|b|"));
    }
}
