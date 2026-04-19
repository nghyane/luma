/// Text primitives: Span, Line, Padding, char_width, display_width.
mod buffer;
mod cell;
mod wrap;

pub use buffer::ScreenBuffer;
pub use wrap::wrap_line;

use crate::tui::theme::Rgb;
use smallvec::SmallVec;

/// Per-cell metadata flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellFlag(pub u8);

impl std::ops::BitOrAssign<u8> for CellFlag {
    fn bitor_assign(&mut self, rhs: u8) {
        self.0 |= rhs;
    }
}

impl CellFlag {
    pub const NONE: u8 = 0;
    pub const BOLD: u8 = 1;
    pub const PADDING: u8 = 2;
    pub const DECORATION: u8 = 4;
    pub const ITALIC: u8 = 8;
    /// Trailing cell of a wide character — skipped during ANSI render.
    pub const WIDE_PAD: u8 = 16;
    /// Whether this cell is content (not padding or decoration).
    pub fn is_content(self) -> bool {
        self.0 & (Self::PADDING | Self::DECORATION) == 0
    }
    /// Whether this cell is the trailing half of a wide character.
    pub fn is_wide_pad(self) -> bool {
        self.0 & Self::WIDE_PAD != 0
    }
    /// Whether this cell is bold.
    pub fn is_bold(self) -> bool {
        self.0 & Self::BOLD != 0
    }
    /// Whether this cell is italic.
    pub fn is_italic(self) -> bool {
        self.0 & Self::ITALIC != 0
    }
}

/// A styled text segment.
#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub decoration: bool,
}

impl Span {
    /// Create a plain span.
    pub fn new(text: impl Into<String>, fg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: None,
            bold: false,
            italic: false,
            decoration: false,
        }
    }
    /// Create a decoration span (skipped during copy).
    pub fn deco(text: impl Into<String>, fg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: None,
            bold: false,
            italic: false,
            decoration: true,
        }
    }
    /// Create a decoration span with bg (skipped during copy).
    pub fn deco_colored(text: impl Into<String>, fg: Rgb, bg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: Some(bg),
            bold: false,
            italic: false,
            decoration: true,
        }
    }
    /// Create a span with background color.
    pub fn with_bg(text: impl Into<String>, fg: Rgb, bg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: Some(bg),
            bold: false,
            italic: false,
            decoration: false,
        }
    }
    /// Create a bold span.
    pub fn bold(text: impl Into<String>, fg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: None,
            bold: true,
            italic: false,
            decoration: false,
        }
    }
    /// Create an italic span.
    pub fn italic(text: impl Into<String>, fg: Rgb) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: None,
            bold: false,
            italic: true,
            decoration: false,
        }
    }
}

/// A single rendered line with optional background.
#[derive(Debug, Clone)]
pub struct Line {
    pub spans: SmallVec<[Span; 4]>,
    pub bg: Option<Rgb>,
    pub margin: bool,
    pub indent: u16,
    /// Bleed into left padding by this many cells (for accent bars).
    pub bleed: u16,
    /// Number of leading characters that are decoration (line numbers, markers).
    /// These are flagged DECORATION so they are excluded from text selection.
    pub deco: u16,
}

impl Line {
    /// Create a line from spans.
    pub fn new(spans: impl Into<SmallVec<[Span; 4]>>) -> Self {
        Self {
            spans: spans.into(),
            bg: None,
            margin: false,
            indent: 0,
            bleed: 0, deco: 0,
        }
    }
    /// Create an empty line.
    pub fn empty() -> Self {
        Self {
            spans: SmallVec::new(),
            bg: None,
            margin: false,
            indent: 0,
            bleed: 0, deco: 0,
        }
    }
    /// Total visible character width.
    pub fn visible_width(&self) -> usize {
        self.spans.iter().map(|s| display_width(&s.text)).sum()
    }
}

/// Edge spacing around a region's content area.
#[derive(Debug, Clone, Copy)]
pub struct Padding {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl Padding {
    /// No padding.
    pub fn zero() -> Self {
        Self {
            left: 0,
            right: 0,
            top: 0,
            bottom: 0,
        }
    }
    /// Symmetric horizontal padding.
    #[cfg(test)]
    pub fn horizontal(h: u16) -> Self {
        Self {
            left: h,
            right: h,
            top: 0,
            bottom: 0,
        }
    }
}

/// Visible display width — accounts for wide chars (emoji, CJK), skips OSC8 escapes.
pub fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&']') {
            while let Some(c) = chars.next() {
                if c == '\x1b' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        w += char_width(ch);
    }
    w
}

/// Terminal display width of a single character.
/// Follows unicode-width (UAX #11) strictly. Some emoji without
/// Emoji_Presentation may render wider on certain terminals — that's
/// a terminal-specific behavior we can't reliably predict.
pub fn char_width(ch: char) -> usize {
    if ch == '\t' {
        return 4;
    }
    use unicode_width::UnicodeWidthChar;
    ch.width().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::palette;
    use smallvec::smallvec;

    #[test]
    fn span_new() {
        let s = Span::new("hello", palette::FG);
        assert_eq!(s.text, "hello");
        assert!(!s.bold);
    }
    #[test]
    fn line_visible_width() {
        let l = Line::new(smallvec![
            Span::new("ab", palette::FG),
            Span::new("cd", palette::DIM)
        ]);
        assert_eq!(l.visible_width(), 4);
    }
    #[test]
    fn line_empty() {
        let l = Line::empty();
        assert_eq!(l.visible_width(), 0);
        assert!(l.spans.is_empty());
    }
    #[test]
    fn padding_zero() {
        let p = Padding::zero();
        assert_eq!(p.left, 0);
        assert_eq!(p.right, 0);
    }
    #[test]
    fn padding_horizontal() {
        let p = Padding::horizontal(3);
        assert_eq!(p.left, 3);
        assert_eq!(p.right, 3);
        assert_eq!(p.top, 0);
    }
}
