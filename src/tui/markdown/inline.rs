/// Inline markdown parser — bold, italic, code, links, strikethrough.
use crate::tui::text::Span;
use crate::tui::theme::palette;
use smallvec::SmallVec;

/// Parse inline markdown into styled Spans (strict — unmatched markers stay as text).
pub fn parse_inline(text: &str) -> SmallVec<[Span; 4]> {
    parse_inline_inner(text, false)
}

/// Parse inline markdown optimistically — unclosed markers render styled to end.
pub fn parse_inline_streaming(text: &str) -> SmallVec<[Span; 4]> {
    parse_inline_inner(text, true)
}

fn parse_inline_inner(text: &str, streaming: bool) -> SmallVec<[Span; 4]> {
    let mut spans = SmallVec::new();
    let bytes = text.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // **bold**
        if bytes[pos] == b'*' && pos + 1 < bytes.len() && bytes[pos + 1] == b'*' {
            if let Some(end) = text[pos + 2..].find("**") {
                spans.push(Span::bold(
                    text[pos + 2..pos + 2 + end].to_owned(),
                    palette::PEACH,
                ));
                pos = pos + 2 + end + 2;
                continue;
            }
            if streaming {
                let rest = &text[pos + 2..];
                if !rest.is_empty() {
                    spans.push(Span::bold(rest.to_owned(), palette::PEACH));
                    pos = bytes.len();
                    continue;
                }
            }
            spans.push(Span::new("**".to_owned(), palette::FG));
            pos += 2;
            continue;
        }

        // *italic*
        if bytes[pos] == b'*' {
            if let Some(end) = text[pos + 1..].find('*') {
                let close_pos = pos + 1 + end;
                let inner = &text[pos + 1..close_pos];
                let is_double = close_pos + 1 < bytes.len() && bytes[close_pos + 1] == b'*';
                if !is_double
                    && !inner.is_empty()
                    && !inner.starts_with(' ')
                    && !inner.ends_with(' ')
                {
                    spans.push(Span::italic(inner.to_owned(), palette::FG));
                    pos = close_pos + 1;
                    continue;
                }
            }
            if streaming {
                let rest = &text[pos + 1..];
                if !rest.is_empty() {
                    spans.push(Span::italic(rest.to_owned(), palette::FG));
                    pos = bytes.len();
                    continue;
                }
            }
            spans.push(Span::new("*".to_owned(), palette::FG));
            pos += 1;
            continue;
        }

        // ~~strikethrough~~
        if bytes[pos] == b'~' {
            if pos + 1 < bytes.len() && bytes[pos + 1] == b'~' {
                if let Some(end) = text[pos + 2..].find("~~") {
                    spans.push(Span::new(
                        text[pos + 2..pos + 2 + end].to_owned(),
                        palette::MUTED,
                    ));
                    pos = pos + 2 + end + 2;
                    continue;
                }
                spans.push(Span::new("~~".to_owned(), palette::FG));
                pos += 2;
                continue;
            }
            // Single ~ — treat as plain text
            spans.push(Span::new("~".to_owned(), palette::FG));
            pos += 1;
            continue;
        }

        // `code`
        if bytes[pos] == b'`' {
            if let Some(end) = text[pos + 1..].find('`') {
                spans.push(Span::new(
                    text[pos + 1..pos + 1 + end].to_owned(),
                    palette::ACCENT,
                ));
                pos = pos + 1 + end + 1;
                continue;
            }
            if streaming {
                let rest = &text[pos + 1..];
                if !rest.is_empty() {
                    spans.push(Span::new(rest.to_owned(), palette::ACCENT));
                    pos = bytes.len();
                    continue;
                }
            }
            spans.push(Span::new("`".to_owned(), palette::FG));
            pos += 1;
            continue;
        }

        // [label](url) → OSC8 hyperlink (Unix) or plain label (Windows)
        if bytes[pos] == b'[' {
            if let Some(close) = text[pos..].find("](") {
                let label = &text[pos + 1..pos + close];
                let url_start = pos + close + 2;
                if let Some(url_end) = text[url_start..].find(')') {
                    let url = &text[url_start..url_start + url_end];
                    #[cfg(not(windows))]
                    let display = format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\");
                    #[cfg(windows)]
                    let display = format!("{label} ({url})");
                    spans.push(Span::new(display, palette::ACCENT));
                    pos = url_start + url_end + 1;
                    continue;
                }
            }
            spans.push(Span::new("[".to_owned(), palette::FG));
            pos += 1;
            continue;
        }

        // Plain text until next special char
        let start = pos;
        while pos < bytes.len()
            && bytes[pos] != b'*'
            && bytes[pos] != b'`'
            && bytes[pos] != b'['
            && bytes[pos] != b'~'
        {
            pos += 1;
        }
        if pos > start {
            spans.push(Span::new(text[start..pos].to_owned(), palette::FG));
        }
    }

    if spans.is_empty() {
        spans.push(Span::new(text.to_owned(), palette::FG));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold() {
        let spans = parse_inline("hello **world** end");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].text, "world");
        assert!(spans[1].bold);
    }

    #[test]
    fn inline_code() {
        let spans = parse_inline("use `cargo` here");
        assert_eq!(spans[1].text, "cargo");
        assert_eq!(spans[1].fg, palette::ACCENT);
    }

    #[test]
    fn inline_plain() {
        let spans = parse_inline("just text");
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn bold_with_colon() {
        let spans = parse_inline("**Ví dụ những gì tôi có thể làm:**");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].bold);
    }

    #[test]
    fn italic_not_eating_double_star() {
        let spans = parse_inline("*foo**");
        assert!(!spans.iter().any(|s| s.italic && s.text == "foo"));
    }

    #[test]
    fn strikethrough() {
        let spans = parse_inline("hello ~~removed~~ end");
        assert_eq!(spans[1].text, "removed");
        assert_eq!(spans[1].fg, palette::MUTED);
    }

    #[test]
    fn single_tilde_plain_text() {
        let spans = parse_inline("~15K dòng Rust");
        let all: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(all, "~15K dòng Rust");
    }

    #[test]
    fn tilde_in_sentence() {
        let spans = parse_inline("project có ~15K dòng, ~200 files");
        let all: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(all, "project có ~15K dòng, ~200 files");
    }
}
