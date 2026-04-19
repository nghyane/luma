/// ScreenBuffer — 2D cell grid with line writing and per-row hashing.
use super::cell::{Cell, render_cells_ansi};
use super::{CellFlag, Line, Rgb, Span, char_width};
use std::iter::Peekable;
use std::str::Chars;

/// Compute base CellFlag from a Span's style booleans.
fn span_flags(span: &Span) -> u8 {
    let mut f = CellFlag::NONE;
    if span.bold {
        f |= CellFlag::BOLD;
    }
    if span.italic {
        f |= CellFlag::ITALIC;
    }
    if span.decoration {
        f |= CellFlag::DECORATION;
    }
    f
}

/// Skip an OSC8 escape sequence. Returns true if sequence was consumed.
fn skip_osc8(ch: char, chars: &mut Peekable<Chars<'_>>) -> bool {
    if ch == '\x1b' && chars.peek() == Some(&']') {
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'\\') {
                chars.next();
                break;
            }
        }
        return true;
    }
    false
}

/// 2D grid of cells backing the terminal display.
pub struct ScreenBuffer {
    cells: Vec<Cell>,
    pub width: u16,
    pub height: u16,
}

impl ScreenBuffer {
    /// Create a buffer filled with blank cells.
    pub fn new(width: u16, height: u16, bg: Rgb) -> Self {
        let size = width as usize * height as usize;
        Self {
            cells: vec![Cell::blank(bg); size],
            width,
            height,
        }
    }

    /// Resize the buffer, filling new cells with bg.
    pub fn resize(&mut self, width: u16, height: u16, bg: Rgb) {
        self.width = width;
        self.height = height;
        let size = width as usize * height as usize;
        self.cells.resize(size, Cell::blank(bg));
        self.cells.fill(Cell::blank(bg));
    }

    /// Fill entire buffer with a background color.
    pub fn clear(&mut self, bg: Rgb) {
        self.cells.fill(Cell::blank(bg));
    }

    /// Get a cell at (row, col) — 0-indexed.
    pub fn get(&self, row: u16, col: u16) -> &Cell {
        &self.cells[row as usize * self.width as usize + col as usize]
    }

    /// Get mutable cell at (row, col) — 0-indexed.
    pub fn get_mut(&mut self, row: u16, col: u16) -> &mut Cell {
        let idx = row as usize * self.width as usize + col as usize;
        &mut self.cells[idx]
    }

    /// Fill a rectangular region with padding cells.
    pub fn fill_padding(&mut self, row: u16, col: u16, w: u16, h: u16, bg: Rgb) {
        for r in row..row + h {
            for c in col..col + w {
                if r < self.height && c < self.width {
                    *self.get_mut(r, c) = Cell::padding(bg);
                }
            }
        }
    }

    /// Fill a rectangular region with blank cells (content bg).
    pub fn fill_bg(&mut self, row: u16, col: u16, w: u16, h: u16, bg: Rgb) {
        for r in row..row + h {
            for c in col..col + w {
                if r < self.height && c < self.width {
                    *self.get_mut(r, c) = Cell::blank(bg);
                }
            }
        }
    }

    /// Write a Line into the buffer at (row, col) within width.
    pub fn write_line(&mut self, line: &Line, row: u16, col: u16, width: u16) -> u16 {
        self.write_line_decorated(line, row, col, width, 0)
    }

    /// Write a Line with decoration flags on the first `deco_chars` characters.
    pub fn write_line_decorated(
        &mut self,
        line: &Line,
        row: u16,
        col: u16,
        width: u16,
        deco_chars: u16,
    ) -> u16 {
        if row >= self.height {
            return 0;
        }
        let mut x = col;
        let max_col = (col + width).min(self.width);
        let deco_end = col + deco_chars;
        for span in &line.spans {
            let flag_base = span_flags(span);
            let mut chars = span.text.chars().peekable();
            while let Some(ch) = chars.next() {
                if x >= max_col {
                    return x - col;
                }
                if skip_osc8(ch, &mut chars) {
                    continue;
                }
                let cw = char_width(ch);
                // Expand tab to spaces.
                if ch == '\t' {
                    let tab_w = cw as u16;
                    for _ in 0..tab_w {
                        if x >= max_col {
                            break;
                        }
                        let cell = self.get_mut(row, x);
                        cell.ch = ' ';
                        cell.fg = span.fg;
                        if let Some(bg) = span.bg.or(line.bg) {
                            cell.bg = bg;
                        }
                        cell.flags = CellFlag(if x < deco_end {
                            span_flags(span) | CellFlag::DECORATION
                        } else {
                            span_flags(span)
                        });
                        x += 1;
                    }
                    continue;
                }
                // Skip zero-width chars (variation selectors, combining marks).
                if cw == 0 {
                    continue;
                }
                if x + cw as u16 > max_col {
                    return x - col;
                }
                let cell_bg = span.bg.or(line.bg);
                let cell = self.get_mut(row, x);
                cell.ch = ch;
                cell.fg = span.fg;
                if let Some(bg) = cell_bg {
                    cell.bg = bg;
                }
                let extra = if x < deco_end {
                    CellFlag::DECORATION
                } else {
                    0
                };
                cell.flags = CellFlag(flag_base | extra);
                if cw == 2 && x + 1 < max_col {
                    let bg = cell_bg.unwrap_or(self.get_mut(row, x).bg);
                    let pad = self.get_mut(row, x + 1);
                    pad.ch = ' ';
                    pad.fg = span.fg;
                    pad.bg = bg;
                    pad.flags = CellFlag(CellFlag::WIDE_PAD | CellFlag::PADDING);
                }
                x += cw as u16;
            }
        }
        x - col
    }

