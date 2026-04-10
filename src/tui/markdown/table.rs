/// Table parsing and rendering — pipe-delimited markdown tables.
///
/// Tables are rendered responsive: when total width exceeds `max_width`,
/// columns shrink proportionally and cell content wraps inside each cell.
/// Row height grows to fit the tallest cell, like HTML `<td>`.
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

// ── Constants ──

const SEP: &str = " │ ";
const SEP_W: usize = 3;
const MIN_COL_W: usize = 6;

/// Render a batch of raw table lines into Lines with multi-line cell wrapping.
pub fn render_table(rows: &[String], max_width: usize) -> Vec<Line> {
    let mut all_cells: Vec<(Vec<String>, bool)> = Vec::new();
    let has_separator = rows.iter().any(|r| is_separator_line(r));
    let mut header_done = !has_separator;

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
    if num_cols == 0 {
        return Vec::new();
    }

    // Natural column widths from content.
    let mut col_widths = vec![0usize; num_cols];
    for (cells, _) in &all_cells {
        for (i, cell) in cells.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(cell_display_width(cell));
            }
        }
    }

    // Shrink columns to fit max_width if needed.
    if max_width > 0 {
        fit_columns(&mut col_widths, max_width);
    }

    let mut result = Vec::new();

    for (r, (cells, is_header)) in all_cells.iter().enumerate() {
        // Wrap each cell content into multiple lines within its allocated width.
        let mut wrapped_cells: Vec<Vec<String>> = Vec::with_capacity(num_cols);
        for (c, &col_w) in col_widths.iter().enumerate() {
            let text = cells.get(c).map(|s| s.as_str()).unwrap_or("");
            wrapped_cells.push(wrap_cell_text(text, col_w));
        }

        let row_height = wrapped_cells.iter().map(|wc| wc.len()).max().unwrap_or(1);

        // Render each visual line of this row.
        for line_idx in 0..row_height {
            let mut spans: SmallVec<[Span; 4]> = SmallVec::new();
            for c in 0..num_cols {
                if c > 0 {
                    spans.push(Span::deco(SEP.to_owned(), palette::BORDER));
                }
                let col_w = col_widths[c];
                let cell_text = wrapped_cells[c]
                    .get(line_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");

                if cell_text.is_empty() {
                    // Empty continuation — pad to column width.
                    spans.push(Span::new(" ".repeat(col_w), palette::FG));
                } else {
                    let rendered: SmallVec<[Span; 4]> = if *is_header {
                        smallvec![Span::bold(strip_inline_syntax(cell_text), palette::ACCENT)]
                    } else {
                        parse_inline(cell_text)
                    };
                    let rendered_w = spans_display_width(&rendered);
                    let pad = col_w.saturating_sub(rendered_w);
                    spans.extend(rendered);
                    if pad > 0 {
                        spans.push(Span::new(" ".repeat(pad), palette::FG));
                    }
                }
            }
            result.push(Line::new(spans));
        }

        // Header separator line after first row.
        if r == 0 && has_separator && all_cells.len() > 1 {
            let total_w: usize =
                col_widths.iter().sum::<usize>() + (num_cols.saturating_sub(1)) * SEP_W;
            result.push(Line::new(smallvec![Span::new(
                "─".repeat(total_w),
                palette::BORDER
            )]));
        }
    }
    result
}

// ── Column fitting ──

