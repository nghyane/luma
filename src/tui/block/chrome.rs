use super::SkillBlock;
/// Chrome renders — user prompt, skill, thinking, wrap helpers.
use super::render::RenderState;
use crate::tui::markdown::parse_inline;
use crate::tui::stream::StreamBuf;
use crate::tui::text::{Line, Span};
use crate::tui::theme::{icon, palette};
use smallvec::smallvec;

pub const ICON_INFO: &str = icon::INFO;
pub const ICON_ERROR: &str = icon::ERROR;
pub const ICON_WARN: &str = icon::WARN;

/// Cache for thinking block — avoids re-parsing committed lines.
#[derive(Debug, Clone)]
pub struct ThinkingCache {
    committed_count: usize,
    lines: Vec<Line>,
    width: usize,
}

impl ThinkingCache {
    fn new() -> Self {
        Self {
            committed_count: 0,
            lines: Vec::new(),
            width: 0,
        }
    }
}

/// Render thinking block with inline markdown + dim styling.
pub fn render_thinking(stream: &StreamBuf, state: &mut RenderState, width: usize) -> Vec<Line> {
    let cache = ensure_thinking_cache(state);

    if cache.width != width {
        cache.committed_count = 0;
        cache.lines.clear();
        cache.width = width;
    }

    // Parse new committed lines incrementally
    for text in &stream.committed[cache.committed_count..] {
        let is_first = cache.committed_count == 0 && cache.lines.is_empty();
        let rendered = render_thinking_line(text, is_first, width);
        cache.lines.extend(rendered);
        cache.committed_count += 1;
    }

    let mut result = cache.lines.clone();

    // Partial line
    if !stream.partial().is_empty() {
        let is_first = cache.committed_count == 0 && cache.lines.is_empty();
        result.extend(render_thinking_line(stream.partial(), is_first, width));
    }

    result
}

fn render_thinking_line(text: &str, is_first: bool, width: usize) -> Vec<Line> {
    let spans = parse_inline(text);
    let dim_spans = dim_spans(spans);

    let (line, pad) = if is_first {
        let mut all = smallvec![Span::italic("Thinking: ".to_owned(), palette::WARN)];
        all.extend(dim_spans);
        (Line::new(all), Some("  "))
    } else {
        (Line::new(dim_spans), None)
    };
    crate::tui::text::wrap_line(&line, width, pad)
}

/// Dim all spans to MUTED color while preserving other formatting.
fn dim_spans(spans: smallvec::SmallVec<[Span; 4]>) -> smallvec::SmallVec<[Span; 4]> {
    spans
        .into_iter()
        .map(|mut s| {
            s.fg = palette::MUTED;
            s
        })
        .collect()
}

fn ensure_thinking_cache(state: &mut RenderState) -> &mut ThinkingCache {
    if !matches!(state, RenderState::Thinking(_)) {
        *state = RenderState::Thinking(ThinkingCache::new());
    }
    match state {
        RenderState::Thinking(c) => c,
        _ => unreachable!(),
    }
}

/// Render content blocks into lines. Shared by prompt input and user bubble.
/// Text → spans, Image → inline chip, Paste → inline chip. The non-text
/// variants (ToolUse / ToolResult / Thinking / RedactedThinking) never
/// appear in prompt input or user bubbles and are ignored here.
pub fn content_lines(content: &[crate::core::types::ContentBlock]) -> Vec<Line> {
    use crate::core::types::ContentBlock;
    let mut lines = Vec::new();
    let mut cur: smallvec::SmallVec<[Span; 4]> = smallvec![];
    let mut img_n = 0;

    for block in content {
        match block {
            ContentBlock::Text { text } => {
                for (i, part) in text.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Line::new(std::mem::take(&mut cur)));
                    }
                    if !part.is_empty() {
                        cur.push(Span::new(part.to_owned(), palette::FG));
                    }
                }
            }
            ContentBlock::Image { .. } => {
                img_n += 1;
                cur.push(Span::with_bg(
                    format!(" Image {img_n} "),
                    palette::BG,
                    palette::FILE_REF,
                ));
                cur.push(Span::new(" ".to_owned(), palette::FG));
            }
            ContentBlock::Paste { text } => {
                let n = text.lines().count();
                cur.push(Span::with_bg(
                    format!(" Pasted ~{n} lines "),
                    palette::BG,
                    palette::WARN,
                ));
                cur.push(Span::new(" ".to_owned(), palette::FG));
            }
            ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::CodexReasoning { .. } => {
                // Not a user-facing prompt block — skip.
            }
        }
    }
    if !cur.is_empty() {
        lines.push(Line::new(cur));
    }
    lines
}

/// Render user prompt block — bar + content_lines wrapped.
pub fn render_user(content: &[crate::core::types::ContentBlock], content_w: usize) -> Vec<Line> {
    use crate::tui::theme::CONTENT_PAD;
    let bg = palette::USER_BG;
    let bleed = CONTENT_PAD;
    let bar_str = format!("{}  ", icon::PROMPT);
    let bar_w = crate::tui::text::display_width(&bar_str);
    let inner_w = content_w.saturating_sub(bar_w);
    let bar_line = Line {
        spans: smallvec![Span::deco(icon::PROMPT.to_owned(), palette::ACCENT)],
        bg: Some(bg),
        margin: false,
        indent: 0,
        bleed,
        deco: 0,
    };

    let mut result = vec![bar_line.clone()];
    for cl in content_lines(content) {
        for wl in crate::tui::text::wrap_line(&cl, inner_w, None) {
            let mut spans = smallvec![Span::deco(bar_str.clone(), palette::ACCENT)];
            spans.extend(wl.spans);
            result.push(Line {
                spans,
                bg: Some(bg),
                margin: false,
                indent: 0,
                bleed,
                deco: 0,
            });
        }
    }
    result.push(bar_line);
    result
}

/// Render skill block.
pub fn render_skill(sb: &SkillBlock, width: usize) -> Vec<Line> {
    if sb.is_done {
        super::render::wrap_icon(icon::SKILL, palette::MUTED, &sb.end_summary, width)
    } else {
        let line = Line::new(smallvec![
            Span::new(format!("{} ", icon::SKILL), palette::SUCCESS),
            Span::bold(sb.name.clone(), palette::SUCCESS),
        ]);
        crate::tui::text::wrap_line(&line, width, None)
    }
}
