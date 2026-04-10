/// Streaming JSON string-value extractor.
///
/// Feeds partial JSON chunks and emits unescaped content of one specific
/// top-level string field as it arrives, without parsing the full document.
///
/// Use case: Anthropic / OpenAI tool-call streaming. The model emits tool
/// arguments as `partial_json` deltas — incremental JSON fragments. To show
/// a live preview (e.g. "Write" tool's `content` field) we need to extract
/// just that one field's value as characters arrive, handling escapes and
/// UTF-8 boundaries correctly.
///
/// This is NOT a general JSON parser. It handles the shape the model emits:
/// a single top-level object with primitive fields and possibly nested
/// arrays/objects (which are skipped). It does not validate strictness.
use crate::core::types::ToolSchema;

/// Look up the streamable argument field for a tool by name.
///
/// Returns the name of the JSON field that should be streamed to the UI as
/// the tool's args arrive, or `None` if the tool opts out of streaming.
pub fn streamable_arg_for(tools: &[ToolSchema], tool_name: &str) -> Option<String> {
    tools
        .iter()
        .find(|t| t.name == tool_name)
        .and_then(|t| t.streamable_arg.clone())
}

/// Extracts a single top-level string field from a streaming JSON object.
///
/// Call [`Self::feed`] with chunks as they arrive; it returns the unescaped
/// portion of the target field's string value that became available.
#[derive(Debug)]
pub struct JsonStringExtractor {
    target: String,
    state: State,
    /// Buffer for incomplete escape sequences at chunk boundary (`\u...`
    /// needs up to 6 bytes: `\uXXXX`).
    escape_buf: String,
    /// Current key being read (when in `InKey`).
    current_key: String,
    /// Depth of nested structures. The target field only matches at depth 1.
    depth: u32,
}

/// Parser state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the opening `{` of the top-level object.
    BeforeObject,
    /// Expecting a key (after `{` or `,`), skipping whitespace.
    ExpectKey,
    /// Inside a key string literal, accumulating into `current_key`.
    InKey,
    /// After a key, expecting `:`.
    ExpectColon,
    /// After `:`, expecting the start of a value.
    ExpectValue,
    /// Inside a string value that is NOT the target — skip characters.
    InOtherString,
    /// Inside the target field's string value — emit characters.
    InTargetString,
    /// Inside an escape sequence inside a string (of either kind).
    /// Tracks which string type we're in.
    InEscape { target: bool },
    /// Inside a `\u` sequence; buffering 4 hex chars.
    InUnicodeEscape {
        target: bool,
        hex: [u8; 4],
        have: u8,
    },
    /// Inside a non-string value (number, true/false/null) — skip until
    /// `,`, `}`, or `]`.
    InPrimitive,
    /// Inside a nested array or object at depth > 1 — skip characters until
    /// the structure closes, tracking nested depth.
    InNested {
        nested_depth: u32,
        in_string: bool,
        escaped: bool,
    },
    /// After a key-value pair, expecting `,` or `}`.
    AfterValue,
    /// Parser done — saw closing `}` of top-level object.
    Done,
}

impl JsonStringExtractor {
    /// Create an extractor looking for the given top-level field name.
    pub fn new(target_field: impl Into<String>) -> Self {
        Self {
            target: target_field.into(),
            state: State::BeforeObject,
            escape_buf: String::new(),
            current_key: String::new(),
            depth: 0,
        }
    }

    /// Feed a chunk of JSON. Returns whatever portion of the target string
    /// value was unescaped from this chunk.
    ///
    /// Repeated calls accumulate: the caller should concatenate the returned
    /// strings to get the full value seen so far.
    pub fn feed(&mut self, chunk: &str) -> String {
        let mut out = String::new();
        for c in chunk.chars() {
            self.step(c, &mut out);
            if self.state == State::Done {
                break;
            }
        }
        out
    }

