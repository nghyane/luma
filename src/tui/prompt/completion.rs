/// Autocomplete — command dropdown, @file detection, file matching, rendering.
use crate::tui::text::{Line, Span};
use crate::tui::theme::palette;
use smallvec::smallvec;

const MAX_FILE_DEPTH: usize = 4;
const MAX_DROPDOWN_ITEMS: usize = 8;
const MAX_CANDIDATES: usize = 50;

/// A registered slash command.
pub struct Command {
    pub name: String,
    pub desc: String,
    pub visible: bool,
}

/// Autocomplete state for commands and @file.
pub struct Completion {
    pub commands: Vec<Command>,
    pub file_cache: Vec<String>,
    pub file_cache_valid: bool,
    pub dropdown_idx: usize,
}

impl Completion {
    /// Create empty completion state.
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            file_cache: Vec::new(),
            file_cache_valid: false,
            dropdown_idx: 0,
        }
    }

    /// Fuzzy match files against query.
    pub fn file_matches(&self, query: &str) -> Vec<String> {
        if !self.file_cache_valid {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut matches: Vec<&String> = self
            .file_cache
            .iter()
            .filter(|f| {
                if q.is_empty() {
                    return true;
                }
                let fl = f.to_lowercase();
                fl.starts_with(&q) || fl.contains(&q)
            })
            .take(MAX_CANDIDATES)
            .collect();
        matches.sort_by(|a, b| {
            let a_prefix = a.to_lowercase().starts_with(&q);
            let b_prefix = b.to_lowercase().starts_with(&q);
            b_prefix.cmp(&a_prefix).then(a.len().cmp(&b.len()))
        });
        matches
            .into_iter()
            .take(MAX_DROPDOWN_ITEMS)
            .cloned()
            .collect()
    }

    /// Refresh the file cache. Called when `@` is typed.
    pub fn refresh_file_cache(&mut self) {
        if self.file_cache_valid {
            return;
        }
        let mut files = Vec::new();
        let walker = ignore::WalkBuilder::new(".")
            .max_depth(Some(MAX_FILE_DEPTH))
            .hidden(true)
            .git_ignore(true)
            .build();
        for entry in walker.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let path = entry.path().strip_prefix("./").unwrap_or(entry.path());
                files.push(path.to_string_lossy().into_owned());
            }
        }
        files.sort();
        self.file_cache = files;
        self.file_cache_valid = true;
    }

    /// Get matching visible commands for a query.
    pub fn command_matches(&self, query: &str) -> Vec<&Command> {
        let q = query.to_lowercase();
        if q.is_empty() {
            self.commands.iter().filter(|c| c.visible).collect()
        } else {
            self.commands
                .iter()
                .filter(|c| c.visible && c.name.starts_with(&q))
                .collect()
        }
    }
}

/// Build a single dropdown line with accent bar and active highlight.
pub fn dropdown_line(
    bar: &str,
    label: &str,
    desc: &str,
    active: bool,
    fg: crate::tui::theme::Rgb,
) -> Line {
    let pad = crate::tui::theme::CONTENT_PAD;
    if active {
        Line {
            spans: smallvec![
                Span::deco(format!("{bar}  "), palette::ACCENT),
                Span::bold(label.to_owned(), fg),
                if desc.is_empty() {
                    Span::new(String::new(), palette::DIM)
                } else {
                    Span::new(desc.to_owned(), palette::DIM)
                },
            ],
            bg: Some(palette::SURFACE),
            margin: false,
            indent: 0,
            bleed: pad,
            deco: 0,
        }
    } else {
        Line {
            spans: smallvec![
                Span::deco(format!("{bar}  "), palette::BORDER),
                Span::new(label.to_owned(), palette::DIM),
                if desc.is_empty() {
                    Span::new(String::new(), palette::MUTED)
                } else {
                    Span::new(desc.to_owned(), palette::MUTED)
                },
            ],
            bg: None,
            margin: false,
            indent: 0,
            bleed: pad,
            deco: 0,
        }
    }
}

/// Split buffer into spans, highlighting @path references.
pub fn highlight_at_refs(buf: &str) -> smallvec::SmallVec<[Span; 4]> {
    let mut spans = smallvec::SmallVec::new();
    let mut last = 0;
    let bytes = buf.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            let at_start = i;
            i += 1;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let path = &buf[at_start + 1..i];
            if !path.is_empty() {
                if at_start > last {
                    spans.push(Span::new(buf[last..at_start].to_owned(), palette::FG));
                }
                spans.push(Span::new(buf[at_start..i].to_owned(), palette::FILE_REF));
                last = i;
            }
        } else {
            i += 1;
        }
    }
    if last < buf.len() {
        spans.push(Span::new(buf[last..].to_owned(), palette::FG));
    }
    if spans.is_empty() {
        spans.push(Span::new(buf.to_owned(), palette::FG));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_matches_empty_cache() {
        let comp = Completion::new();
        assert!(comp.file_matches("foo").is_empty());
    }

    #[test]
    fn file_matches_prefix_first() {
        let mut comp = Completion::new();
        comp.file_cache = vec![
            "src/main.rs".into(),
            "Cargo.toml".into(),
            "src/lib.rs".into(),
        ];
        comp.file_cache_valid = true;
        let m = comp.file_matches("src");
        assert!(!m.is_empty());
        assert!(m[0].starts_with("src"));
    }

    #[test]
    fn command_matches_all() {
        let mut comp = Completion::new();
        comp.commands.push(Command {
            name: "model".into(),
            desc: "switch".into(),
            visible: true,
        });
        comp.commands.push(Command {
            name: "new".into(),
            desc: "new thread".into(),
            visible: true,
        });
        assert_eq!(comp.command_matches("").len(), 2);
        assert_eq!(comp.command_matches("mo").len(), 1);
    }

    #[test]
    fn command_matches_hides_invisible() {
        let mut comp = Completion::new();
        comp.commands.push(Command {
            name: "resume".into(),
            desc: "resume".into(),
            visible: false,
        });
        comp.commands.push(Command {
            name: "new".into(),
            desc: "new thread".into(),
            visible: true,
        });
        assert_eq!(comp.command_matches("").len(), 1);
        assert_eq!(comp.command_matches("re").len(), 0);
    }

    #[test]
    fn highlight_plain() {
        let spans = highlight_at_refs("hello world");
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn highlight_with_file_ref() {
        let spans = highlight_at_refs("check @src/main.rs please");
        assert!(spans.len() >= 3);
        assert_eq!(spans[1].fg, palette::FILE_REF);
    }
}
