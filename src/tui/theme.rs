//! Catppuccin Mocha palette and layout constants.

/// RGB color as (r, g, b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Catppuccin Mocha colors.
pub mod palette {
    use super::Rgb;
    pub const BG: Rgb = Rgb(17, 17, 27);
    pub const USER_BG: Rgb = Rgb(22, 22, 35);

    pub const SURFACE: Rgb = Rgb(24, 24, 37);
    pub const FG: Rgb = Rgb(205, 214, 244);
    pub const DIM: Rgb = Rgb(108, 112, 134);
    pub const ACCENT: Rgb = Rgb(137, 180, 250);
    pub const SUCCESS: Rgb = Rgb(166, 227, 161);
    pub const WARN: Rgb = Rgb(249, 226, 175);
    pub const ERROR: Rgb = Rgb(243, 139, 168);
    pub const MUTED: Rgb = Rgb(69, 71, 90);
    pub const BORDER: Rgb = Rgb(49, 50, 68);
    pub const PEACH: Rgb = Rgb(250, 179, 135);
    pub const DIFF_ADD: Rgb = Rgb(166, 227, 161);
    pub const DIFF_DEL: Rgb = Rgb(243, 139, 168);

    pub const DIFF_ADD_BG: Rgb = Rgb(30, 50, 30);
    pub const DIFF_DEL_BG: Rgb = Rgb(55, 25, 30);
    pub const DIFF_NUM: Rgb = Rgb(69, 71, 90);

    pub const FILE_REF: Rgb = PEACH;

    pub const MODE_RUSH: Rgb = WARN;
    pub const MODE_SMART: Rgb = ACCENT;
    pub const MODE_DEEP: Rgb = SUCCESS;
}

/// Icons and glyphs.
pub mod icon {
    pub const PROMPT: &str = "┃";
    pub const TOOL_OUT: &str = "→";
    pub const TOOL_IN: &str = "←";
    pub const SKILL: &str = "◈";

    pub const ERROR: &str = "✗";
    pub const WARN: &str = "!";
    pub const INFO: &str = "·";

    /// Spinner frames — braille dots cycle (all Narrow east_asian_width, safe on CJK terminals).
    pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
}

/// Horizontal padding inside output/input regions (cells).
pub const CONTENT_PAD: u16 = 2;
/// Outer margin from terminal edge to region (cells).
pub const OUTER_MARGIN: u16 = 2;
