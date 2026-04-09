/// Syntax highlighting for code blocks — lightweight language-aware coloring.
use crate::tui::text::Span;
use crate::tui::theme::{Rgb, palette};
use smallvec::SmallVec;

const COLOR_KEYWORD: Rgb = Rgb(198, 160, 246);
const COLOR_STRING: Rgb = Rgb(166, 227, 161);
const COLOR_NUMBER: Rgb = Rgb(250, 179, 135);
const COLOR_FUNCTION: Rgb = Rgb(137, 220, 235);
const COLOR_OPERATOR_GENERIC: Rgb = Rgb(137, 180, 250);
const COLOR_OPERATOR_RUST: Rgb = Rgb(116, 199, 236);
const COLOR_OPERATOR_PY: Rgb = Rgb(249, 226, 175);
const COLOR_OPERATOR_JS: Rgb = Rgb(245, 194, 231);

const GENERIC_KEYWORDS: &[&str] = &[
    "import",
    "export",
    "from",
    "const",
    "let",
    "var",
    "function",
    "class",
    "return",
    "if",
    "else",
    "for",
    "while",
    "async",
    "await",
    "fn",
    "pub",
    "mut",
    "impl",
    "struct",
    "trait",
    "use",
    "mod",
    "type",
    "interface",
    "true",
    "false",
    "null",
    "undefined",
    "None",
    "self",
    "def",
];
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "Self", "self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "async", "await", "class", "def", "elif", "else", "False", "for", "from", "if",
    "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
    "self", "True", "try", "while", "with", "yield",
];
const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
    // Common builtins highlighted as keywords
    "console",
    "document",
    "window",
    "process",
    "module",
    "require",
    "exports",
    "Promise",
    "Array",
    "Object",
    "String",
    "Number",
    "Boolean",
    "Map",
    "Set",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
];

/// Highlight a code line into styled spans.
#[cfg(test)]
pub fn highlight_code(code: &str) -> SmallVec<[Span; 4]> {
    highlight_code_with_lang(code, None)
}

/// Highlight a code line using an optional fenced language hint.
pub fn highlight_code_with_lang(code: &str, lang: Option<&str>) -> SmallVec<[Span; 4]> {
    let lang = normalise_lang(lang);
    let mut spans: SmallVec<[Span; 4]> = SmallVec::new();
    if let Some(idx) = find_comment_start(code, lang) {
        if idx > 0 {
            highlight_tokens(&code[..idx], lang, &mut spans);
        }
        spans.push(Span::italic(code[idx..].to_owned(), palette::DIM));
        return spans;
    }
    highlight_tokens(code, lang, &mut spans);
    spans
}

fn find_comment_start(code: &str, lang: Option<&str>) -> Option<usize> {
    match lang {
        Some("py") => code
            .match_indices('#')
            .map(|(i, _)| i)
            .find(|&i| i == 0 || code.as_bytes()[i - 1] == b' '),
        _ => {
            if let Some((i, _)) = code.match_indices("//").next() {
                return Some(i);
            }
            code.match_indices('#')
                .map(|(i, _)| i)
                .find(|&i| i == 0 || code.as_bytes()[i - 1] == b' ')
        }
    }
}

fn highlight_tokens(code: &str, lang: Option<&str>, spans: &mut SmallVec<[Span; 4]>) {
    let mut pos = 0;
    let bytes = code.as_bytes();
    let keywords = keywords_for(lang);

    while pos < bytes.len() {
        // String literals
        if let Some(end) = string_end(code, pos, lang) {
            spans.push(Span::new(code[pos..end].to_owned(), COLOR_STRING));
            pos = end;
            continue;
        }

        // Numbers
        if bytes[pos].is_ascii_digit() {
            let start = pos;
            while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b'.') {
                pos += 1;
            }
            spans.push(Span::new(code[start..pos].to_owned(), COLOR_NUMBER));
            continue;
        }

        // Identifiers / keywords
        if bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_' {
            let start = pos;
            while pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            let word = &code[start..pos];
            let after_dot = start > 0 && bytes[start - 1] == b'.';
            let before_paren = pos < bytes.len() && bytes[pos] == b'(';
            let color = if keywords.contains(&word) {
                COLOR_KEYWORD
            } else if after_dot || before_paren {
                // Method calls (obj.method) and function calls (func())
                COLOR_FUNCTION
            } else {
                palette::FG
            };
            spans.push(Span::new(word.to_owned(), color));
            continue;
        }

        // Whitespace
        if bytes[pos] == b' ' || bytes[pos] == b'\t' {
            let start = pos;
            while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
                pos += 1;
            }
            spans.push(Span::new(code[start..pos].to_owned(), palette::FG));
            continue;
        }

        // Multi-byte UTF-8
        if bytes[pos] >= 0x80 {
            let start = pos;
            while pos < bytes.len() && bytes[pos] >= 0x80 {
                pos += 1;
            }
            while pos < bytes.len() && bytes[pos].is_ascii_alphanumeric() {
                pos += 1;
            }
            spans.push(Span::new(code[start..pos].to_owned(), palette::FG));
            continue;
        }

        // Punctuation/operators
        let end = operator_end(code, pos, lang);
        let token = &code[pos..end];
        let color = if is_operator(token, lang) {
            operator_color(lang)
        } else if token.chars().all(is_punctuation) {
            palette::DIM
        } else {
            palette::FG
        };
        spans.push(Span::new(token.to_owned(), color));
        pos = end;
    }
}

