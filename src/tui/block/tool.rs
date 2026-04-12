use super::ToolBlock;
/// Tool block rendering — pending, block (write/edit), search, inline.
use super::diff::{diff_line_lang, lang_from_path};
use crate::core::types::{FileArtifact, FileChangeArtifact, FileOp, ToolStatus};
use crate::tui::text::{Line, Span};
use crate::tui::theme::{icon, palette};
use smallvec::smallvec;

const TOOL_PREVIEW_LINES: usize = 12;
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "apply_patch"];
const SEARCH_TOOLS: &[&str] = &["web_search", "WebSearch"];

fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

fn is_search_tool(name: &str) -> bool {
    SEARCH_TOOLS.contains(&name)
}

fn tool_icon(name: &str) -> &'static str {
    if is_write_tool(name) {
        icon::TOOL_IN
    } else {
        icon::TOOL_OUT
    }
}

/// Render a tool block into lines.
pub fn render_tool(tb: &ToolBlock, content_w: usize, spinner_frame: usize) -> Vec<Line> {
    if !tb.is_done {
        return render_pending(tb, content_w, spinner_frame);
    }
    if let Some(artifact) = &tb.artifact {
        return render_file_change_block(tb, artifact, content_w);
    }
    if is_search_tool(&tb.name) {
        return render_search(tb, content_w);
    }
    render_inline(tb, content_w)
}

fn render_file_change_block(tb: &ToolBlock, artifact: &FileChangeArtifact, w: usize) -> Vec<Line> {
    let ic = tool_icon(&tb.name);
    let title = format!("{} {} {}", ic, tb.name, tb.summary);
    let mut h = smallvec![Span::new(title, palette::DIM)];
    push_end_summary(&mut h, tb);
    push_file_change_expand_hint(&mut h, tb, artifact);
    let mut result = crate::tui::text::wrap_line(&Line::new(h), w, None);

    match artifact.status {
        ToolStatus::Streaming => {
            if let Some(raw) = &artifact.raw_input {
                for line in raw.lines() {
                    result.push(Line::new(smallvec![
                        Span::new("  ".to_owned(), palette::DIM),
                        Span::new(line.to_owned(), palette::DIM),
                    ]));
                }
            }
        }
        ToolStatus::Failed => {
            if let Some(err) = &artifact.error {
                result.push(Line::new(smallvec![Span::new(err.clone(), palette::ERROR)]));
            }
        }
        ToolStatus::Done => {
            if artifact.files.len() == 1 {
                render_file_artifact(&artifact.files[0], tb.is_expanded, &mut result);
            } else {
                for file in &artifact.files {
                    result.push(Line::new(smallvec![
                        Span::new("  # ".to_owned(), palette::DIM),
                        Span::new(file_change_title(file), palette::DIM),
                    ]));
                    render_file_artifact(file, tb.is_expanded, &mut result);
                }
            }
        }
    }

    result
}

fn file_change_title(file: &FileArtifact) -> String {
    match &file.operation {
        FileOp::Add => format!("Created {}", file.path),
        FileOp::Update => format!("Updated {}", file.path),
        FileOp::Delete => format!("Deleted {}", file.path),
        FileOp::Move { from } => format!("Renamed {from} -> {}", file.path),
    }
}

fn render_file_artifact(file: &FileArtifact, is_expanded: bool, result: &mut Vec<Line>) {
    let lang = lang_from_path(&file.path);
    if let Some(diff) = &file.diff {
        for line in visible_tail_lines(diff, is_expanded) {
            result.push(diff_line_lang(line, lang));
        }
    } else if let Some(preview) = &file.preview {
        for line in visible_tail_lines(preview, is_expanded) {
            result.push(Line::new(smallvec![
                Span::new("  ".to_owned(), palette::DIM),
                Span::new(line.to_owned(), palette::DIM),
            ]));
        }
    }
}

fn push_file_change_expand_hint(
    h: &mut smallvec::SmallVec<[Span; 4]>,
    tb: &ToolBlock,
    artifact: &FileChangeArtifact,
) {
    let total_lines = artifact_line_count(artifact);
    let total_files = artifact.files.len();
    if total_files > 1 || total_lines > TOOL_PREVIEW_LINES {
        if tb.is_expanded {
            h.push(Span::new(" (click to collapse)".to_owned(), palette::MUTED));
        } else if total_files > 1 {
            h.push(Span::new(
                format!(" ({total_files} files · click to expand)"),
                palette::MUTED,
            ));
        } else {
            h.push(Span::new(
                format!(" ({total_lines} lines · click to expand)"),
                palette::MUTED,
            ));
        }
    }
}

