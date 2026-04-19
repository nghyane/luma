/// Cell-buffer renderer — regions paint into ScreenBuffer, diff per-row hash.
mod paint;

use crate::tui::text::{CellFlag, Line, Padding, ScreenBuffer};
use crate::tui::theme::Rgb;
use std::io::{self, BufWriter, Stdout, Write};

/// A named rectangular region on screen with padding.
#[derive(Debug, Clone)]
pub struct Region {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    pub bg: Rgb,
    pub padding: Padding,
}

impl Region {
    /// Content area width.
    pub fn content_width(&self) -> u16 {
        self.width
            .saturating_sub(self.padding.left + self.padding.right)
    }

    /// Content area height.
    pub fn content_height(&self) -> u16 {
        self.height
            .saturating_sub(self.padding.top + self.padding.bottom)
    }
}

/// Cursor position or hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    Visible { row: u16, col: u16 },
    Hidden,
}

/// Scrollbar cell type — supports fractional thumb edges.
#[derive(Clone, Copy)]
pub enum ScrollCell {
    Track,
    Thumb,
    TopEdge(u8),
    BottomEdge(u8),
}

/// Scrollbar overlay.
pub struct Overlay {
    pub row: u16,
    pub col: u16,
    pub fg_thumb: Rgb,
    pub fg_track: Rgb,
    pub cells: Vec<ScrollCell>,
}

/// Floating text layer — lines painted at absolute position over regions.
pub struct FloatingLayer {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub lines: Vec<Line>,
    pub bg: Rgb,
}

/// Screen-space selection highlight (1-indexed, inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub r0: u16,
    pub c0: u16,
    pub r1: u16,
    pub c1: u16,
}

struct RegionSlot {
    name: String,
    region: Region,
    lines: Vec<Line>,
}

/// Cell-buffer terminal renderer with per-row hash diff.
pub struct Renderer {
    out: BufWriter<Stdout>,
    slots: Vec<RegionSlot>,
    buf: ScreenBuffer,
    prev_hashes: Vec<u64>,
    overlay: Option<Overlay>,
    floating: Vec<FloatingLayer>,
    selection: Option<SelectionRange>,
    cursor: CursorState,
}

impl Renderer {
    /// Create renderer for given terminal dimensions.
    pub fn new(term_width: u16, term_height: u16) -> Self {
        Self {
            out: BufWriter::with_capacity(8192, io::stdout()),
            slots: Vec::new(),
            buf: ScreenBuffer::new(term_width, term_height, Rgb(0, 0, 0)),
            prev_hashes: Vec::new(),
            overlay: None,
            floating: Vec::new(),
            selection: None,
            cursor: CursorState::Hidden,
        }
    }

    /// Define a named region.
    pub fn define(&mut self, name: &str, region: Region) {
        self.slots.push(RegionSlot {
            name: name.to_owned(),
            region,
            lines: Vec::new(),
        });
    }

    /// Update region geometry.
    pub fn update_region(&mut self, name: &str, region: Region) {
        if let Some(slot) = self.slot_mut(name) {
            slot.region = region;
        }
    }

    /// Update terminal dimensions (on resize).
    pub fn set_term_size(&mut self, w: u16, h: u16) {
        let bg = self.default_bg();
        self.buf.resize(w, h, bg);
        self.prev_hashes.clear();
    }

    /// Set rendered lines for a region.
    pub fn set_lines(&mut self, name: &str, lines: &[Line]) {
        if let Some(slot) = self.slot_mut(name) {
            slot.lines.clear();
            slot.lines.extend_from_slice(lines);
        }
    }

    /// Set floating layers (dropdown, picker, etc). Cleared each frame.
    pub fn set_floating(&mut self, layers: Vec<FloatingLayer>) {
        self.floating = layers;
    }

    /// Set scrollbar overlay.
    pub fn set_overlay(&mut self, overlay: Option<Overlay>) {
        self.overlay = overlay;
    }

    /// Set selection highlight range.
    pub fn set_selection(&mut self, sel: Option<SelectionRange>) {
        self.selection = sel;
    }

    /// Set cursor state.
    pub fn set_cursor(&mut self, state: CursorState) {
        self.cursor = state;
    }