fn string_end(code: &str, start: usize, lang: Option<&str>) -> Option<usize> {
    let bytes = code.as_bytes();
    if start >= bytes.len() {
        return None;
    }

    if matches!(lang, Some("rust"))
        && bytes[start] == b'r'
        && start + 1 < bytes.len()
        && (bytes[start + 1] == b'"' || bytes[start + 1] == b'#')
    {
        return raw_string_end(code, start + 1);
    }

    if matches!(lang, Some("rust"))
        && bytes[start] == b'b'
        && start + 1 < bytes.len()
        && (bytes[start + 1] == b'"' || bytes[start + 1] == b'\'')
    {
        return quoted_string_end(code, start + 1);
    }

    if matches!(lang, Some("js")) && bytes[start] == b'`' {
        return template_string_end(code, start);
    }

    if bytes[start] == b'"' || bytes[start] == b'\'' {
        return quoted_string_end(code, start);
    }

    None
}

fn quoted_string_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let quote = *bytes.get(start)?;
    let mut pos = start + 1;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' {
            pos += 2;
            continue;
        }
        pos += 1;
        if bytes[pos - 1] == quote {
            return Some(pos);
        }
    }
    Some(bytes.len())
}

fn raw_string_end(code: &str, marker_start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut hashes = 0usize;
    let mut pos = marker_start;
    while pos < bytes.len() && bytes[pos] == b'#' {
        hashes += 1;
        pos += 1;
    }
    if *bytes.get(pos)? != b'"' {
        return None;
    }
    pos += 1;
    while pos < bytes.len() {
        if bytes[pos] == b'"' {
            let mut end = pos + 1;
            let mut seen = 0usize;
            while end < bytes.len() && seen < hashes && bytes[end] == b'#' {
                seen += 1;
                end += 1;
            }
            if seen == hashes {
                return Some(end);
            }
        }
        pos += 1;
    }
    Some(bytes.len())
}

fn template_string_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut pos = start + 1;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' {
            pos += 2;
            continue;
        }
        pos += 1;
        if bytes[pos - 1] == b'`' {
            return Some(pos);
        }
    }
    Some(bytes.len())
}

fn operator_end(code: &str, start: usize, lang: Option<&str>) -> usize {
    let bytes = code.as_bytes();
    let next = start + 1;
    // Non-ASCII byte at start → not an operator, skip this char
    if next > bytes.len() || !code.is_char_boundary(next) {
        // Advance past the full UTF-8 char
        let mut end = next;
        while end < bytes.len() && !code.is_char_boundary(end) {
            end += 1;
        }
        return end;
    }
    let mut best_end = if is_operator(&code[start..next], lang) {
        next
    } else {
        return next;
    };
    let mut end = next;
    while end < bytes.len() {
        let n = end + 1;
        if n > bytes.len() || !code.is_char_boundary(n) {
            break;
        }
        if is_operator(&code[start..n], lang) {
            best_end = n;
            end += 1;
            continue;
        }
        break;
    }
    best_end
}

fn is_operator(token: &str, lang: Option<&str>) -> bool {
    match lang {
        Some("rust") => matches!(
            token,
            "=" | "=="
                | "!="
                | "+"
                | "-"
                | "*"
                | "/"
                | "%"
                | "->"
                | "=>"
                | "::"
                | ":"
                | "&&"
                | "||"
                | "&"
                | "|"
                | ".."
                | "..="
                | "<"
                | ">"
                | "<="
                | ">="
                | "?"
        ),
        Some("py") => matches!(
            token,
            "=" | "=="
                | "!="
                | "+"
                | "-"
                | "*"
                | "/"
                | "//"
                | "%"
                | "**"
                | ":"
                | "<"
                | ">"
                | "<="
                | ">="
        ),
        Some("js") => matches!(
            token,
            "=" | "=="
                | "==="
                | "!="
                | "!=="
                | "+"
                | "-"
                | "*"
                | "/"
                | "%"
                | "=>"
                | ":"
                | "&&"
                | "||"
                | "??"
                | "."
                | "?."
                | "<"
                | ">"
                | "<="
                | ">="
        ),
        _ => matches!(
            token,
            "=" | "+" | "-" | "*" | "/" | "<" | ">" | "!" | "&" | "|" | ":"
        ),
    }
}

