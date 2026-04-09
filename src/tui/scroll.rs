//! Viewport scroll state — pure arithmetic, no knowledge of content.

/// Tracks scroll offset and whether the user manually scrolled.
#[derive(Debug)]
pub struct ScrollView {
    pub offset: usize,
    pub is_user_scrolled: bool,
    /// True after scroll_down reached bottom. Used to detect trackpad
    /// bounce: a small scroll_up right after reaching bottom via down.
    just_hit_bottom: bool,
}

impl ScrollView {
    /// Create at top position.
    pub fn new() -> Self {
        Self {
            offset: 0,
            is_user_scrolled: false,
            just_hit_bottom: false,
        }
    }

    /// Scroll up by `n` lines. If we just arrived at bottom via scroll_down
    /// and this scroll is small (≤ threshold), treat as trackpad bounce.
    pub fn up(&mut self, n: usize, max_scroll: usize, threshold: usize) {
        self.offset = self.offset.saturating_sub(n);
        if self.just_hit_bottom && n <= threshold && self.offset + threshold >= max_scroll {
            // Trackpad bounce — stay in auto-scroll mode
            self.just_hit_bottom = false;
        } else {
            self.is_user_scrolled = true;
            self.just_hit_bottom = false;
        }
    }

    /// Scroll down by `n` lines within bounds. Clears user-scrolled if at bottom.
    pub fn down(&mut self, n: usize, max_scroll: usize) {
        self.offset = (self.offset + n).min(max_scroll);
        if self.offset >= max_scroll {
            self.is_user_scrolled = false;
            self.just_hit_bottom = true;
        }
    }

    /// Jump to a specific offset. Sets user-scrolled if not at bottom.
    pub fn set_offset(&mut self, target: usize, max_scroll: usize) {
        self.offset = target.min(max_scroll);
        self.is_user_scrolled = self.offset < max_scroll;
        self.just_hit_bottom = !self.is_user_scrolled;
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
        self.just_hit_bottom = false;
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
    fn up_marks_user_scrolled() {
        let mut s = ScrollView::new();
        s.offset = 50;
        // Not just_hit_bottom → always marks user_scrolled
        s.up(3, 100, 3);
        assert_eq!(s.offset, 47);
        assert!(s.is_user_scrolled);
    }

    #[test]
    fn up_from_auto_scroll_bottom_marks_user_scrolled() {
        // At bottom via auto_scroll (not via scroll_down)
        // → just_hit_bottom is false → scroll_up IS intentional
        let mut s = ScrollView::new();
        s.auto_scroll(100, 20); // offset = 80, at bottom
        assert_eq!(s.offset, 80);
        assert!(!s.is_user_scrolled);
        assert!(!s.just_hit_bottom); // auto_scroll doesn't set this

        s.up(3, 80, 3);
        assert_eq!(s.offset, 77);
        assert!(
            s.is_user_scrolled,
            "scroll up from auto-scroll bottom should lock"
        );
    }

    #[test]
    fn up_clamps_to_zero() {
        let mut s = ScrollView::new();
        s.offset = 2;
        s.up(10, 100, 3);
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn down_clears_user_scrolled_at_bottom() {
        let mut s = ScrollView::new();
        s.is_user_scrolled = true;
        s.down(100, 10);
        assert_eq!(s.offset, 10);
        assert!(!s.is_user_scrolled);
        assert!(s.just_hit_bottom);
    }

    #[test]
    fn bounce_after_scroll_down_to_bottom() {
        let mut s = ScrollView::new();
        s.is_user_scrolled = true;
        s.offset = 70;
        // Scroll down to bottom
        s.down(100, 80);
        assert_eq!(s.offset, 80);
        assert!(!s.is_user_scrolled);
        assert!(s.just_hit_bottom);

        // Trackpad bounce: small scroll up
        s.up(3, 80, 3);
        assert_eq!(s.offset, 77);
        assert!(!s.is_user_scrolled, "bounce should not break auto-scroll");
        assert!(!s.just_hit_bottom, "bounce flag consumed");

        // Second scroll up: no longer bounce
        s.up(3, 80, 3);
        assert_eq!(s.offset, 74);
        assert!(s.is_user_scrolled, "second scroll up is intentional");
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
        s.just_hit_bottom = true;
        s.reset();
        assert_eq!(s.offset, 0);
        assert!(!s.is_user_scrolled);
        assert!(!s.just_hit_bottom);
    }

    /// Simulate: streaming grows content, user scrolls up, content keeps growing.
    /// Scroll lock must hold across multiple prepare_frame cycles.
    #[test]
    fn streaming_scroll_up_stays_locked() {
        let height = 20;
        let mut s = ScrollView::new();

        // Frame 1: content = 50 lines, auto-scroll to bottom
        s.auto_scroll(50, height);
        s.clamp(50, height);
        assert_eq!(s.offset, 30, "frame 1: at bottom");
        assert!(!s.is_user_scrolled);

        // User scrolls up 3 lines (between frames)
        let max = 50usize.saturating_sub(height); // 30 — stale cached_total
        s.up(3, max, 3);
        assert_eq!(s.offset, 27);
        assert!(s.is_user_scrolled, "scroll up must lock");

        // Frame 2: content grew to 60 lines during streaming
        s.auto_scroll(60, height);
        s.clamp(60, height);
        assert_eq!(s.offset, 27, "frame 2: offset unchanged");
        assert!(s.is_user_scrolled, "frame 2: still locked");

        // Frame 3: content grew to 80 lines
        s.auto_scroll(80, height);
        s.clamp(80, height);
        assert_eq!(s.offset, 27, "frame 3: offset unchanged");
        assert!(s.is_user_scrolled, "frame 3: still locked");

        // Frame 4: content grew to 200 lines
        s.auto_scroll(200, height);
        s.clamp(200, height);
        assert_eq!(s.offset, 27, "frame 4: offset unchanged");
        assert!(s.is_user_scrolled, "frame 4: still locked");
    }

    /// Simulate: user scrolls up with stale max, then content shrinks below offset.
    #[test]
    fn clamp_after_content_shrink_resets_lock() {
        let height = 20;
        let mut s = ScrollView::new();

        // At bottom with 100 lines
        s.auto_scroll(100, height);
        assert_eq!(s.offset, 80);

        // User scrolls up
        s.up(30, 80, 3);
        assert_eq!(s.offset, 50);
        assert!(s.is_user_scrolled);

        // Content shrinks to 25 lines (e.g. /new command)
        s.clamp(25, height);
        // max = 5, offset clamped to 5, 5 >= 5 → is_user_scrolled = false
        assert_eq!(s.offset, 5);
        assert!(!s.is_user_scrolled, "at max after shrink → unlock");
    }
}
