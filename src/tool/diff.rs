//! Unified diff generation for tool output display.
//!
//! Two entry points:
//! - `make_edit_diff` — O(n) context-based diff for edit tool (knows old/new strings + position)
//! - `make_diff` — Myers diff for write tool, O((n+m)d) where d = edit distance
//!
//! Output format per line: `{lineno:>w} {marker} {content}`
//! where marker is `+`, `-`, or ` `. Separator lines are `...`.
//! Renderer parses this back via `parse_diff_line`.

const CONTEXT_LINES: usize = 3;

/// Parsed diff line — used by renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub lineno: u32,
    pub text: String,
}

/// Diff line type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
    Context,
    Separator,
}

/// Parse a serialized diff line back into structured data.
pub fn parse_diff_line(raw: &str) -> DiffLine {
    if raw == "..." {
        return DiffLine {
            kind: DiffKind::Separator,
            lineno: 0,
            text: String::new(),
        };
    }
    // Format: `{lineno:>w} {marker} {content}`
    // Find first non-space digit sequence, then marker after space
    let trimmed = raw.trim_start();
    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let lineno: u32 = trimmed[..num_end].parse().unwrap_or(0);
    let rest = &trimmed[num_end..];

    // rest should be " + content" or " - content" or "   content"
    if rest.len() >= 3 {
        let marker = rest.as_bytes()[1];
        let content_start = 3.min(rest.len());
        let text = rest[content_start..].to_owned();
        let kind = match marker {
            b'+' => DiffKind::Add,
            b'-' => DiffKind::Del,
            _ => DiffKind::Context,
        };
        return DiffLine { kind, lineno, text };
    }

    DiffLine {
        kind: DiffKind::Context,
        lineno,
        text: rest.to_owned(),
    }
}

// ── Edit tool: context-based diff (no LCS needed) ──

/// Generate diff for edit tool — knows exact position in file.
/// `file_content` is the ORIGINAL file, `old_str`/`new_str` are the replacement pair.
/// `replace_all` controls single vs global replace.
pub fn make_edit_diff(
    file_content: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Vec<String> {
    let file_lines: Vec<&str> = file_content.lines().collect();
    let num_w = line_num_width(file_lines.len() + new_str.lines().count());
    let mut result = Vec::new();

    // Find all match positions (byte offsets)
    let positions: Vec<usize> = if replace_all {
        file_content
            .match_indices(old_str)
            .map(|(i, _)| i)
            .collect()
    } else {
        file_content.find(old_str).into_iter().collect()
    };

    for &byte_pos in &positions {
        // 0-based line index containing byte_pos
        let start_line = file_content[..byte_pos].matches('\n').count();

        let old_line_count = old_str.lines().count().max(1);
        let new_lines: Vec<&str> = new_str.lines().collect();

        // Context before
        let ctx_start = start_line.saturating_sub(CONTEXT_LINES);
        if ctx_start > 0
            && (result.is_empty() || result.last().is_some_and(|l: &String| l != "..."))
        {
            result.push("...".to_owned());
        }
        for i in ctx_start..start_line {
            if i < file_lines.len() {
                result.push(format!("{:>num_w$}   {}", i + 1, file_lines[i]));
            }
        }

        // Deleted lines
        for i in 0..old_line_count {
            let li = start_line + i;
            if li < file_lines.len() {
                result.push(format!("{:>num_w$} - {}", li + 1, file_lines[li]));
            }
        }

        // Added lines
        for (i, line) in new_lines.iter().enumerate() {
            result.push(format!("{:>num_w$} + {}", start_line + i + 1, line));
        }

        // Context after
        let after_start = (start_line + old_line_count).min(file_lines.len());
        let after_end = (after_start + CONTEXT_LINES).min(file_lines.len());
        for (i, line) in file_lines[after_start..after_end].iter().enumerate() {
            result.push(format!("{:>num_w$}   {}", after_start + i + 1, line));
        }
    }

    result
}

// ── Write tool: Myers diff ──

/// Generate full-file diff (for write tool — old vs new content).
pub fn make_diff(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let num_w = line_num_width(old_lines.len().max(new_lines.len()));

    if old.is_empty() {
        return new_lines
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>num_w$} + {l}", i + 1))
            .collect();
    }

    let edits = myers_diff(&old_lines, &new_lines);
    extract_hunks(&edits, &old_lines, &new_lines, num_w)
}