    /// Clear screen and invalidate previous frame.
    pub fn clear_screen(&mut self) {
        let _ = write!(self.out, "\x1b[2J");
        self.prev_hashes.clear();
    }

    /// Access the screen buffer (for selection copy).
    pub fn buffer(&self) -> &ScreenBuffer {
        &self.buf
    }

    /// Override bottom padding for a region (e.g. dropdown fills padding).
    pub fn set_bottom_padding(&mut self, name: &str, bottom: u16) {
        if let Some(slot) = self.slot_mut(name) {
            slot.region.padding.bottom = bottom;
        }
    }

    /// Compose all regions, diff against prev frame, write changes.
    pub fn flush(&mut self) -> io::Result<()> {
        self.buf.clear(self.default_bg());
        for i in 0..self.slots.len() {
            paint::paint_region(
                &mut self.buf,
                &self.slots[i].region,
                self.slots[i].lines.iter(),
            );
        }
        for layer in &self.floating {
            paint::paint_floating(&mut self.buf, layer);
        }
        self.floating.clear();
        self.paint_scrollbar();
        if let Some(sel) = self.selection {
            self.apply_selection(sel);
        }
        self.diff_and_write()
    }

    fn default_bg(&self) -> Rgb {
        self.slots
            .first()
            .map(|s| s.region.bg)
            .unwrap_or(Rgb(0, 0, 0))
    }

    fn slot_mut(&mut self, name: &str) -> Option<&mut RegionSlot> {
        self.slots.iter_mut().find(|s| s.name == name)
    }

    fn paint_scrollbar(&mut self) {
        let Some(ov) = &self.overlay else { return };
        const LOWER: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
        let col = ov.col.saturating_sub(1);
        for (j, &cell) in ov.cells.iter().enumerate() {
            let row = ov.row.saturating_sub(1) + j as u16;
            if row >= self.buf.height || col >= self.buf.width {
                continue;
            }
            if self.buf.get(row, col).flags.is_wide_pad() && col > 0 {
                self.buf.get_mut(row, col - 1).ch = ' ';
            }
            let c = self.buf.get_mut(row, col);
            c.flags = CellFlag(CellFlag::NONE);
            match cell {
                ScrollCell::Track => {
                    c.ch = ' ';
                    c.fg = ov.fg_track;
                    c.bg = ov.fg_track;
                }
                ScrollCell::Thumb => {
                    c.ch = ' ';
                    c.fg = ov.fg_thumb;
                    c.bg = ov.fg_thumb;
                }
                ScrollCell::TopEdge(eighths) => {
                    c.ch = LOWER[(8 - eighths).min(7) as usize];
                    c.fg = ov.fg_thumb;
                    c.bg = ov.fg_track;
                }
                ScrollCell::BottomEdge(eighths) => {
                    c.ch = LOWER[(8 - eighths).min(7) as usize];
                    c.fg = ov.fg_track;
                    c.bg = ov.fg_thumb;
                }
            }
        }
    }

    fn diff_and_write(&mut self) -> io::Result<()> {
        let total_rows = self.buf.height as usize;
        self.prev_hashes.resize(total_rows, 0);
        let mut any_diff = false;

        // Synchronized output (Mode 2026) — prevents flicker by batching
        // all row updates into a single visible frame. Terminals that don't
        // support it (e.g. legacy conhost) silently ignore these sequences.
        let _ = write!(self.out, "\x1b[?2026h");
        let _ = write!(self.out, "\x1b[?25l");
        for row in 0..self.buf.height {
            let hash = self.buf.row_hash(row);
            if hash != self.prev_hashes[row as usize] {
                let ansi = self.buf.render_row(row);
                let _ = write!(self.out, "\x1b[{};1H{}", row + 1, ansi);
                self.prev_hashes[row as usize] = hash;
                any_diff = true;
            }
        }
        if let CursorState::Visible { row, col } = self.cursor {
            // Position cursor + show + set steady block shape (DECSCUSR 2)
            // + set cursor color to bright white (OSC 12).
            let _ = write!(
                self.out,
                "\x1b[{row};{col}H\x1b[?25h\x1b[2 q\x1b]12;#cdd6f4\x1b\\"
            );
            any_diff = true;
        }
        let _ = write!(self.out, "\x1b[?2026l");
        if any_diff {
            self.out.flush()?;
        }
        Ok(())
    }

