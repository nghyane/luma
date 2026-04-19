/// Diff line rendering and helpers shared across block types.
use crate::tool::diff::{DiffKind, parse_diff_line};
use crate::tui::markdown::highlight::highlight_code_with_lang;
use crate::tui::text::{Line, Span};
use crate::tui::theme::palette;
use smallvec::smallvec;

/// Render a diff output line with optional language hint.
pub fn diff_line_lang(raw: &str, lang: Option<&str>) -> Line {
    let dl = parse_diff_line(raw);

    if dl.kind == DiffKind::Separator {
        return Line::new(smallvec![
            Span::new("  ".to_owned(), palette::DIM),
            Span::new("...".to_owned(), palette::MUTED),
        ]);
    }

    let (marker, marker_color, bg) = match dl.kind {
        DiffKind::Add => ("+", palette::DIFF_ADD, Some(palette::DIFF_ADD_BG)),
        DiffKind::Del => ("-", palette::DIFF_DEL, Some(palette::DIFF_DEL_BG)),
        DiffKind::Context => (" ", palette::DIM, None),
        DiffKind::Separator => unreachable!(),
    };

    let mut spans = smallvec![Span::new("  ".to_owned(), palette::DIM)];

    if dl.lineno > 0 {
        let mut num_span = Span::new(format!("{:>3} ", dl.lineno), palette::DIFF_NUM);
        num_span.bg = bg;
        spans.push(num_span);
    }

    let mut marker_span = Span::new(format!("{marker} "), marker_color);
    marker_span.bg = bg;
    spans.push(marker_span);

    if dl.kind == DiffKind::Add || dl.kind == DiffKind::Del {
        for mut s in highlight_code_with_lang(&dl.text, lang) {
            s.bg = bg;
            spans.push(s);
        }
    } else {
        let mut s = Span::new(dl.text, palette::DIM);
        s.bg = bg;
        spans.push(s);
    }

    let mut line = Line::new(spans);
    // Mark gutter (indent + line number + marker) as decoration
    // so it's excluded from text selection: "  " (2) + "NNN " (4) + "+ " (2) = 8
    line.deco = 8;
    line
}

/// Infer language hint from a file path extension.
pub fn lang_from_path(path: &str) -> Option<&str> {
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "mjs" | "cjs" => Some("js"),
        "ts" | "mts" | "cts" | "tsx" | "jsx" => Some("ts"),
        _ => None,
    }
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() {
            if b[i + 1] == b'[' {
                i += 2;
                while i < b.len() && !b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if i < b.len() {
                    i += 1;
                }
            } else if b[i + 1] == b']' {
                i += 2;
                while i < b.len() {
                    if b[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 2;
            }
        } else {
            let start = i;
            i += 1;
            while i < b.len() && b[i] & 0xC0 == 0x80 {
                i += 1;
            }
            out.push_str(&s[start..i]);
        }
    }
    out
}