/// Count insertions and deletions from diff output lines.
pub fn diff_stats(diff: &[String]) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for line in diff {
        let dl = parse_diff_line(line);
        match dl.kind {
            DiffKind::Add => adds += 1,
            DiffKind::Del => dels += 1,
            _ => {}
        }
    }
    (adds, dels)
}

fn line_num_width(max: usize) -> usize {
    if max >= 10000 {
        5
    } else if max >= 1000 {
        4
    } else {
        3
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Keep,
    Delete,
    Insert,
}

/// Myers diff algorithm — O((n+m)d) time, O(n+m) space per d-step.
///
/// Finds a shortest edit script between `old` and `new` line slices.
/// `d` is the number of edits (inserts + deletes), so identical files
/// are O(n+m) and small changes in large files are near-linear.
fn myers_diff(old: &[&str], new: &[&str]) -> Vec<Edit> {
    let (n, m) = (old.len(), new.len());
    if n == 0 {
        return vec![Edit::Insert; m];
    }
    if m == 0 {
        return vec![Edit::Delete; n];
    }

    let max_d = n + m;
    let vsize = 2 * max_d + 1;
    let mut v = vec![0usize; vsize];
    // trace[d] = snapshot of v *after* processing d-step.
    let mut trace: Vec<Vec<usize>> = Vec::new();

    let idx = |k: isize| -> usize { (k + max_d as isize) as usize };

    let mut found_d = 0;
    'outer: for d in 0..=max_d {
        // Clone v before mutating so trace[d] captures state entering d.
        let snap = v.clone();
        for k in (-(d as isize)..=(d as isize)).step_by(2) {
            let mut x = if k == -(d as isize) || (k != d as isize && v[idx(k - 1)] < v[idx(k + 1)])
            {
                v[idx(k + 1)] // move down (insert)
            } else {
                v[idx(k - 1)] + 1 // move right (delete)
            };
            let mut y = (x as isize - k) as usize;

            // Follow diagonal (matching lines)
            while x < n && y < m && old[x] == new[y] {
                x += 1;
                y += 1;
            }
            v[idx(k)] = x;

            if x >= n && y >= m {
                found_d = d;
                // Push the snapshot that was taken *before* this d-round.
                trace.push(snap);
                break 'outer;
            }
        }
        trace.push(snap);
    }

    // Backtrack from (n,m) to (0,0) through the trace.
    let mut edits = Vec::with_capacity(n + m);
    let (mut x, mut y) = (n, m);

    for d in (1..=found_d).rev() {
        let k = x as isize - y as isize;
        let v_prev = &trace[d];

        let prev_k =
            if k == -(d as isize) || (k != d as isize && v_prev[idx(k - 1)] < v_prev[idx(k + 1)]) {
                k + 1 // came from insert (down)
            } else {
                k - 1 // came from delete (right)
            };
        let prev_x = v_prev[idx(prev_k)];
        let prev_y = (prev_x as isize - prev_k) as usize;

        // Diagonal (Keep) moves after the edit
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            edits.push(Edit::Keep);
        }

        if prev_k < k {
            // Moved right → delete from old
            x -= 1;
            edits.push(Edit::Delete);
        } else {
            // Moved down → insert from new
            y -= 1;
            edits.push(Edit::Insert);
        }
    }

    // Remaining diagonal from (0,0)
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        edits.push(Edit::Keep);
    }

    edits.reverse();
    edits
}

