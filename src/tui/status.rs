/// Status bar — mode, model, thinking level, context usage, spinner.
use crate::tui::text::{Line, Span};
use crate::tui::theme::{Rgb, icon, palette};
use smallvec::{SmallVec, smallvec};

/// Status bar display state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Ready,
    Thinking,
}

/// Current model identity for display.
struct ModelInfo {
    mode: String,
    mode_color: Rgb,
    model: String,
    provider: String,
}

/// Cumulative token usage for status display.
struct UsageStats {
    context_tokens: u64,
    context_pct: u8,
    cache_read: u64,
    cache_write: u64,
}

/// Health summary of the auth account pool. Only surfaced in the status
/// bar when something actually needs the user's attention — when every
/// account is healthy we render nothing (no clutter).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolHealth {
    pub cooling: u8,
    pub needs_relogin: u8,
}

impl PoolHealth {
    fn is_clean(&self) -> bool {
        self.cooling == 0 && self.needs_relogin == 0
    }
}

/// Status bar model.
pub struct StatusBar {
    state: StatusState,
    info: ModelInfo,
    usage: UsageStats,
    thinking_level: String,
    fast_mode: bool,
    spinner_idx: usize,
    pool: PoolHealth,
}

impl StatusBar {
    /// Create with defaults.
    pub fn new() -> Self {
        Self {
            state: StatusState::Ready,
            info: ModelInfo {
                mode: String::new(),
                mode_color: palette::ACCENT,
                model: String::new(),
                provider: String::new(),
            },
            usage: UsageStats {
                context_tokens: 0,
                context_pct: 0,
                cache_read: 0,
                cache_write: 0,
            },
            thinking_level: String::new(),
            fast_mode: false,
            spinner_idx: 0,
            pool: PoolHealth::default(),
        }
    }

    /// Update the pool-health summary. Rendered only when non-clean.
    pub fn set_pool_health(&mut self, health: PoolHealth) {
        self.pool = health;
    }

    /// Set the active mode name and color.
    pub fn set_mode(&mut self, mode: &str, color: Rgb) {
        self.info.mode = mode.to_owned();
        self.info.mode_color = color;
    }

    /// Set the current model name.
    pub fn set_model(&mut self, model: &str) {
        self.info.model = model.to_owned();
    }

    /// Set the provider display name.
    pub fn set_provider(&mut self, provider: &str) {
        self.info.provider = provider.to_owned();
    }

    /// Set thinking level label.
    pub fn set_thinking_level(&mut self, level: &str) {
        self.thinking_level = level.to_owned();
    }

    /// Set whether the compact fast-mode chip is visible.
    pub fn set_fast_mode(&mut self, enabled: bool) {
        self.fast_mode = enabled;
    }

    /// Set context window usage — tokens and percentage.
    pub fn set_context(&mut self, tokens: u64, pct: u8) {
        self.usage.context_tokens = tokens;
        self.usage.context_pct = pct.min(100);
    }

    /// Set cache usage from the latest provider response (replaces previous).
    pub fn set_cache(&mut self, read: u64, write: u64) {
        self.usage.cache_read = read;
        self.usage.cache_write = write;
    }

    /// Reset all usage counters — context tokens and cache — for a new thread.
    pub fn reset_usage(&mut self) {
        self.usage = UsageStats {
            context_tokens: 0,
            context_pct: 0,
            cache_read: 0,
            cache_write: 0,
        };
    }

    /// Current cache values for fallback when provider omits them.
    pub fn cache_values(&self) -> (u64, u64) {
        (self.usage.cache_read, self.usage.cache_write)
    }

    /// Set ready or thinking state.
    pub fn set_state(&mut self, state: StatusState) {
        self.state = state;
        if state == StatusState::Ready {
            self.spinner_idx = 0;
        }
    }

    /// Advance spinner frame.
    pub fn tick(&mut self) {
        if self.state == StatusState::Thinking {
            self.spinner_idx = (self.spinner_idx + 1) % icon::SPINNER.len();
        }
    }

