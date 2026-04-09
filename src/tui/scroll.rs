//! Viewport scroll state — pure arithmetic, no knowledge of content.

/// Tracks scroll offset and whether the user manually scrolled.
#[derive(Debug)]
pub struct ScrollView {
    pub offset: usize,
    pub is_user_scrolled: bool,
}

impl ScrollView {
    /// Create at top position.
    pub fn new() -> Self {
        Self {
            offset: 0,
            is_user_scrolled: false,
        }
    }

    /// Scroll up by `n` lines. Always marks as user-scrolled.
    pub fn up(&mut self, n: usize) {
        self.offset = self.offset.saturating_sub(n);
        self.is_user_scrolled = true;
    }

    /// Scroll down by `n` lines within bounds. Clears user-scrolled if at bottom.
    pub fn down(&mut self, n: usize, max_scroll: usize) {
        self.offset = (self.offset + n).min(max_scroll);
        if self.offset >= max_scroll {
            self.is_user_scrolled = false;
        }
    }

    /// Jump to a specific offset. Sets user-scrolled if not at bottom.
    pub fn set_offset(&mut self, target: usize, max_scroll: usize) {
        self.offset = target.min(max_scroll);
        self.is_user_scrolled = self.offset < max_scroll;
    }

    /// Auto-scroll to bottom if user hasn't manually scrolled.
    pub fn auto_scroll(&mut self, total_lines: usize, view_height: usize) {
        if self.is_user_scrolled {
            return;
        }
        let overflow = total_lines.saturating_sub(view_height);
        if overflow > 0 {
            self.offset = overflow;
        }
    }

    /// Clamp offset after content shrinks. Clears user-scrolled if at bottom.
    pub fn clamp(&mut self, total_lines: usize, view_height: usize) {
        let max = total_lines.saturating_sub(view_height);
        if self.offset > max {
            self.offset = max;
        }
        if total_lines <= view_height || self.offset >= max {
            self.is_user_scrolled = false;
        }
    }

    /// Reset everything.
    pub fn reset(&mut self) {
        self.offset = 0;
        self.is_user_scrolled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let s = ScrollView::new();
        assert_eq!(s.offset, 0);
        assert!(!s.is_user_scrolled);
    }

    #[test]
    fn up_always_locks() {
        let mut s = ScrollView::new();
        s.offset = 50;
        s.up(3);
        assert_eq!(s.offset, 47);
        assert!(s.is_user_scrolled);
    }

    #[test]
    fn up_clamps_to_zero() {
        let mut s = ScrollView::new();
        s.offset = 2;
        s.up(10);
        assert_eq!(s.offset, 0);
        assert!(s.is_user_scrolled);
    }

    #[test]
    fn down_clears_user_scrolled_at_bottom() {
        let mut s = ScrollView::new();
        s.is_user_scrolled = true;
        s.down(100, 10);
        assert_eq!(s.offset, 10);
        assert!(!s.is_user_scrolled);
    }

    #[test]
    fn down_stays_locked_when_not_at_bottom() {
        let mut s = ScrollView::new();
        s.is_user_scrolled = true;
        s.offset = 0;
        s.down(5, 100);
        assert_eq!(s.offset, 5);
        assert!(s.is_user_scrolled);
    }

    #[test]
    fn auto_scroll_respects_user_scrolled() {
        let mut s = ScrollView::new();
        s.is_user_scrolled = true;
        s.auto_scroll(100, 20);
        assert_eq!(s.offset, 0); // didn't move
    }

    #[test]
    fn auto_scroll_follows_content() {
        let mut s = ScrollView::new();
        s.auto_scroll(100, 20);
        assert_eq!(s.offset, 80);
    }

    #[test]
    fn clamp_shrinks_offset() {
        let mut s = ScrollView::new();
        s.offset = 50;
        s.clamp(30, 20); // max = 10
        assert_eq!(s.offset, 10);
    }

    #[test]
    fn set_offset_sets_user_scrolled() {
        let mut s = ScrollView::new();
        s.set_offset(5, 10);
        assert!(s.is_user_scrolled);
        s.set_offset(10, 10);
        assert!(!s.is_user_scrolled); // at bottom
    }

    #[test]
    fn reset_clears_all() {
        let mut s = ScrollView::new();
        s.offset = 42;
        s.is_user_scrolled = true;
        s.reset();
        assert_eq!(s.offset, 0);
        assert!(!s.is_user_scrolled);
    }

    /// Streaming: user scrolls up, content keeps growing. Lock must hold.
    #[test]
    fn streaming_scroll_up_stays_locked() {
        let height = 20;
        let mut s = ScrollView::new();

        // Frame 1: auto-scroll to bottom
        s.auto_scroll(50, height);
        s.clamp(50, height);
        assert_eq!(s.offset, 30);
        assert!(!s.is_user_scrolled);

        // User scrolls up
        s.up(3);
        assert_eq!(s.offset, 27);
        assert!(s.is_user_scrolled);

        // Frames 2-4: content grows, lock holds
        for total in [60, 80, 200] {
            s.auto_scroll(total, height);
            s.clamp(total, height);
            assert_eq!(s.offset, 27);
            assert!(s.is_user_scrolled);
        }
    }

    /// Content shrinks below offset → clamp resets lock.
    #[test]
    fn clamp_after_content_shrink_resets_lock() {
        let mut s = ScrollView::new();
        s.auto_scroll(100, 20);
        s.up(30);
        assert_eq!(s.offset, 50);
        assert!(s.is_user_scrolled);

        s.clamp(25, 20); // max = 5
        assert_eq!(s.offset, 5);
        assert!(!s.is_user_scrolled);
    }

    /// Scroll up from auto-scroll bottom always locks.
    #[test]
    fn scroll_up_from_auto_scroll_locks() {
        let mut s = ScrollView::new();
        s.auto_scroll(50, 20);
        assert_eq!(s.offset, 30);
        s.up(3);
        assert!(s.is_user_scrolled);
    }
}