/// Shrink column widths to fit within `max_width`.
///
/// Strategy: keep columns that already fit a fair share at their natural width,
/// distribute remaining budget to wider columns proportionally, with a minimum
/// floor per column.
fn fit_columns(col_widths: &mut [usize], max_width: usize) {
    let num_cols = col_widths.len();
    let sep_overhead = num_cols.saturating_sub(1) * SEP_W;
    let available = max_width.saturating_sub(sep_overhead);

    let total_natural: usize = col_widths.iter().sum();
    if total_natural <= available {
        return;
    }

    // Two-pass: freeze small columns, shrink large ones proportionally.
    let fair_share = available / num_cols;
    let mut frozen = vec![false; num_cols];
    let mut frozen_total = 0usize;
    let mut unfrozen_natural = 0usize;

    for (i, &w) in col_widths.iter().enumerate() {
        if w <= fair_share.max(MIN_COL_W) {
            frozen[i] = true;
            frozen_total += w;
        } else {
            unfrozen_natural += w;
        }
    }

    let budget = available.saturating_sub(frozen_total);

    if unfrozen_natural == 0 {
        return;
    }

    for (i, w) in col_widths.iter_mut().enumerate() {
        if frozen[i] {
            continue;
        }
        let share = (*w as u64 * budget as u64 / unfrozen_natural as u64) as usize;
        *w = share.max(MIN_COL_W);
    }
}

// ── Cell text wrapping ──

/// Word-wrap cell text to fit within `width` display columns.
/// Takes inline markdown syntax into account when measuring width.
fn wrap_cell_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || cell_display_width(text) <= width {
        return vec![text.to_owned()];
    }
    wrap_raw_text(text, width)
}

