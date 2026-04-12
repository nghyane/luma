/// View — composes Layout + ScrollView. Orchestrates render phase.
use crate::tui::block::Block;
use crate::tui::layout::Layout;
use crate::tui::layout::LayoutIter;
use crate::tui::scroll::ScrollView;
use crate::tui::text::Line;

pub struct ViewState {
    pub layout: Layout,
    pub scroll: ScrollView,
}

impl ViewState {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            layout: Layout::new(width, height),
            scroll: ScrollView::new(),
        }
    }

    /// Refresh layout and reconcile scroll offset. Call once per frame before reading.
    ///
    /// Auto-follows the bottom only when the user hasn't scrolled away; this is
    /// tracked inside `ScrollView` so every caller gets the same policy.
    pub fn prepare_frame(&mut self, blocks: &[Block]) {
        self.layout.refresh(blocks, self.scroll.offset);
        let total = self.layout.total_lines();
        let height = self.layout.height();
        self.scroll.auto_scroll(total, height);
        self.scroll.clamp(total, height);
    }

    /// Collect visible lines into a Vec (for overlay composite).
    pub fn collect_visible(&self) -> Vec<Line> {
        self.visible_iter().cloned().collect()
    }

    fn visible_iter(&self) -> LayoutIter<'_> {
        self.layout
            .window_iter(self.scroll.offset, self.layout.height())
    }

    pub fn tick(&mut self) {
        self.layout.tick();
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.layout.set_size(width, height);
    }

    /// Scroll up by `n` lines. Always marks as user-scrolled.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll.up(n);
    }

    /// Scroll down by `n` lines. Clears user-scrolled if at bottom.
    pub fn scroll_down(&mut self, n: usize) {
        let max = self
            .layout
            .total_lines()
            .saturating_sub(self.layout.height());
        self.scroll.down(n, max);
    }

    /// Jump to a specific offset.
    pub fn scroll_to(&mut self, offset: usize) {
        let max = self
            .layout
            .total_lines()
            .saturating_sub(self.layout.height());
        self.scroll.set_offset(offset, max);
    }

    /// (total_lines, view_height, offset).
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        (
            self.layout.total_lines(),
            self.layout.height(),
            self.scroll.offset,
        )
    }

    pub fn hit_test_block(&self, screen_row: usize, region_row: usize) -> Option<usize> {
        let abs = self.scroll.offset + screen_row.saturating_sub(region_row);
        self.layout.hit_test(abs)
    }

    pub fn clear(&mut self) {
        self.layout.clear();
        self.scroll.reset();
    }
}