fn artifact_line_count(artifact: &FileChangeArtifact) -> usize {
    artifact
        .files
        .iter()
        .map(|file| {
            file.diff
                .as_ref()
                .map(|text| text.lines().count())
                .or_else(|| file.preview.as_ref().map(|text| text.lines().count()))
                .unwrap_or(0)
        })
        .sum()
}

fn visible_tail_lines(text: &str, is_expanded: bool) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if is_expanded || lines.len() <= TOOL_PREVIEW_LINES {
        lines
    } else {
        lines[lines.len().saturating_sub(TOOL_PREVIEW_LINES)..].to_vec()
    }
}

fn render_pending(tb: &ToolBlock, w: usize, spinner_frame: usize) -> Vec<Line> {
    let spinner = icon::SPINNER[spinner_frame % icon::SPINNER.len()];
    let char_w = crate::tui::text::display_width(spinner);
    let pad = icon::SPINNER_WIDTH.saturating_sub(char_w);
    let has_content = !tb.output.is_empty()
        || tb.stream.as_ref().is_some_and(|s| !s.is_empty())
        || tb.arg_preview.as_ref().is_some_and(|s| !s.is_empty());

    let mut h = smallvec![Span::new(
        format!("{spinner}{}", " ".repeat(pad)),
        palette::ACCENT
    )];
    if has_content || !tb.summary.is_empty() {
        h.push(Span::bold(tb.name.clone(), palette::ACCENT));
        h.push(Span::new(format!(" {}", tb.summary), palette::DIM));
    } else {
        h.push(Span::new(
            format!("preparing {}...", tb.name),
            palette::MUTED,
        ));
    }
    let mut result = crate::tui::text::wrap_line(&Line::new(h), w, None);

    if is_write_tool(&tb.name) {
        render_pending_write(tb, &mut result);
    } else {
        render_pending_read(tb, &mut result);
    }
    result
}

fn render_pending_write(tb: &ToolBlock, result: &mut Vec<Line>) {
    let lang = lang_from_path(&tb.summary);
    for t in &tb.output {
        result.push(diff_line_lang(t, lang));
    }
    // Arg preview (streamed input) takes precedence — it carries the
    // content the user is watching get written. Output-phase stream only
    // matters for tools whose `ToolOutput` carries progress (not common
    // for Write/Edit/apply_patch).
    let active = tb.arg_preview.as_ref().or(tb.stream.as_ref());
    if let Some(stream) = active {
        let total = stream.committed.len();
        let start = total.saturating_sub(TOOL_PREVIEW_LINES);
        let mut visible: Vec<String> = stream.committed[start..].to_vec();
        if !stream.partial().is_empty() {
            if visible.len() < TOOL_PREVIEW_LINES {
                visible.push(stream.partial().to_owned());
            } else if let Some(last) = visible.last_mut() {
                *last = stream.partial().to_owned();
            }
        }
        for line in visible {
            result.push(Line::new(smallvec![
                Span::new("  ".to_owned(), palette::DIM),
                Span::new(line, palette::DIM),
            ]));
        }
    }
}

fn render_pending_read(tb: &ToolBlock, result: &mut Vec<Line>) {
    let total = tb.output.len();
    if total > 0 {
        for t in &tb.output[total.saturating_sub(TOOL_PREVIEW_LINES)..] {
            let color = if t.starts_with("... ") {
                palette::MUTED
            } else {
                palette::DIM
            };
            result.push(Line::new(smallvec![
                Span::new("  ".to_owned(), palette::DIM),
                Span::new(t.clone(), color),
            ]));
        }
    }
    if let Some(stream) = &tb.stream
        && !stream.partial().is_empty()
    {
        result.push(Line::new(smallvec![
            Span::new("  ".to_owned(), palette::DIM),
            Span::new(stream.partial().to_owned(), palette::DIM),
        ]));
    }
}