/// Extract context-windowed hunks with line numbers.
fn extract_hunks(edits: &[Edit], old: &[&str], new: &[&str], num_w: usize) -> Vec<String> {
    let mut changed = vec![false; edits.len()];
    for (i, e) in edits.iter().enumerate() {
        if *e != Edit::Keep {
            let start = i.saturating_sub(CONTEXT_LINES);
            let end = (i + CONTEXT_LINES + 1).min(edits.len());
            for c in &mut changed[start..end] {
                *c = true;
            }
        }
    }

    let mut result = Vec::new();
    let (mut oi, mut ni) = (0usize, 0usize);
    let mut in_hunk = false;

    for (i, edit) in edits.iter().enumerate() {
        if changed[i] {
            if !in_hunk && (oi > 0 || ni > 0) {
                result.push("...".to_owned());
            }
            in_hunk = true;
            match edit {
                Edit::Keep => {
                    result.push(format!("{:>num_w$}   {}", oi + 1, old[oi]));
                    oi += 1;
                    ni += 1;
                }
                Edit::Delete => {
                    result.push(format!("{:>num_w$} - {}", oi + 1, old[oi]));
                    oi += 1;
                }
                Edit::Insert => {
                    result.push(format!("{:>num_w$} + {}", ni + 1, new[ni]));
                    ni += 1;
                }
            }
        } else {
            in_hunk = false;
            match edit {
                Edit::Keep => {
                    oi += 1;
                    ni += 1;
                }
                Edit::Delete => {
                    oi += 1;
                }
                Edit::Insert => {
                    ni += 1;
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── make_diff (LCS) ──

    #[test]
    fn new_file() {
        let diff = make_diff("", "line1\nline2");
        assert_eq!(diff.len(), 2);
        assert!(diff[0].contains("+ line1"), "got: {}", diff[0]);
        assert!(diff[1].contains("+ line2"), "got: {}", diff[1]);
    }

    #[test]
    fn simple_edit() {
        let old = "a\nb\nc\nd";
        let new = "a\nB\nc\nd";
        let diff = make_diff(old, new);
        assert!(
            diff.iter().any(|l| l.contains(" - b")),
            "missing -b: {diff:?}"
        );
        assert!(
            diff.iter().any(|l| l.contains(" + B")),
            "missing +B: {diff:?}"
        );
    }

    #[test]
    fn no_changes() {
        let diff = make_diff("same\n", "same\n");
        assert!(diff.is_empty(), "no-op: {diff:?}");
    }

    #[test]
    fn large_file_context() {
        let old: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line10", "CHANGED");
        let diff = make_diff(&old, &new);
        assert!(diff.iter().any(|l| l == "..."), "missing ...: {diff:?}");
        assert!(diff.iter().any(|l| l.contains(" - line10")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + CHANGED")), "{diff:?}");
    }

    // ── make_edit_diff (context-based) ──

    #[test]
    fn edit_diff_single() {
        let file = "aaa\nbbb\nccc\nddd\neee";
        let diff = make_edit_diff(file, "bbb", "BBB", false);
        assert!(diff.iter().any(|l| l.contains(" - bbb")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + BBB")), "{diff:?}");
        // Context: aaa before, ccc+ddd+eee after
        assert!(
            diff.iter().any(|l| l.contains("aaa")),
            "ctx before: {diff:?}"
        );
        assert!(
            diff.iter().any(|l| l.contains("ccc")),
            "ctx after: {diff:?}"
        );
    }

    #[test]
    fn edit_diff_multiline() {
        let file = "a\nb\nc\nd\ne\nf";
        let diff = make_edit_diff(file, "b\nc", "X\nY\nZ", false);
        assert!(diff.iter().any(|l| l.contains(" - b")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" - c")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + X")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + Y")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + Z")), "{diff:?}");
    }

    #[test]
    fn edit_diff_at_start() {
        let file = "first\nsecond\nthird";
        let diff = make_edit_diff(file, "first", "FIRST", false);
        assert!(diff.iter().any(|l| l.contains(" - first")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + FIRST")), "{diff:?}");
        // No ... before (we're at line 1)
        assert!(
            !diff.iter().any(|l| l == "..."),
            "no separator at start: {diff:?}"
        );
    }

    #[test]
    fn edit_diff_at_end() {
        let file = "a\nb\nc\nd\nlast";
        let diff = make_edit_diff(file, "last", "LAST", false);
        assert!(diff.iter().any(|l| l.contains(" - last")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains(" + LAST")), "{diff:?}");
    }

    #[test]
    fn edit_diff_line_numbers() {
        let file = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
        let diff = make_edit_diff(file, "5", "FIVE", false);
        let del = diff.iter().find(|l| l.contains("- 5")).unwrap();
        let parsed = parse_diff_line(del);
        assert_eq!(parsed.lineno, 5, "line 5: {del}");
        assert_eq!(parsed.kind, DiffKind::Del);
        assert_eq!(parsed.text, "5");
    }

    // ── parse_diff_line ──

    #[test]
    fn parse_add() {
        let dl = parse_diff_line("  42 + fn main() {");
        assert_eq!(dl.kind, DiffKind::Add);
        assert_eq!(dl.lineno, 42);
        assert_eq!(dl.text, "fn main() {");
    }

    #[test]
    fn parse_del() {
        let dl = parse_diff_line(" 7 - old line");
        assert_eq!(dl.kind, DiffKind::Del);
        assert_eq!(dl.lineno, 7);
        assert_eq!(dl.text, "old line");
    }

    #[test]
    fn parse_context() {
        let dl = parse_diff_line("100   let x = 1;");
        assert_eq!(dl.kind, DiffKind::Context);
        assert_eq!(dl.lineno, 100);
        assert_eq!(dl.text, "let x = 1;");
    }

    #[test]
    fn parse_separator() {
        let dl = parse_diff_line("...");
        assert_eq!(dl.kind, DiffKind::Separator);
    }

    // ── Myers diff edge cases ──

    #[test]
    fn completely_different() {
        let diff = make_diff("a\nb\nc", "x\ny\nz");
        let dels: Vec<_> = diff.iter().filter(|l| l.contains(" - ")).collect();
        let adds: Vec<_> = diff.iter().filter(|l| l.contains(" + ")).collect();
        assert_eq!(dels.len(), 3, "3 deletes: {diff:?}");
        assert_eq!(adds.len(), 3, "3 inserts: {diff:?}");
    }

    #[test]
    fn insert_at_beginning() {
        let diff = make_diff("b\nc", "a\nb\nc");
        assert!(diff.iter().any(|l| l.contains(" + a")), "{diff:?}");
        assert!(
            !diff.iter().any(|l| l.contains(" - ")),
            "no deletes: {diff:?}"
        );
    }

    #[test]
    fn delete_at_end() {
        let diff = make_diff("a\nb\nc", "a\nb");
        assert!(diff.iter().any(|l| l.contains(" - c")), "{diff:?}");
        assert!(
            !diff.iter().any(|l| l.contains(" + ")),
            "no inserts: {diff:?}"
        );
    }

    #[test]
    fn identical_files() {
        let text = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = make_diff(&text, &text);
        assert!(diff.is_empty(), "identical: {diff:?}");
    }

    #[test]
    fn large_file_small_edit_performance() {
        // 5000 lines, single edit in the middle — should complete fast with Myers.
        let lines: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
        let old = lines.join("\n");
        let mut new_lines = lines.clone();
        new_lines[2500] = "CHANGED LINE".to_owned();
        let new = new_lines.join("\n");

        let start = std::time::Instant::now();
        let diff = make_diff(&old, &new);
        let elapsed = start.elapsed();

        assert!(diff.iter().any(|l| l.contains(" - line 2500")), "{diff:?}");
        assert!(
            diff.iter().any(|l| l.contains(" + CHANGED LINE")),
            "{diff:?}"
        );
        // With Myers O(nd) where d=2, this should be well under 100ms.
        // Old LCS O(n*m) would take seconds and ~100MB memory.
        assert!(
            elapsed.as_millis() < 500,
            "took {}ms — too slow for Myers",
            elapsed.as_millis()
        );
    }
}