/// Word-wrap raw markdown cell text at `width` display columns.
///
/// Accounts for inline syntax when measuring width: e.g. `**bold**` is
/// 4 display chars, not 8. Breaks at word boundaries (spaces).
fn wrap_raw_text(text: &str, width: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;

    for &word in &words {
        let word_w = cell_display_width(word);

        if current.is_empty() {
            // First word on line — always take it even if wider than width.
            current.push_str(word);
            current_w = word_w;
            continue;
        }

        // Space + word would exceed width → break line.
        let needed = 1 + word_w; // 1 for the space
        if current_w + needed > width {
            lines.push(current);
            current = word.to_owned();
            current_w = word_w;
        } else {
            current.push(' ');
            current.push_str(word);
            current_w += needed;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ── Inline syntax helpers ──

fn split_table_cells(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|s| s.to_owned()).collect()
}

/// Strip markdown inline syntax to get visible text.
/// Handles: `**bold**`, `` `code` ``, `[label](url)`, `~~strike~~`.
fn strip_inline_syntax(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // **bold**
        if bytes[pos] == b'*' && pos + 1 < bytes.len() && bytes[pos + 1] == b'*' {
            if let Some(end) = text[pos + 2..].find("**") {
                out.push_str(&text[pos + 2..pos + 2 + end]);
                pos = pos + 2 + end + 2;
                continue;
            }
            out.push_str("**");
            pos += 2;
            continue;
        }

        // *italic*
        if bytes[pos] == b'*' {
            if let Some(end) = text[pos + 1..].find('*') {
                let close_pos = pos + 1 + end;
                let inner = &text[pos + 1..close_pos];
                let is_double = close_pos + 1 < bytes.len() && bytes[close_pos + 1] == b'*';
                if !is_double
                    && !inner.is_empty()
                    && !inner.starts_with(' ')
                    && !inner.ends_with(' ')
                {
                    out.push_str(inner);
                    pos = close_pos + 1;
                    continue;
                }
            }
            out.push('*');
            pos += 1;
            continue;
        }

        // ~~strikethrough~~
        if bytes[pos] == b'~' && pos + 1 < bytes.len() && bytes[pos + 1] == b'~' {
            if let Some(end) = text[pos + 2..].find("~~") {
                out.push_str(&text[pos + 2..pos + 2 + end]);
                pos = pos + 2 + end + 2;
                continue;
            }
            out.push_str("~~");
            pos += 2;
            continue;
        }

        // `code`
        if bytes[pos] == b'`' {
            if let Some(end) = text[pos + 1..].find('`') {
                out.push_str(&text[pos + 1..pos + 1 + end]);
                pos = pos + 1 + end + 1;
                continue;
            }
            out.push('`');
            pos += 1;
            continue;
        }

        // [label](url)
        if bytes[pos] == b'[' {
            if let Some(close) = text[pos..].find("](") {
                let label = &text[pos + 1..pos + close];
                let url_start = pos + close + 2;
                if let Some(url_end) = text[url_start..].find(')') {
                    out.push_str(label);
                    pos = url_start + url_end + 1;
                    continue;
                }
            }
            out.push('[');
            pos += 1;
            continue;
        }

        // Plain char — advance by char (not byte) for UTF-8 safety.
        if let Some(ch) = text[pos..].chars().next() {
            out.push(ch);
            pos += ch.len_utf8();
        } else {
            pos += 1;
        }
    }

    out
}

/// Display width of cell content after stripping inline syntax.
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
        let lines = render_table(&rows, 80);
        assert_eq!(lines.len(), 4); // header + separator + 2 data
        for l in &lines {
            assert!(l.visible_width() > 0);
        }
    }

    #[test]
    fn table_fits_no_wrap() {
        let rows = vec![
            "| A | B |".to_owned(),
            "|---|---|".to_owned(),
            "| x | y |".to_owned(),
        ];
        let lines = render_table(&rows, 80);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn table_cell_wraps_when_narrow() {
        let rows = vec![
            "| Name | Description |".to_owned(),
            "|------|-------------|".to_owned(),
            "| Foo  | This is a very long description that should wrap inside the cell |"
                .to_owned(),
        ];
        let lines = render_table(&rows, 40);
        // Should have more than 3 lines because cell content wraps.
        assert!(
            lines.len() > 3,
            "expected wrapping, got {} lines",
            lines.len()
        );
        // All lines should have consistent visible width (columns aligned).
        let widths: Vec<usize> = lines.iter().map(|l| l.visible_width()).collect();
        let header_w = widths[0];
        // Separator may differ, data rows should match header.
        for (i, &w) in widths.iter().enumerate() {
            if i == 1 {
                continue; // separator line
            }
            assert_eq!(w, header_w, "line {i} width {w} != header width {header_w}");
        }
    }

    #[test]
    fn table_vietnamese() {
        let rows = vec![
            "| Tiết | Thứ 2 |".to_owned(),
            "|------|-------|".to_owned(),
            "| Tiết 1 (07:00) | Toán |".to_owned(),
        ];
        let lines = render_table(&rows, 80);
        assert_eq!(lines.len(), 3);
        let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Tiết"));
    }

    #[test]
    fn tree_not_detected_as_table() {
        assert!(!is_table_line("│   ├── src/"));
        assert!(!is_table_line("├── main.rs"));
        assert!(!is_table_line("|-- main.rs"));
        assert!(!is_table_line("|   |-- lib.rs"));
        assert!(!is_table_line("|   |   |-- mod.rs"));
        assert!(!is_table_line("| just text"));
        assert!(!is_table_line("| box |"));
        assert!(is_table_line("| col1 | col2 |"));
        assert!(is_table_line("|a|b|"));
    }

    #[test]
    fn strip_inline_link() {
        assert_eq!(
            strip_inline_syntax("[docs/file.md](http://example.com) text"),
            "docs/file.md text"
        );
    }

    #[test]
    fn strip_inline_strikethrough() {
        assert_eq!(strip_inline_syntax("~~removed~~ kept"), "removed kept");
    }

    #[test]
    fn strip_inline_bold_code() {
        assert_eq!(strip_inline_syntax("**bold** and `code`"), "bold and code");
    }

    #[test]
    fn wrap_cell_short() {
        let lines = wrap_cell_text("short", 20);
        assert_eq!(lines, vec!["short"]);
    }

    #[test]
    fn wrap_cell_long() {
        let lines = wrap_cell_text("one two three four five", 10);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(cell_display_width(l) <= 10, "line too wide: {l}");
        }
    }

    #[test]
    fn fit_columns_no_shrink() {
        let mut widths = vec![10, 10, 10];
        fit_columns(&mut widths, 80);
        assert_eq!(widths, vec![10, 10, 10]);
    }

    #[test]
    fn fit_columns_shrinks() {
        let mut widths = vec![50, 50, 10];
        fit_columns(&mut widths, 60);
        let total: usize = widths.iter().sum::<usize>() + 2 * SEP_W;
        assert!(total <= 60, "total {total} > 60");
    }
}