fn operator_color(lang: Option<&str>) -> Rgb {
    match lang {
        Some("rust") => COLOR_OPERATOR_RUST,
        Some("py") => COLOR_OPERATOR_PY,
        Some("js") => COLOR_OPERATOR_JS,
        _ => COLOR_OPERATOR_GENERIC,
    }
}

fn is_punctuation(ch: char) -> bool {
    matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ';' | ',')
}

fn keywords_for(lang: Option<&str>) -> &'static [&'static str] {
    match normalise_lang(lang) {
        Some("rust") => RUST_KEYWORDS,
        Some("py") => PYTHON_KEYWORDS,
        Some("js") => JS_KEYWORDS,
        _ => GENERIC_KEYWORDS,
    }
}

fn normalise_lang(lang: Option<&str>) -> Option<&'static str> {
    let s = lang?.trim();
    if s.eq_ignore_ascii_case("rust") || s.eq_ignore_ascii_case("rs") {
        Some("rust")
    } else if s.eq_ignore_ascii_case("python") || s.eq_ignore_ascii_case("py") {
        Some("py")
    } else if s.eq_ignore_ascii_case("javascript")
        || s.eq_ignore_ascii_case("js")
        || s.eq_ignore_ascii_case("typescript")
        || s.eq_ignore_ascii_case("ts")
        || s.eq_ignore_ascii_case("tsx")
        || s.eq_ignore_ascii_case("jsx")
    {
        Some("js")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_basic() {
        let spans = highlight_code("let x = 42;");
        assert!(spans.len() > 1);
    }

    #[test]
    fn highlight_python_uses_lang_keywords() {
        let spans = highlight_code_with_lang("yield value", Some("python"));
        assert_eq!(spans[0].text, "yield");
        assert_eq!(spans[0].fg, Rgb(198, 160, 246));
    }

    #[test]
    fn highlight_rust_uses_lang_keywords() {
        let spans = highlight_code_with_lang("match value {", Some("rust"));
        assert_eq!(spans[0].text, "match");
        assert_eq!(spans[0].fg, COLOR_KEYWORD);
    }

    #[test]
    fn highlight_rust_raw_string_and_operator() {
        let spans = highlight_code_with_lang("let x = r#\"hi\"#;", Some("rust"));
        assert!(
            spans
                .iter()
                .any(|s| s.text == "=" && s.fg == COLOR_OPERATOR_RUST)
        );
        assert!(
            spans
                .iter()
                .any(|s| s.text == "r#\"hi\"#" && s.fg == COLOR_STRING)
        );
    }

    #[test]
    fn highlight_python_comment_and_operator() {
        let spans = highlight_code_with_lang("value ** 2 # square", Some("python"));
        assert!(
            spans
                .iter()
                .any(|s| s.text == "**" && s.fg == COLOR_OPERATOR_PY)
        );
        assert!(spans.iter().any(|s| s.text == "# square" && s.italic));
    }

    #[test]
    fn highlight_js_template_and_operator() {
        let spans = highlight_code_with_lang("const msg = `hi`;", Some("js"));
        assert!(
            spans
                .iter()
                .any(|s| s.text == "=" && s.fg == COLOR_OPERATOR_JS)
        );
        assert!(
            spans
                .iter()
                .any(|s| s.text == "`hi`" && s.fg == COLOR_STRING)
        );
    }

    #[test]
    fn highlight_js_console_log() {
        let spans = highlight_code_with_lang("console.log(\"hello\")", Some("js"));
        assert!(
            spans
                .iter()
                .any(|s| s.text == "console" && s.fg == COLOR_KEYWORD),
            "console: {:?}",
            spans.iter().map(|s| (&s.text, s.fg)).collect::<Vec<_>>()
        );
        assert!(
            spans
                .iter()
                .any(|s| s.text == "log" && s.fg == COLOR_FUNCTION),
            "log: {:?}",
            spans.iter().map(|s| (&s.text, s.fg)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn highlight_function_call() {
        let spans = highlight_code_with_lang("foo(bar)", None);
        assert!(
            spans
                .iter()
                .any(|s| s.text == "foo" && s.fg == COLOR_FUNCTION),
            "foo should be function color: {:?}",
            spans.iter().map(|s| (&s.text, s.fg)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn highlight_utf8_with_operators() {
        // Regression: "**Đ" panicked on byte boundary inside UTF-8 char
        let code = "**Đo trước khi optimize.**";
        let spans = highlight_code_with_lang(code, None);
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Đo"), "UTF-8 preserved: {text}");
    }
}