    fn apply_selection(&mut self, sel: SelectionRange) {
        for row_1 in sel.r0..=sel.r1 {
            let row = row_1.saturating_sub(1);
            if row >= self.buf.height {
                continue;
            }
            let cs = self.buf.content_start(row);
            let ce = self.buf.content_end(row);
            if cs >= ce {
                continue;
            }
            let c_start = if row_1 == sel.r0 {
                sel.c0.saturating_sub(1).max(cs)
            } else {
                cs
            };
            let c_end = if row_1 == sel.r1 {
                sel.c1.saturating_sub(1).min(ce)
            } else {
                ce
            };
            for c in c_start..c_end {
                if c < self.buf.width {
                    let cell = self.buf.get_mut(row, c);
                    if cell.flags.is_content() {
                        std::mem::swap(&mut cell.fg, &mut cell.bg);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::text::Span;
    use crate::tui::theme::palette;
    use smallvec::smallvec;

    fn test_region() -> Region {
        Region {
            row: 1,
            col: 3,
            width: 30,
            height: 5,
            bg: palette::BG,
            padding: Padding::horizontal(2),
        }
    }

    #[test]
    fn region_content_area() {
        let r = test_region();
        assert_eq!(r.content_width(), 26);
        assert_eq!(r.content_height(), 5);
    }

    #[test]
    fn define_and_set_lines() {
        let mut r = Renderer::new(40, 10);
        r.define("main", test_region());
        let lines = vec![Line::new(smallvec![Span::new("hello", palette::FG)])];
        r.set_lines("main", &lines);
        assert_eq!(r.slots[0].lines.len(), 1);
    }

    #[test]
    fn padding_creates_padding_cells() {
        let mut r = Renderer::new(20, 3);
        let region = Region {
            row: 1,
            col: 1,
            width: 20,
            height: 3,
            bg: palette::BG,
            padding: Padding::horizontal(2),
        };
        r.define("test", region);
        let lines = vec![Line::new(smallvec![Span::new("hi", palette::FG)])];
        r.set_lines("test", &lines);
        let reg = r.slots[0].region.clone();
        paint::paint_region(&mut r.buf, &reg, lines.iter());
        assert!(!r.buf.get(0, 0).flags.is_content());
        assert!(r.buf.get(0, 2).flags.is_content());
    }

    #[test]
    fn selection_swaps_colors() {
        let mut r = Renderer::new(20, 3);
        let region = Region {
            row: 1,
            col: 1,
            width: 20,
            height: 3,
            bg: palette::BG,
            padding: Padding::zero(),
        };
        r.define("main", region);
        let lines = vec![Line::new(smallvec![Span::new("hello", palette::FG)])];
        r.set_lines("main", &lines);
        let reg = r.slots[0].region.clone();
        paint::paint_region(&mut r.buf, &reg, lines.iter());
        let fg_before = r.buf.get(0, 0).fg;
        let bg_before = r.buf.get(0, 0).bg;
        r.apply_selection(SelectionRange {
            r0: 1,
            c0: 1,
            r1: 1,
            c1: 3,
        });
        assert_eq!(r.buf.get(0, 0).fg, bg_before);
        assert_eq!(r.buf.get(0, 0).bg, fg_before);
    }

    #[test]
    fn user_bg_extends_to_padding() {
        let mut r = Renderer::new(20, 3);
        let region = Region {
            row: 1,
            col: 1,
            width: 20,
            height: 3,
            bg: palette::BG,
            padding: Padding::horizontal(2),
        };
        r.define("test", region);
        let user_line = Line {
            spans: smallvec![Span::new("hi", palette::FG)],
            bg: Some(palette::USER_BG),
            margin: false,
            indent: 0,
            bleed: 0, deco: 0,
        };
        let reg = r.slots[0].region.clone();
        paint::paint_region(&mut r.buf, &reg, [user_line].iter());
        assert_eq!(r.buf.get(0, 0).bg, palette::USER_BG);
        assert_eq!(r.buf.get(0, 2).bg, palette::USER_BG);
    }
}