    fn step(&mut self, c: char, out: &mut String) {
        match self.state {
            State::BeforeObject => {
                if c == '{' {
                    self.depth = 1;
                    self.state = State::ExpectKey;
                }
                // Ignore whitespace and anything else until `{`.
            }
            State::ExpectKey => {
                if c.is_whitespace() || c == ',' {
                    return;
                }
                if c == '}' {
                    self.state = State::Done;
                    return;
                }
                if c == '"' {
                    self.current_key.clear();
                    self.state = State::InKey;
                }
                // Anything else is invalid in strict JSON; tolerate silently.
            }
            State::InKey => {
                if c == '\\' {
                    self.state = State::InEscape { target: false };
                    self.escape_buf.push('k'); // 'k' marker: escape inside key
                    return;
                }
                if c == '"' {
                    self.state = State::ExpectColon;
                    return;
                }
                self.current_key.push(c);
            }
            State::ExpectColon => {
                if c.is_whitespace() {
                    return;
                }
                if c == ':' {
                    self.state = State::ExpectValue;
                }
            }
            State::ExpectValue => {
                if c.is_whitespace() {
                    return;
                }
                let is_target = self.current_key == self.target;
                match c {
                    '"' => {
                        self.state = if is_target {
                            State::InTargetString
                        } else {
                            State::InOtherString
                        };
                    }
                    '{' | '[' => {
                        self.state = State::InNested {
                            nested_depth: 1,
                            in_string: false,
                            escaped: false,
                        };
                    }
                    _ => {
                        // Primitive: number, true, false, null.
                        self.state = State::InPrimitive;
                    }
                }
            }
            State::InOtherString => match c {
                '\\' => {
                    self.state = State::InEscape { target: false };
                }
                '"' => {
                    self.state = State::AfterValue;
                }
                _ => {}
            },
            State::InTargetString => match c {
                '\\' => {
                    self.state = State::InEscape { target: true };
                }
                '"' => {
                    self.state = State::AfterValue;
                }
                _ => {
                    out.push(c);
                }
            },
            State::InEscape { target } => {
                // Special marker for escape-inside-key (see InKey).
                let in_key = !self.escape_buf.is_empty() && self.escape_buf.as_bytes()[0] == b'k';
                if in_key {
                    self.escape_buf.clear();
                    let decoded = decode_simple_escape(c);
                    if let Some(d) = decoded {
                        self.current_key.push(d);
                        self.state = State::InKey;
                    } else if c == 'u' {
                        self.state = State::InUnicodeEscape {
                            target: false,
                            hex: [0; 4],
                            have: 0,
                        };
                        self.escape_buf.push('k');
                    } else {
                        // Unknown escape in key — push literally.
                        self.current_key.push(c);
                        self.state = State::InKey;
                    }
                    return;
                }
                if let Some(d) = decode_simple_escape(c) {
                    if target {
                        out.push(d);
                        self.state = State::InTargetString;
                    } else {
                        self.state = State::InOtherString;
                    }
                } else if c == 'u' {
                    self.state = State::InUnicodeEscape {
                        target,
                        hex: [0; 4],
                        have: 0,
                    };
                } else if target {
                    // Unknown escape — emit backslash + char literally.
                    out.push('\\');
                    out.push(c);
                    self.state = State::InTargetString;
                } else {
                    self.state = State::InOtherString;
                }
            }
            State::InUnicodeEscape {
                target,
                mut hex,
                mut have,
            } => {
                if !c.is_ascii_hexdigit() {
                    // Malformed — abort escape, resume string state.
                    self.state = if self.escape_buf.starts_with('k') {
                        self.escape_buf.clear();
                        State::InKey
                    } else if target {
                        State::InTargetString
                    } else {
                        State::InOtherString
                    };
                    return;
                }
                hex[have as usize] = c as u8;
                have += 1;
                if have == 4 {
                    let code = hex_to_u32(&hex);
                    let in_key = self.escape_buf.starts_with('k');
                    if in_key {
                        self.escape_buf.clear();
                    }
                    if let Some(ch) = char::from_u32(code) {
                        if in_key {
                            self.current_key.push(ch);
                        } else if target {
                            out.push(ch);
                        }
                    }
                    // Note: surrogate pairs (high + low) are not joined; the
                    // API emits non-BMP characters directly as escaped pairs
                    // which would need joining. For our use case (tool args
                    // are usually ASCII/BMP code/text), this is acceptable.
                    self.state = if in_key {
                        State::InKey
                    } else if target {
                        State::InTargetString
                    } else {
                        State::InOtherString
                    };
                } else {
                    self.state = State::InUnicodeEscape { target, hex, have };
                }
            }
            State::InPrimitive => {
                if c == ',' {
                    self.state = State::ExpectKey;
                } else if c == '}' && self.depth == 1 {
                    self.state = State::Done;
                }
                // Otherwise, keep consuming primitive characters.
            }
            State::InNested {
                mut nested_depth,
                mut in_string,
                mut escaped,
            } => {
                if escaped {
                    escaped = false;
                } else if in_string {
                    match c {
                        '\\' => escaped = true,
                        '"' => in_string = false,
                        _ => {}
                    }
                } else {
                    match c {
                        '"' => in_string = true,
                        '{' | '[' => nested_depth += 1,
                        '}' | ']' => {
                            nested_depth -= 1;
                            if nested_depth == 0 {
                                self.state = State::AfterValue;
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                self.state = State::InNested {
                    nested_depth,
                    in_string,
                    escaped,
                };
            }
            State::AfterValue => {
                if c.is_whitespace() {
                    return;
                }
                if c == ',' {
                    self.current_key.clear();
                    self.state = State::ExpectKey;
                } else if c == '}' {
                    self.state = State::Done;
                }
            }
            State::Done => {}
        }
    }
}

fn decode_simple_escape(c: char) -> Option<char> {
    Some(match c {
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        'b' => '\u{08}',
        'f' => '\u{0C}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        _ => return None,
    })
}

fn hex_to_u32(bytes: &[u8; 4]) -> u32 {
    let mut n = 0u32;
    for &b in bytes {
        n = n * 16
            + match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => 0,
            };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(target: &str, chunks: &[&str]) -> String {
        let mut ex = JsonStringExtractor::new(target);
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&ex.feed(chunk));
        }
        out
    }

    #[test]
    fn extracts_simple_field() {
        assert_eq!(
            feed_all("content", &[r#"{"content":"hello world"}"#]),
            "hello world"
        );
    }

    #[test]
    fn ignores_other_fields() {
        assert_eq!(
            feed_all("content", &[r#"{"path":"/tmp/a.txt","content":"body"}"#]),
            "body"
        );
    }

    #[test]
    fn handles_partial_chunks_across_boundaries() {
        assert_eq!(
            feed_all("content", &[r#"{"content":"he"#, r#"llo wo"#, r#"rld"}"#]),
            "hello world"
        );
    }

    #[test]
    fn decodes_simple_escapes() {
        assert_eq!(
            feed_all(
                "content",
                &[r#"{"content":"line1\nline2\ttab\"quote\\back"}"#]
            ),
            "line1\nline2\ttab\"quote\\back"
        );
    }

    #[test]
    fn decodes_unicode_escape() {
        // \u0041 = 'A'
        assert_eq!(feed_all("content", &[r#"{"content":"\u0041BC"}"#]), "ABC");
    }

    #[test]
    fn escape_split_across_chunks() {
        // Backslash in one chunk, the escape char in the next.
        assert_eq!(
            feed_all("content", &[r#"{"content":"hel"#, r#"\nworld"}"#]),
            "hel\nworld"
        );
    }

    #[test]
    fn unicode_escape_split_across_chunks() {
        assert_eq!(
            feed_all("content", &[r#"{"content":"\u00"#, r#"41"}"#]),
            "A"
        );
    }

    #[test]
    fn field_order_does_not_matter() {
        assert_eq!(
            feed_all("content", &[r#"{"a":1,"content":"x","b":true}"#]),
            "x"
        );
    }

    #[test]
    fn skips_nested_object_values() {
        assert_eq!(
            feed_all("content", &[r#"{"meta":{"k":"v"},"content":"text"}"#]),
            "text"
        );
    }

    #[test]
    fn skips_nested_array_values() {
        assert_eq!(
            feed_all("content", &[r#"{"tags":["a","b"],"content":"body"}"#]),
            "body"
        );
    }

    #[test]
    fn nested_structure_with_braces_in_strings() {
        // Inner string contains `}` — must not terminate the outer value early.
        assert_eq!(
            feed_all("content", &[r#"{"meta":{"s":"a}b"},"content":"ok"}"#]),
            "ok"
        );
    }

    #[test]
    fn target_field_not_present_emits_nothing() {
        assert_eq!(feed_all("content", &[r#"{"other":"value"}"#]), "");
    }

    #[test]
    fn incomplete_input_emits_partial() {
        assert_eq!(feed_all("content", &[r#"{"content":"hel"#]), "hel");
    }

    #[test]
    fn emits_across_many_tiny_chunks() {
        let input = r#"{"content":"hello world"}"#;
        let mut ex = JsonStringExtractor::new("content");
        let mut out = String::new();
        for ch in input.chars() {
            let buf = ch.to_string();
            out.push_str(&ex.feed(&buf));
        }
        assert_eq!(out, "hello world");
    }

    #[test]
    fn handles_multibyte_utf8() {
        assert_eq!(
            feed_all("content", &[r#"{"content":"xin chào"}"#]),
            "xin chào"
        );
    }

    #[test]
    fn multibyte_split_across_chunks() {
        // Splitting on char boundary (Rust `&str` guarantees char boundaries,
        // so this is the realistic case — the SSE parser delivers full chars).
        assert_eq!(
            feed_all("content", &[r#"{"content":"xin "#, r#"chào"}"#]),
            "xin chào"
        );
    }

    #[test]
    fn whitespace_around_tokens() {
        assert_eq!(
            feed_all("content", &[r#"{ "path" : "/a" , "content" : "body" }"#]),
            "body"
        );
    }

    #[test]
    fn primitive_values_before_target() {
        assert_eq!(
            feed_all("content", &[r#"{"count":42,"enabled":true,"content":"x"}"#]),
            "x"
        );
    }

    #[test]
    fn target_field_inside_nested_object_is_ignored() {
        // Only the top-level `content` counts.
        assert_eq!(
            feed_all(
                "content",
                &[r#"{"meta":{"content":"nested"},"content":"top"}"#]
            ),
            "top"
        );
    }

    #[test]
    fn key_with_escape() {
        // Key containing an escape: `"con\u0074ent"` == "content".
        assert_eq!(feed_all("content", &[r#"{"con\u0074ent":"x"}"#]), "x");
    }

    #[test]
    fn empty_string_value() {
        assert_eq!(feed_all("content", &[r#"{"content":""}"#]), "");
    }

    #[test]
    fn streamable_arg_lookup() {
        let tools = vec![
            ToolSchema {
                name: "Write".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
                streamable_arg: Some("content".into()),
            },
            ToolSchema {
                name: "Read".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
                streamable_arg: None,
            },
        ];
        assert_eq!(streamable_arg_for(&tools, "Write"), Some("content".into()));
        assert_eq!(streamable_arg_for(&tools, "Read"), None);
        assert_eq!(streamable_arg_for(&tools, "Unknown"), None);
    }
}
