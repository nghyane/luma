/// Word-aware line wrapping with char_width support.
use super::{Line, Rgb, Span, char_width, display_width};
use smallvec::SmallVec;

/// Wrap a line at `width`, breaking at word boundaries.
pub fn wrap_line(line: &Line, width: usize, cont_pad: Option<&str>) -> Vec<Line> {
    if width == 0 {
        return vec![line.clone()];
    }
    if line.visible_width() <= width {
        return vec![line.clone()];
    }

    let flat: String = line.spans.iter().map(|s| s.text.as_str()).collect();
    let chars: Vec<(usize, char, usize)> = flat
        .char_indices()
        .map(|(byte_off, ch)| (byte_off, ch, char_width(ch)))
        .collect();

    let pad = cont_pad.unwrap_or("");
    let pad_w = display_width(pad);
    let mut result = Vec::new();
    let mut ci = 0;
    let mut is_first = true;

    while ci < chars.len() {
        let line_w = if is_first {
            width
        } else {
            width.saturating_sub(pad_w)
        };
        let mut col = 0;
        let mut last_space_ci = None;
        let mut end_ci = ci;

        for (j, &(_, ch, cw)) in chars.iter().enumerate().skip(ci) {
            if col + cw > line_w {
                break;
            }
            if ch == ' ' {
                last_space_ci = Some(j + 1);
            }
            col += cw;
            end_ci = j + 1;
        }

        if end_ci >= chars.len() {
            let byte_start = chars[ci].0;
            result.push(build_wrapped_line(
                &line.spans,
                byte_start,
                flat.len(),
                if is_first { "" } else { pad },
                line.bg,
                line.bleed,
            ));
            break;
        }

        if end_ci == ci {
            // Single char wider than line — force advance to avoid infinite loop
            end_ci = ci + 1;
        }
        let min_ci = ci + (end_ci - ci) * 3 / 10;
        let break_ci = last_space_ci.filter(|&s| s > min_ci).unwrap_or(end_ci);
        let byte_start = chars[ci].0;
        let byte_end = if break_ci < chars.len() {
            chars[break_ci].0
        } else {
            flat.len()
        };
        result.push(build_wrapped_line(
            &line.spans,
            byte_start,
            byte_end,
            if is_first { "" } else { pad },
            line.bg,
            line.bleed,
        ));
        ci = break_ci;
        is_first = false;
    }
    result
}

fn build_wrapped_line(
    original: &[Span],
    start: usize,
    end: usize,
    prefix: &str,
    bg: Option<Rgb>,
    bleed: u16,
) -> Line {
    let mut spans = SmallVec::new();
    if !prefix.is_empty()
        && let Some(first) = original.first()
    {
        spans.push(Span::new(prefix, first.fg));
    }
    let mut pos = 0;
    for s in original {
        let s_start = pos;
        let s_end = pos + s.text.len();
        pos = s_end;
        if s_end <= start || s_start >= end {
            continue;
        }
        let slice_start = start.saturating_sub(s_start);
        let slice_end = (end - s_start).min(s.text.len());
        let s_bytes = s.text.as_bytes();
        let safe_start = snap_to_char_boundary(s_bytes, slice_start);
        let safe_end = snap_to_char_boundary(s_bytes, slice_end);
        let sliced = &s.text[safe_start..safe_end];
        if !sliced.is_empty() {
            spans.push(Span {
                text: sliced.to_owned(),
                fg: s.fg,
                bg: s.bg,
                bold: s.bold,
                italic: s.italic,
                decoration: s.decoration,
            });
        }
    }
    Line {
        spans,
        bg,
        margin: false,
        indent: 0,
        bleed,
        deco: 0,
    }
}

/// Snap a byte offset to the nearest char boundary (forward).
fn snap_to_char_boundary(bytes: &[u8], offset: usize) -> usize {
    let mut i = offset.min(bytes.len());
    while i < bytes.len() && bytes[i] & 0xC0 == 0x80 {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::palette;
    use smallvec::smallvec;

    #[test]
    fn short_line_unchanged() {
        let l = Line::new(smallvec![Span::new("short", palette::FG)]);
        assert_eq!(wrap_line(&l, 80, Some("  ")).len(), 1);
    }
    #[test]
    fn long_line_breaks() {
        let long = "word ".repeat(20);
        let l = Line::new(smallvec![Span::new(long, palette::FG)]);
        assert!(wrap_line(&l, 40, Some("  ")).len() > 1);
    }
}