    /// Compute a hash for a row (for diff). Uses FxHash-style multiply-add
    /// instead of SipHash — this is a perf-critical per-frame operation and
    /// we don't need cryptographic strength, only good distribution.
    pub fn row_hash(&self, row: u16) -> u64 {
        let start = row as usize * self.width as usize;
        let end = start + self.width as usize;
        // FxHash constant — good avalanche for non-adversarial input.
        const K: u64 = 0x517cc1b727220a95;
        let mut h: u64 = 0xcbf29ce484222325;
        for cell in &self.cells[start..end] {
            // Pack cell fields into a single u64 chunk per cell, then fold.
            // char is 21-bit, Rgb/Rgb/flags fit in 56 bits.
            let packed = (cell.ch as u64)
                ^ (u64::from(cell.fg.0) << 24)
                ^ (u64::from(cell.fg.1) << 32)
                ^ (u64::from(cell.fg.2) << 40)
                ^ (u64::from(cell.bg.0) << 48)
                ^ (u64::from(cell.bg.1) << 56);
            h = (h ^ packed).wrapping_mul(K);
            // Second round to absorb bg.2 and flags without collision.
            let tail = u64::from(cell.bg.2) | (u64::from(cell.flags.0) << 8);
            h = (h ^ tail).wrapping_mul(K);
        }
        h
    }

    /// Render a row into an ANSI string for terminal output.
    pub fn render_row(&self, row: u16) -> String {
        let w = self.width as usize;
        let start = row as usize * w;
        render_cells_ansi(&self.cells[start..start + w])
    }

    /// Extract content text from cells in a range, skipping PADDING|DECORATION.
    pub fn extract_text(&self, row: u16, c0: u16, c1: u16) -> String {
        let mut out = String::new();
        for c in c0..c1.min(self.width) {
            let cell = self.get(row, c);
            if cell.flags.is_content() {
                out.push(cell.ch);
            }
        }
        out.trim_end().to_owned()
    }

    /// Get the column of the last non-space content cell (0-indexed, exclusive).
    pub fn content_end(&self, row: u16) -> u16 {
        let mut last = 0u16;
        for c in 0..self.width {
            let cell = self.get(row, c);
            if cell.flags.is_content() && cell.ch != ' ' {
                last = c + 1;
            }
        }
        last
    }

    /// Get the column of the first content cell (0-indexed).
    pub fn content_start(&self, row: u16) -> u16 {
        for c in 0..self.width {
            if self.get(row, c).flags.is_content() {
                return c;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::text::Span;
    use crate::tui::theme::palette;
    use smallvec::smallvec;

    #[test]
    fn write_line_basic() {
        let mut buf = ScreenBuffer::new(20, 3, palette::BG);
        let line = Line::new(smallvec![
            Span::new("hi", palette::FG),
            Span::new(" there", palette::DIM),
        ]);
        assert_eq!(buf.write_line(&line, 0, 0, 20), 8);
        assert_eq!(buf.get(0, 0).ch, 'h');
    }

    #[test]
    fn extract_text_skips_padding() {
        let mut buf = ScreenBuffer::new(20, 1, palette::BG);
        buf.fill_padding(0, 0, 2, 1, palette::BG);
        let line = Line::new(smallvec![Span::new("hello", palette::FG)]);
        buf.write_line(&line, 0, 2, 18);
        assert_eq!(buf.extract_text(0, 0, 10), "hello");
    }

    #[test]
    fn content_end_correct() {
        let mut buf = ScreenBuffer::new(20, 1, palette::BG);
        buf.write_line(
            &Line::new(smallvec![Span::new("abc", palette::FG)]),
            0,
            0,
            20,
        );
        assert_eq!(buf.content_end(0), 3);
    }

    #[test]
    fn decorated_write() {
        let mut buf = ScreenBuffer::new(20, 1, palette::BG);
        let line = Line::new(smallvec![
            Span::new("· ", palette::DIM),
            Span::new("hello", palette::FG),
        ]);
        buf.write_line_decorated(&line, 0, 0, 20, 2);
        assert!(!buf.get(0, 0).flags.is_content());
        assert!(buf.get(0, 2).flags.is_content());
    }

    #[test]
    fn row_hash_differs() {
        let mut buf = ScreenBuffer::new(10, 2, palette::BG);
        let h1 = buf.row_hash(0);
        buf.write_line(&Line::new(smallvec![Span::new("x", palette::FG)]), 0, 0, 10);
        assert_ne!(h1, buf.row_hash(0));
    }

    #[test]
    fn render_row_produces_ansi() {
        let mut buf = ScreenBuffer::new(5, 1, palette::BG);
        buf.write_line(&Line::new(smallvec![Span::new("ab", palette::FG)]), 0, 0, 5);
        assert!(buf.render_row(0).contains("ab"));
    }

    #[test]
    fn wide_char_does_not_shift_subsequent_cells() {
        let mut buf = ScreenBuffer::new(10, 1, palette::BG);
        buf.write_line(
            &Line::new(smallvec![Span::new("🚀x", palette::FG)]),
            0,
            0,
            10,
        );
        // Cell layout: [🚀][wide_pad][x][space]...
        assert_eq!(buf.get(0, 0).ch, '🚀');
        assert!(buf.get(0, 1).flags.is_wide_pad());
        assert_eq!(buf.get(0, 2).ch, 'x');
        // ANSI output should contain 🚀 immediately followed by x (no extra space)
        let ansi = buf.render_row(0);
        assert!(ansi.contains("🚀x"), "expected 🚀x adjacent, got: {ansi}");
    }
}
