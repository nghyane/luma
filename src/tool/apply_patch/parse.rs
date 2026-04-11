/// Patch parser — types, parsing, and fuzzy line matching.
use anyhow::Result;
use std::path::PathBuf;

use crate::core::types::FileOp;

#[derive(Debug, Clone)]
pub enum Hunk {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<Chunk>,
    },
}

impl Hunk {
    pub fn path(&self) -> &PathBuf {
        match self {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => path,
        }
    }

    pub fn operation(&self) -> FileOp {
        match self {
            Hunk::Add { .. } => FileOp::Add,
            Hunk::Delete { .. } => FileOp::Delete,
            Hunk::Update {
                path,
                move_to: Some(_),
                ..
            } => FileOp::Move {
                from: path.display().to_string(),
            },
            Hunk::Update { .. } => FileOp::Update,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Chunk {
    /// Context hint from `@@ fn main()` header — used to jump to the
    /// right scope before seeking the exact line sequence.
    pub context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_eof: bool,
}

/// Parse a patch string into a list of Hunks.
pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    if lines.is_empty() {
        anyhow::bail!("Empty patch");
    }
    let lines = strip_heredoc(&lines);

    if lines.first().is_none_or(|l| l.trim() != "*** Begin Patch") {
        anyhow::bail!("Patch must start with '*** Begin Patch'");
    }
    if lines.last().is_none_or(|l| l.trim() != "*** End Patch") {
        anyhow::bail!("Patch must end with '*** End Patch'");
    }

    let mut hunks = Vec::new();
    let inner = &lines[1..lines.len() - 1];
    let mut i = 0;

    while i < inner.len() {
        let line = inner[i].trim();

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let mut contents = String::new();
            i += 1;
            while i < inner.len() && inner[i].starts_with('+') {
                contents.push_str(&inner[i][1..]);
                contents.push('\n');
                i += 1;
            }
            hunks.push(Hunk::Add {
                path: PathBuf::from(path),
                contents,
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            hunks.push(Hunk::Delete {
                path: PathBuf::from(path),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if i < inner.len()
                && let Some(mp) = inner[i].trim().strip_prefix("*** Move to: ")
            {
                move_to = Some(PathBuf::from(mp));
                i += 1;
            }
            let chunks = parse_chunks(inner, &mut i)?;
            if chunks.is_empty() {
                anyhow::bail!("Update hunk for '{}' has no changes", path);
            }
            hunks.push(Hunk::Update {
                path: PathBuf::from(path),
                move_to,
                chunks,
            });
        } else if line.is_empty() {
            i += 1;
        } else {
            anyhow::bail!("Unexpected line in patch: '{line}'");
        }
    }
    Ok(hunks)
}

fn parse_chunks(inner: &[&str], i: &mut usize) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    while *i < inner.len() {
        let cl = inner[*i].trim();
        if cl.starts_with("*** ") {
            break;
        }

        let context = if cl == "@@" {
            *i += 1;
            None
        } else if let Some(ctx) = cl.strip_prefix("@@ ") {
            *i += 1;
            Some(ctx.to_owned())
        } else if cl.starts_with(' ') || cl.starts_with('+') || cl.starts_with('-') || cl.is_empty()
        {
            None
        } else {
            break;
        };

        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        let mut is_eof = false;

        while *i < inner.len() {
            let dl = inner[*i];
            let trimmed = dl.trim();
            if trimmed.starts_with("*** ") || trimmed == "@@" || trimmed.starts_with("@@ ") {
                break;
            }
            if trimmed == "*** End of File" {
                is_eof = true;
                *i += 1;
                break;
            }
            if dl.is_empty() {
                old_lines.push(String::new());
                new_lines.push(String::new());
            } else {
                match dl.as_bytes()[0] {
                    b' ' => {
                        old_lines.push(dl[1..].to_owned());
                        new_lines.push(dl[1..].to_owned());
                    }
                    b'-' => old_lines.push(dl[1..].to_owned()),
                    b'+' => new_lines.push(dl[1..].to_owned()),
                    _ => break,
                }
            }
            *i += 1;
        }

        if !old_lines.is_empty() || !new_lines.is_empty() {
            chunks.push(Chunk {
                context,
                old_lines,
                new_lines,
                is_eof,
            });
        }
    }
    Ok(chunks)
}

fn strip_heredoc<'a>(lines: &'a [&'a str]) -> Vec<&'a str> {
    if lines.len() >= 4 {
        let first = lines[0].trim();
        let last = lines[lines.len() - 1].trim();
        if (first == "<<EOF" || first == "<<'EOF'" || first == "<<\"EOF\"") && last == "EOF" {
            return lines[1..lines.len() - 1].to_vec();
        }
    }
    lines.to_vec()
}

/// Find the line containing context string (e.g. "fn main()") starting from `start`.
/// Returns the line index so `seek_sequence` can search from there.
pub fn seek_context(lines: &[String], context: &str, start: usize) -> Option<usize> {
    let ctx = context.trim();
    if ctx.is_empty() {
        return None;
    }
    // Exact substring match
    for (i, line) in lines.iter().enumerate().skip(start) {
        if line.contains(ctx) {
            return Some(i);
        }
    }
    // Normalized match (unicode, whitespace)
    let norm_ctx = normalise(ctx);
    for (i, line) in lines.iter().enumerate().skip(start) {
        if normalise(line).contains(&norm_ctx) {
            return Some(i);
        }
    }
    // Collapse internal whitespace so wrapped / padded context like
    // `fn                  lines_rendering() {` can still match
    // `fn lines_rendering() {` in the file.
    let collapsed_ctx = collapse_ws(&norm_ctx);
    for (i, line) in lines.iter().enumerate().skip(start) {
        if collapse_ws(&normalise(line)).contains(&collapsed_ctx) {
            return Some(i);
        }
    }
    None
}

/// Find pattern in lines starting from start, with fuzzy fallbacks.
pub fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };

    // Exact match
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // Trim trailing whitespace
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if pattern
            .iter()
            .enumerate()
            .all(|(j, p)| lines[i + j].trim_end() == p.trim_end())
        {
            return Some(i);
        }
    }
    // Trim both sides
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if pattern
            .iter()
            .enumerate()
            .all(|(j, p)| lines[i + j].trim() == p.trim())
        {
            return Some(i);
        }
    }
    // Normalize unicode
    (search_start..=lines.len().saturating_sub(pattern.len())).find(|&i| {
        pattern
            .iter()
            .enumerate()
            .all(|(j, p)| normalise(&lines[i + j]) == normalise(p))
    })
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_file() {
        let patch = "*** Begin Patch\n*** Add File: hello.txt\n+Hello\n+World\n*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            Hunk::Add { path, contents } => {
                assert_eq!(path, &PathBuf::from("hello.txt"));
                assert_eq!(contents, "Hello\nWorld\n");
            }
            _ => panic!("Expected Add"),
        }
    }

    #[test]
    fn parse_delete_file() {
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert!(matches!(&hunks[0], Hunk::Delete { .. }));
    }

    #[test]
    fn parse_update_file() {
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@ fn main()\n foo\n-bar\n+baz\n*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        match &hunks[0] {
            Hunk::Update { path, chunks, .. } => {
                assert_eq!(path, &PathBuf::from("src/main.rs"));
                assert_eq!(chunks[0].old_lines, vec!["foo", "bar"]);
                assert_eq!(chunks[0].new_lines, vec!["foo", "baz"]);
            }
            _ => panic!("Expected Update"),
        }
    }

    #[test]
    fn parse_heredoc_wrapper() {
        let patch = "<<'EOF'\n*** Begin Patch\n*** Delete File: x.txt\n*** End Patch\nEOF";
        assert_eq!(parse_patch(patch).unwrap().len(), 1);
    }

    #[test]
    fn parse_multi_ops() {
        let patch = "\
*** Begin Patch
*** Add File: new.py
+print('hi')
*** Delete File: old.py
*** Update File: main.py
@@
 import os
-import sys
+import json
*** End Patch";
        assert_eq!(parse_patch(patch).unwrap().len(), 3);
    }

    #[test]
    fn seek_exact() {
        let lines: Vec<String> = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(String::from)
            .collect();
        let pattern: Vec<String> = vec!["b", "c"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }

    #[test]
    fn seek_trimmed() {
        let lines: Vec<String> = vec!["a", "b  ", "c"]
            .into_iter()
            .map(String::from)
            .collect();
        let pattern: Vec<String> = vec!["b", "c"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }

    #[test]
    fn seek_eof() {
        let lines: Vec<String> = vec!["a", "b", "c"].into_iter().map(String::from).collect();
        let pattern: Vec<String> = vec!["b", "c"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, true), Some(1));
    }

    #[test]
    fn seek_context_jumps_to_function() {
        let lines: Vec<String> = vec![
            "fn first() {",
            "    1",
            "}",
            "",
            "fn second() {",
            "    2",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(seek_context(&lines, "fn second()", 0), Some(4));
        assert_eq!(seek_context(&lines, "fn first()", 0), Some(0));
        assert_eq!(seek_context(&lines, "fn nonexistent()", 0), None);
    }

    #[test]
    fn seek_context_collapses_internal_whitespace() {
        let lines: Vec<String> = vec!["fn lines_rendering() {", "}"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            seek_context(&lines, "fn                  lines_rendering() {", 0),
            Some(0)
        );
    }

    #[test]
    fn seek_not_found() {
        let lines: Vec<String> = vec!["a", "b"].into_iter().map(String::from).collect();
        let pattern: Vec<String> = vec!["x", "y"].into_iter().map(String::from).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), None);
    }
}