    /// Mode/model line — rendered inside the input block (bar added by caller).
    pub fn mode_line(&self) -> Line {
        let mut spans = smallvec![
            Span::bold(self.info.mode.clone(), self.info.mode_color),
            Span::new(format!("  {}", self.info.model), palette::DIM),
        ];

        if !self.info.provider.is_empty() {
            spans.push(Span::new(
                format!(" {}", self.info.provider),
                palette::MUTED,
            ));
        }

        if !self.thinking_level.is_empty() && self.thinking_level != "off" {
            spans.push(Span::new(" · ".to_owned(), palette::MUTED));
            spans.push(Span::new(self.thinking_level.clone(), palette::WARN));
        }
        if self.fast_mode {
            spans.push(Span::new(" · ".to_owned(), palette::MUTED));
            spans.push(Span::new("fast".to_owned(), palette::ACCENT));
        }

        Line::new(spans)
    }

    /// Hint bar — left: spinner+esc, right: usage+shortcuts. Padded to width.
    pub fn hint_line(&self, width: usize) -> Line {
        // Left side: esc interrupt (when thinking)
        let mut left: SmallVec<[Span; 4]> = smallvec![];
        if self.state == StatusState::Thinking {
            let frame = icon::SPINNER[self.spinner_idx % icon::SPINNER.len()];
            let char_w = crate::tui::text::display_width(frame);
            let pad = icon::SPINNER_WIDTH.saturating_sub(char_w);
            left.push(Span::new(
                format!("{frame}{} ", " ".repeat(pad)),
                palette::DIM,
            ));
            left.push(Span::new("esc ".to_owned(), palette::DIM));
            left.push(Span::new("interrupt".to_owned(), palette::MUTED));
        }

        // Right side: usage (always) + cache + pool-health (only when dirty)
        let mut right: SmallVec<[Span; 4]> = smallvec![];
        if !self.pool.is_clean() {
            let label = format_pool_health(&self.pool);
            let color = if self.pool.needs_relogin > 0 {
                palette::ERROR
            } else {
                palette::WARN
            };
            right.push(Span::new(label, color));
            right.push(Span::new("  ".to_owned(), palette::DIM));
        }
        let color = match self.usage.context_pct {
            81..=100 => palette::ERROR,
            51..=80 => palette::WARN,
            _ => palette::DIM,
        };
        let tokens = compact_tokens(self.usage.context_tokens);
        right.push(Span::new(
            format!("{tokens} ({}%)", self.usage.context_pct),
            color,
        ));
        if self.usage.cache_read > 0 || self.usage.cache_write > 0 {
            right.push(Span::new("  ".to_owned(), palette::DIM));
            let label = format_cache(self.usage.cache_read, self.usage.cache_write);
            right.push(Span::new(label, palette::DIM));
        }

        // Pad middle to push right content to the right edge
        let left_w: usize = left
            .iter()
            .map(|s: &Span| crate::tui::text::display_width(&s.text))
            .sum();
        let right_w: usize = right
            .iter()
            .map(|s: &Span| crate::tui::text::display_width(&s.text))
            .sum();
        let pad = width.saturating_sub(left_w + right_w);

        let mut spans = left;
        if pad > 0 {
            spans.push(Span::new(" ".repeat(pad), palette::DIM));
        }
        spans.extend(right);
        Line::new(spans)
    }
}

/// Format token count compactly: 1500 → "1.5K", 1500000 → "1.5M".
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format the pool-health label shown in the status bar. Called only when
/// there's something to show (see `PoolHealth::is_clean`).
fn format_pool_health(h: &PoolHealth) -> String {
    match (h.cooling, h.needs_relogin) {
        (0, 0) => String::new(),
        (c, 0) => format!("⚠ {c} cooling"),
        (0, r) => format!("⚠ {r} re-login"),
        (c, r) => format!("⚠ {c} cooling · {r} re-login"),
    }
}

