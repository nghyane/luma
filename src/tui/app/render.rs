/// App rendering — mouse handling, screen composition, scrollbar.
use super::state::{DragState, RunState};
use super::{Action, SCROLL_STEP};
use crate::tui::renderer::{CursorState, Overlay};
use crate::tui::selection;
use crate::tui::text::{Line, Span};
use crate::tui::theme::{icon, palette};
use smallvec::smallvec;
use termina::event::{MouseButton, MouseEvent, MouseEventKind};

impl super::App {
    pub(super) fn on_mouse(&mut self, ev: MouseEvent) -> Action {
        let r_row = self.regions.output.row;
        let r_height = self.regions.output.height;
        let r_width = self.regions.output.width;
        let in_output = |row: u16| row >= r_row && row < r_row + r_height;
        let i_row = self.regions.input.row;
        let i_height = self.regions.input.height;
        let in_input = |row: u16| row >= i_row && row < i_row + i_height;
        let (total, visible, _) = self.view.scroll_info();
        let has_sb = total > visible;
        let sb_col = self.regions.output.col + r_width - 1;

        let row = ev.row + 1;
        let col = ev.column + 1;

        match ev.kind {
            MouseEventKind::ScrollUp => {
                self.view.scroll_up(SCROLL_STEP);
                Action::Render
            }
            MouseEventKind::ScrollDown => {
                self.view.scroll_down(SCROLL_STEP);
                Action::Render
            }
            MouseEventKind::Down(MouseButton::Left) if in_output(row) || in_input(row) => {
                if in_output(row) && has_sb && col >= sb_col {
                    let (_, _, offset) = self.view.scroll_info();
                    self.ui.drag = Some(DragState::Scrollbar {
                        start_row: row,
                        start_offset: offset,
                    });
                } else {
                    self.ui.selection.begin(row, col);
                    self.ui.drag = Some(DragState::Selecting);
                }
                Action::Continue
            }
            MouseEventKind::Drag(MouseButton::Left) => match &self.ui.drag {
                Some(DragState::Scrollbar {
                    start_row,
                    start_offset,
                }) if has_sb => {
                    let start_row = *start_row;
                    let start_offset = *start_offset;
                    let delta = i32::from(row) - i32::from(start_row);
                    let max_off = total.saturating_sub(visible);
                    let thumb_h = (visible * visible / total).max(1);
                    let track_h = visible.saturating_sub(thumb_h);
                    if track_h > 0 {
                        let sd =
                            (f64::from(delta) / track_h as f64 * max_off as f64).round() as isize;
                        self.view
                            .scroll_to((start_offset as isize + sd).max(0) as usize);
                    }
                    Action::Render
                }
                Some(DragState::Selecting) => {
                    self.ui.selection.update(row, col);
                    self.ui
                        .selection
                        .edge_scroll(&mut self.view, r_row, r_height);
                    Action::Render
                }
                _ => Action::Continue,
            },
            MouseEventKind::Up(MouseButton::Left) => {
                let was_selecting = matches!(self.ui.drag, Some(DragState::Selecting));
                self.ui.drag = None;

                if was_selecting {
                    if let Some((r0, c0, r1, c1)) = self.ui.selection.finish() {
                        selection::copy_from_buffer(self.renderer.buffer(), r0, c0, r1, c1);
                        return Action::Render;
                    } else if in_output(row) {
                        let rr = self.regions.output.row as usize;
                        if let Some(idx) = self.view.hit_test_block(row as usize, rr)
                            && self.doc.toggle_expand(idx)
                        {
                            return Action::Render;
                        }
                    }
                }
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    pub(super) fn handle_resize(&mut self, w: u16, h: u16) {
        self.regions = super::compute_regions(w, h);
        self.renderer.set_term_size(w, h);
        self.renderer
            .update_region("output", self.regions.output.clone());
        self.renderer
            .update_region("status", self.regions.status.clone());
        self.renderer
            .update_region("input", self.regions.input.clone());
        self.view.set_size(
            self.regions.output.content_width() as usize,
            self.regions.output.content_height() as usize,
        );
        self.renderer.clear_screen();
    }

    pub(super) fn render(&mut self) {
        self.reconcile_input_height();
        match &self.screen {
            super::state::Screen::Welcome { lines } => {
                self.renderer.set_overlay(None);
                self.renderer.set_bottom_padding("output", 0);
                self.renderer.set_lines("output", lines);
            }
            super::state::Screen::Chat => {
                self.view.prepare_frame(self.doc.blocks());
                self.reconcile_scrollbar_width();
                self.renderer
                    .set_bottom_padding("output", self.regions.output.padding.bottom);
                let vis = self.view.collect_visible();
                self.renderer.set_lines("output", &vis);
                self.update_scrollbar();
                self.update_selection_highlight();
            }
        }
        self.set_floating_layers();
        self.render_status();
        self.renderer.set_lines("input", &self.build_input_lines());
        self.update_cursor();
        let _ = self.renderer.flush();
    }

    /// Grow or shrink the input region based on wrapped prompt line count.
    fn reconcile_input_height(&mut self) {
        let (term_w, term_h) = (
            self.regions.output.width + super::OUTER_MARGIN * 2,
            self.regions.output.height + self.regions.input.height + self.regions.status.height,
        );
        let bar_w = crate::tui::text::display_width(&format!("{}  ", icon::PROMPT));
        let content_w = (self.regions.input.width as usize).saturating_sub(bar_w);
        let wrapped = self.prompt_wrapped_count(content_w);
        let max_input = (term_h / 5 * 2).max(super::MIN_INPUT_HEIGHT);
        let desired =
            (wrapped as u16 + super::INPUT_CHROME).clamp(super::MIN_INPUT_HEIGHT, max_input);
        if desired == self.regions.input.height {
            return;
        }
        self.regions = super::compute_regions_with_input(term_w, term_h, desired);
        self.renderer
            .update_region("output", self.regions.output.clone());
        self.renderer
            .update_region("status", self.regions.status.clone());
        self.renderer
            .update_region("input", self.regions.input.clone());
        self.view.set_size(
            self.regions.output.content_width() as usize,
            self.regions.output.content_height() as usize,
        );
    }

    /// Count how many visual lines the prompt wraps to at `content_w`.
    fn prompt_wrapped_count(&self, content_w: usize) -> usize {
        use crate::tui::text::wrap_line;
        let raw = self.ui.prompt.lines();
        raw.iter()
            .map(|pl| wrap_line(pl, content_w, None).len())
            .sum::<usize>()
            .max(1)
    }

    fn reconcile_scrollbar_width(&mut self) {
        let content_w = self.regions.output.content_width();
        let content_h = self.regions.output.content_height();
        let (total, visible, _) = self.view.scroll_info();
        let ow = if total > visible {
            content_w - 1
        } else {
            content_w
        };
        if ow != self.ui.last_output_width {
            self.view.set_size(ow as usize, content_h as usize);
            self.ui.last_output_width = ow;
            self.view.prepare_frame(self.doc.blocks());
        }
    }

    /// Set floating layers — dialog (centered), picker and dropdown (bottom-anchored).
    fn set_floating_layers(&mut self) {
        use crate::tui::renderer::FloatingLayer;

        // Centered dialog takes priority over picker/dropdown.
        if self.ui.dialog.is_active {
            let term_w = self.regions.output.width + super::OUTER_MARGIN * 2;
            let term_h =
                self.regions.output.height + self.regions.input.height + self.regions.status.height;
            let lines = self.ui.dialog.lines(term_w);
            let (row, col) = self.ui.dialog.position(term_w, term_h);
            // box_w must match what dialog.lines() computed internally.
            let box_w = (term_w as usize).clamp(44, 68) as u16;
            self.renderer.set_floating(vec![FloatingLayer {
                row,
                col,
                width: box_w,
                lines,
                bg: crate::tui::theme::palette::BG,
            }]);
            return;
        }

        let content_h = self.regions.output.content_height() as usize;
        let dropdown = self.ui.prompt.dropdown();
        let picker_lines = self.ui.picker.lines(content_h);

        let lines = if !picker_lines.is_empty() {
            picker_lines
        } else if !dropdown.is_empty() {
            dropdown
        } else {
            self.renderer.set_floating(Vec::new());
            return;
        };

        let r = &self.regions.output;
        let count = lines.len().min(r.height as usize);
        let row = r.row + r.height - count as u16;

        self.renderer.set_floating(vec![FloatingLayer {
            row,
            col: r.col,
            width: r.width,
            lines,
            bg: crate::tui::theme::palette::BG,
        }]);
    }

    fn render_status(&mut self) {
        let hint_w = self.regions.status.content_width() as usize;
        let status_line = if self.agent.state == RunState::PendingAbort {
            Line::new(smallvec![
                Span::new("esc", palette::WARN),
                Span::new(" again to interrupt", palette::DIM),
            ])
        } else {
            self.ui.status.hint_line(hint_w)
        };
        self.renderer.set_lines("status", &[status_line]);
    }

    fn build_input_lines(&self) -> Vec<Line> {
        use crate::tui::text::wrap_line;

        let bar = icon::PROMPT;
        let bar_empty = Line::new(smallvec![Span::deco(bar.to_owned(), palette::ACCENT)]);
        let total_h = self.regions.input.height as usize;
        let bar_prefix = format!("{bar}  ");
        let bar_w = crate::tui::text::display_width(&bar_prefix);
        let content_w = (self.regions.input.width as usize).saturating_sub(bar_w);

        // Wrap prompt lines to fit the available width.
        let raw_prompt = self.ui.prompt.lines();
        let mut wrapped: Vec<Line> = Vec::new();
        for pl in &raw_prompt {
            wrapped.extend(wrap_line(pl, content_w, None));
        }

        // Available rows for prompt content (between top bar and mode + bottom border).
        let content_slots = total_h.saturating_sub(3);

        // Find which wrapped line the cursor sits on by walking line widths.
        let cursor_col = self.ui.prompt.cursor_column();
        let (cursor_wrap_row, _) = cursor_position_in_wrapped(&wrapped, cursor_col);

        let scroll = if wrapped.len() > content_slots {
            cursor_wrap_row.saturating_sub(content_slots.saturating_sub(1))
        } else {
            0
        };
        let visible: Vec<&Line> = wrapped.iter().skip(scroll).take(content_slots).collect();

        let mut lines = Vec::with_capacity(total_h);
        // Top bar — show scroll indicator when content is scrolled.
        if scroll > 0 {
            lines.push(Line::new(smallvec![
                Span::deco(bar.to_owned(), palette::ACCENT),
                Span::new(format!(" {scroll} more"), palette::DIM),
            ]));
        } else {
            lines.push(bar_empty.clone());
        }
        for vl in &visible {
            let mut spans = smallvec![Span::deco(bar_prefix.clone(), palette::ACCENT)];
            spans.extend(vl.spans.iter().cloned());
            lines.push(Line::new(spans));
        }

        let mut mode_spans = smallvec![Span::deco(bar_prefix.clone(), palette::ACCENT)];
        mode_spans.extend(self.ui.status.mode_line().spans.iter().cloned());
        let mode = Line::new(mode_spans);

        for _ in lines.len()..total_h.saturating_sub(2) {
            lines.push(bar_empty.clone());
        }
        lines.push(mode);
        lines.push(Line::new(smallvec![
            Span::deco_colored("╹".to_owned(), palette::ACCENT, palette::BG),
            Span::deco_colored(
                "▀".repeat((self.regions.input.width as usize).saturating_sub(1)),
                palette::SURFACE,
                palette::BG,
            ),
        ]));
        lines
    }

    fn update_cursor(&mut self) {
        use crate::tui::text::wrap_line;

        let ir = &self.regions.input;
        let bar_w = 3u16; // "┃  "
        let content_w = ir.width.saturating_sub(bar_w) as usize;
        let cursor_col_abs = self.ui.prompt.cursor_column();

        // Wrap lines identically to build_input_lines.
        let raw_prompt = self.ui.prompt.lines();
        let mut wrapped: Vec<Line> = Vec::new();
        for pl in &raw_prompt {
            wrapped.extend(wrap_line(pl, content_w, None));
        }

        let (wrap_row, wrap_col) = cursor_position_in_wrapped(&wrapped, cursor_col_abs);

        // Scroll offset mirrors build_input_lines.
        let total_h = ir.height as usize;
        let content_slots = total_h.saturating_sub(3);
        let scroll = if wrapped.len() > content_slots {
            wrap_row.saturating_sub(content_slots.saturating_sub(1))
        } else {
            0
        };
        let visible_row = wrap_row - scroll;

        let cursor_row = ir.row + 1 + visible_row as u16;
        let cursor_col = ir.col + bar_w + wrap_col as u16;
        if self.agent.state == RunState::PendingAbort {
            self.renderer.set_cursor(CursorState::Hidden);
        } else {
            self.renderer.set_cursor(CursorState::Visible {
                row: cursor_row,
                col: cursor_col,
            });
        }
    }

    fn update_selection_highlight(&mut self) {
        use crate::tui::renderer::SelectionRange;
        if self.ui.selection.is_active && self.ui.selection.has_range() {
            let (mut r0, mut c0, mut r1, mut c1) = (
                self.ui.selection.start_row,
                self.ui.selection.start_col,
                self.ui.selection.end_row,
                self.ui.selection.end_col,
            );
            if r0 > r1 || (r0 == r1 && c0 > c1) {
                std::mem::swap(&mut r0, &mut r1);
                std::mem::swap(&mut c0, &mut c1);
            }
            self.renderer
                .set_selection(Some(SelectionRange { r0, c0, r1, c1 }));
        } else {
            self.renderer.set_selection(None);
        }
    }

    fn update_scrollbar(&mut self) {
        use crate::tui::renderer::ScrollCell;

        let (total, visible, offset) = self.view.scroll_info();
        if total <= visible {
            self.renderer.set_overlay(None);
            return;
        }
        let r = &self.regions.output;
        let track = visible;

        const SUB: usize = 8;
        let track_sub = track * SUB;
        let thumb_sub = (track_sub * visible / total).max(SUB);
        let max_off = total.saturating_sub(visible);
        let scroll_sub = track_sub.saturating_sub(thumb_sub);
        let start_sub = (offset * scroll_sub).checked_div(max_off).unwrap_or(0);
        let end_sub = start_sub + thumb_sub;

        let mut cells = Vec::with_capacity(track);
        for i in 0..track {
            let cell_start = i * SUB;
            let cell_end = cell_start + SUB;
            if cell_end <= start_sub || cell_start >= end_sub {
                cells.push(ScrollCell::Track);
            } else if cell_start >= start_sub && cell_end <= end_sub {
                cells.push(ScrollCell::Thumb);
            } else if cell_start < start_sub && cell_end > start_sub {
                let frac = (start_sub - cell_start) as u8;
                if frac == 0 {
                    cells.push(ScrollCell::Thumb);
                } else {
                    cells.push(ScrollCell::TopEdge(frac));
                }
            } else {
                let frac = (end_sub - cell_start) as u8;
                if frac >= SUB as u8 {
                    cells.push(ScrollCell::Thumb);
                } else {
                    cells.push(ScrollCell::BottomEdge(frac));
                }
            }
        }

        self.renderer.set_overlay(Some(Overlay {
            row: r.row,
            col: r.col + r.width - 1,
            fg_thumb: palette::DIM,
            fg_track: palette::BORDER,
            cells,
        }));
    }
}

/// Find (row, col) of cursor within wrapped lines by walking visible widths.
fn cursor_position_in_wrapped(wrapped: &[Line], cursor_col: usize) -> (usize, usize) {
    let mut remaining = cursor_col;
    for (i, line) in wrapped.iter().enumerate() {
        let w = line.visible_width();
        if remaining <= w && (i + 1 == wrapped.len() || remaining < w) {
            return (i, remaining);
        }
        remaining = remaining.saturating_sub(w);
    }
    // Cursor past all content — place at end of last line.
    let last = wrapped.len().saturating_sub(1);
    let last_w = wrapped.last().map(|l| l.visible_width()).unwrap_or(0);
    (last, last_w)
}