/// Completed search tool — query + numbered results.
fn render_search(tb: &ToolBlock, w: usize) -> Vec<Line> {
    let ic = tool_icon(&tb.name);
    let mut h = smallvec![
        Span::new(format!("{ic} "), palette::DIM),
        Span::new("Search".to_owned(), palette::DIM),
    ];
    if !tb.summary.is_empty() {
        h.push(Span::new(format!("  \"{}\"", tb.summary), palette::FG));
    }
    if !tb.end_summary.is_empty() {
        h.push(Span::new(format!("  {}", tb.end_summary), palette::MUTED));
    }
    let mut result = crate::tui::text::wrap_line(&Line::new(h), w, None);

    let mut idx = 0;
    let mut hit_num = 0;
    while idx < tb.output.len() {
        let title = tb.output[idx].trim();
        if title.is_empty() {
            idx += 1;
            continue;
        }
        hit_num += 1;
        let url = tb.output.get(idx + 1).map(|s| s.trim()).unwrap_or("");
        let snippet = if idx + 2 < tb.output.len() {
            let s = tb.output[idx + 2].trim();
            if s.is_empty() || s.starts_with("http") {
                ""
            } else {
                s
            }
        } else {
            ""
        };

        let domain = if url.is_empty() {
            String::new()
        } else {
            extract_domain(url)
        };

        let prefix_len = format!("  {hit_num}. ").len();
        let suffix = if domain.is_empty() {
            String::new()
        } else {
            format!(" -- {domain}")
        };
        let max_chars = w.saturating_sub(prefix_len + suffix.len());
        let char_count = title.chars().count();
        let display_title = if char_count > max_chars && max_chars > 3 {
            let truncated: String = title.chars().take(max_chars - 3).collect();
            format!("{truncated}...")
        } else {
            title.to_owned()
        };

        result.push(Line::new(smallvec![
            Span::new(format!("  {hit_num}. "), palette::MUTED),
            Span::new(display_title, palette::FG),
            Span::new(suffix, palette::MUTED),
        ]));

        idx += if snippet.is_empty() { 3 } else { 4 };
    }
    result
}

/// Completed non-write tool — inline line + optional output.
fn render_inline(tb: &ToolBlock, w: usize) -> Vec<Line> {
    let ic = tool_icon(&tb.name);
    let mut h = smallvec![
        Span::new(format!("{ic} "), palette::DIM),
        Span::new(tb.name.clone(), palette::DIM),
    ];
    if !tb.summary.is_empty() {
        h.push(Span::new(format!(" {}", tb.summary), palette::DIM));
    }
    push_end_summary(&mut h, tb);
    push_expand_hint(&mut h, tb);
    let mut result = crate::tui::text::wrap_line(&Line::new(h), w, None);

    let show = visible_output(tb);
    for t in show {
        let color = if t.starts_with("... ") {
            palette::MUTED
        } else {
            palette::DIM
        };
        result.push(Line::new(smallvec![
            Span::new("  ".to_owned(), palette::DIM),
            Span::new(t.clone(), color),
        ]));
    }
    result
}

fn push_end_summary(h: &mut smallvec::SmallVec<[Span; 4]>, tb: &ToolBlock) {
    if !tb.end_summary.is_empty() {
        let sc = if tb.end_summary.contains("exit") {
            palette::ERROR
        } else {
            palette::DIM
        };
        h.push(Span::new(format!(" {}", tb.end_summary), sc));
    }
}

fn push_expand_hint(h: &mut smallvec::SmallVec<[Span; 4]>, tb: &ToolBlock) {
    let total = tb.output.len();
    if total > TOOL_PREVIEW_LINES {
        if tb.is_expanded {
            h.push(Span::new(" (click to collapse)".to_owned(), palette::MUTED));
        } else {
            h.push(Span::new(
                format!(" ({total} lines · click to expand)"),
                palette::MUTED,
            ));
        }
    }
}

fn visible_output(tb: &ToolBlock) -> &[String] {
    let total = tb.output.len();
    if total == 0 {
        return &[];
    }
    if tb.is_expanded || total <= TOOL_PREVIEW_LINES {
        &tb.output[..]
    } else {
        &tb.output[total.saturating_sub(TOOL_PREVIEW_LINES)..]
    }
}

fn extract_domain(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_icon_write_vs_read() {
        assert_eq!(tool_icon("Write"), icon::TOOL_IN);
        assert_eq!(tool_icon("Edit"), icon::TOOL_IN);
        assert_eq!(tool_icon("Bash"), icon::TOOL_OUT);
        assert_eq!(tool_icon("Grep"), icon::TOOL_OUT);
        assert_eq!(tool_icon("web_search"), icon::TOOL_OUT);
    }
}