/// Format cache tokens compactly: "cache ⚡237K ↑49K".
fn format_cache(read: u64, write: u64) -> String {
    match (read > 0, write > 0) {
        (true, true) => format!(
            "cache ⚡{} ↑{}",
            compact_tokens(read),
            compact_tokens(write)
        ),
        (true, false) => format!("cache ⚡{}", compact_tokens(read)),
        (false, true) => format!("cache ↑{}", compact_tokens(write)),
        (false, false) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let sb = StatusBar::new();
        assert_eq!(sb.state, StatusState::Ready);
        assert_eq!(sb.usage.context_pct, 0);
    }

    #[test]
    fn line_ready() {
        let mut sb = StatusBar::new();
        sb.set_mode("smart", palette::MODE_SMART);
        sb.set_model("claude-4");
        let l = sb.mode_line();
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("smart"));
        assert!(text.contains("claude-4"));
        assert!(!text.contains("thinking"));
    }

    #[test]
    fn mode_line_shows_model_in_thinking_state() {
        let mut sb = StatusBar::new();
        sb.set_mode("deep", palette::MODE_DEEP);
        sb.set_model("o3");
        sb.set_state(StatusState::Thinking);
        let l = sb.mode_line();
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("deep"));
        assert!(text.contains("o3"));
    }

    #[test]
    fn mode_line_shows_fast_only_when_enabled() {
        let mut sb = StatusBar::new();
        sb.set_mode("deep", palette::MODE_DEEP);
        sb.set_model("gpt-5.4");
        let l = sb.mode_line();
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!text.contains("fast"));

        sb.set_fast_mode(true);
        let l = sb.mode_line();
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("fast"));
    }

    #[test]
    fn hint_line_thinking_shows_interrupt() {
        let mut sb = StatusBar::new();
        sb.set_state(StatusState::Thinking);
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("interrupt"));
    }

    #[test]
    fn hint_line_context_coloring() {
        let mut sb = StatusBar::new();
        sb.set_context(180_000, 90);
        let l = sb.hint_line(80);
        let ctx_span = l.spans.iter().find(|s| s.text.contains("180.0K"));
        assert!(ctx_span.is_some());
        assert_eq!(ctx_span.unwrap().fg, palette::ERROR);
    }

    #[test]
    fn tick_advances_spinner() {
        let mut sb = StatusBar::new();
        sb.set_state(StatusState::Thinking);
        assert_eq!(sb.spinner_idx, 0);
        sb.tick();
        assert_eq!(sb.spinner_idx, 1);
    }

    #[test]
    fn tick_noop_when_ready() {
        let mut sb = StatusBar::new();
        sb.tick();
        assert_eq!(sb.spinner_idx, 0);
    }

    #[test]
    fn hint_line_cache_display() {
        let mut sb = StatusBar::new();
        sb.set_cache(1200, 0);
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("cache ⚡1.2K"));
    }

    #[test]
    fn hint_line_cache_read_and_write() {
        let mut sb = StatusBar::new();
        sb.set_cache(5000, 2000);
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("cache ⚡5.0K ↑2.0K"));
    }

    #[test]
    fn reset_usage_clears_cache_and_context() {
        let mut sb = StatusBar::new();
        sb.set_cache(1000, 500);
        sb.set_context(120_000, 60);
        sb.reset_usage();
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!text.contains("cache"));
        assert!(text.contains("0 (0%)"));
    }

    #[test]
    fn format_cache_compact() {
        assert_eq!(super::format_cache(1_500_000, 0), "cache ⚡1.5M");
        assert_eq!(super::format_cache(500, 0), "cache ⚡500");
        assert_eq!(super::format_cache(0, 3000), "cache ↑3.0K");
    }

    #[test]
    fn pool_health_clean_by_default() {
        assert!(PoolHealth::default().is_clean());
    }

    #[test]
    fn pool_health_cooling_is_not_clean() {
        assert!(
            !PoolHealth {
                cooling: 1,
                needs_relogin: 0,
            }
            .is_clean()
        );
    }

    #[test]
    fn format_pool_health_variants() {
        assert_eq!(super::format_pool_health(&PoolHealth::default()), "");
        assert_eq!(
            super::format_pool_health(&PoolHealth {
                cooling: 2,
                needs_relogin: 0,
            }),
            "⚠ 2 cooling"
        );
        assert_eq!(
            super::format_pool_health(&PoolHealth {
                cooling: 0,
                needs_relogin: 1,
            }),
            "⚠ 1 re-login"
        );
        assert_eq!(
            super::format_pool_health(&PoolHealth {
                cooling: 1,
                needs_relogin: 2,
            }),
            "⚠ 1 cooling · 2 re-login"
        );
    }

    #[test]
    fn hint_line_hides_pool_when_clean() {
        let sb = StatusBar::new();
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!text.contains("cooling"));
        assert!(!text.contains("re-login"));
    }

    #[test]
    fn hint_line_shows_pool_when_dirty() {
        let mut sb = StatusBar::new();
        sb.set_pool_health(PoolHealth {
            cooling: 1,
            needs_relogin: 0,
        });
        let l = sb.hint_line(80);
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("1 cooling"));
    }
}
