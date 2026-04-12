use super::diff::diff_line_lang;
use super::render::{RenderState, render_block};
use super::*;
use crate::core::types::{FileArtifact, FileChangeArtifact, FileOp, ToolStatus};
use crate::tui::stream::StreamBuf;
use crate::tui::text::ScreenBuffer;
use crate::tui::theme::palette;

fn diff_line(raw: &str) -> crate::tui::text::Line {
    diff_line_lang(raw, None)
}

#[test]
fn render_gap() {
    let mut st = RenderState::new();
    let lines = render_block(&Block::Gap, &mut st, 80, 0);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].visible_width(), 0);
}

#[test]
fn render_info() {
    let mut st = RenderState::new();
    let lines = render_block(&Block::Info("hello".into()), &mut st, 80, 0);
    assert!(!lines.is_empty());
}

#[test]
fn render_user_block() {
    let mut st = RenderState::new();
    let content = vec![crate::core::types::ContentBlock::Text { text: "hi".into() }];
    let lines = render_block(&Block::User(content), &mut st, 80, 0);
    assert!(lines.len() >= 3);
}

#[test]
fn render_tool_collapsed() {
    let mut tb = ToolBlock::history("Bash", "$ ls");
    tb.output = (0..20).map(|i| format!("line {i}")).collect();
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    assert!(lines.len() < 20);
}

#[test]
fn render_tool_expanded() {
    let mut tb = ToolBlock::history("Bash", "$ ls");
    tb.output = (0..20).map(|i| format!("line {i}")).collect();
    tb.is_expanded = true;
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    assert!(lines.len() >= 21);
}

#[test]
fn render_multi_file_change_uses_dim_file_headers() {
    let mut tb = ToolBlock::history("apply_patch", "hello.txt");
    tb.artifact = Some(crate::core::types::FileChangeArtifact {
        files: vec![
            crate::core::types::FileArtifact {
                path: "old.txt".into(),
                operation: crate::core::types::FileOp::Delete,
                diff: Some("-old".into()),
                preview: None,
            },
            crate::core::types::FileArtifact {
                path: "new.txt".into(),
                operation: crate::core::types::FileOp::Add,
                diff: Some("+new".into()),
                preview: None,
            },
        ],
        raw_input: None,
        error: None,
        status: crate::core::types::ToolStatus::Done,
    });
    tb.is_expanded = false;

    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(text.contains("Created new.txt"));
    assert!(text.contains("Deleted old.txt"));
    assert!(!text.contains("A new.txt"));
    assert!(!text.contains("D old.txt"));
}

#[test]
fn tool_pending_shows_spinner() {
    let tb = ToolBlock::streaming("Edit", "");
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(text.contains("Edit"), "should show tool name: {text}");
    assert!(
        text.contains("preparing Edit"),
        "write tool pending: {text}"
    );
}

#[test]
fn tool_pending_write_preview_caps_height_when_partial_exists() {
    let mut tb = ToolBlock::streaming("Write", "src/main.rs");
    let mut preview = StreamBuf::new();
    for i in 0..12 {
        preview.feed(&format!(
            "line {i}
"
        ));
    }
    preview.feed("partial");
    tb.arg_preview = Some(Box::new(preview));

    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);

    assert_eq!(lines.len(), 13, "header + capped preview height");
    let body: Vec<String> = lines[1..]
        .iter()
        .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
        .collect();
    assert_eq!(body.len(), 12);
    assert_eq!(body.last().map(String::as_str), Some("  partial"));
    // Oldest line is dropped to make room for the in-flight partial; all other
    // committed lines remain visible.
    assert!(!body.iter().any(|line| line == "  line 0"));
    assert!(body.iter().any(|line| line == "  line 11"));
}

#[test]
fn tool_inline_completed_read() {
    let mut tb = ToolBlock::history("Read", "src/main.rs");
    tb.end_summary = "(45 lines)".into();
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    assert_eq!(lines.len(), 1, "inline tool should be 1 line");
    let text: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
    assert!(text.contains("→"), "read tool icon: {text}");
    assert!(text.contains("src/main.rs"), "file path: {text}");
    assert!(text.contains("(45 lines)"), "end summary: {text}");
}

#[test]
fn tool_block_completed_write_with_diff() {
    let mut tb = ToolBlock::history("Write", "src/main.rs");
    tb.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some(
                [
                    "  1 + fn main() {",
                    "  2 +     println!(\"hello\");",
                    "  3 + }",
                ]
                .join("\n"),
            ),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    assert_eq!(lines.len(), 4, "block: title + 3 lines");
    let title: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
    assert!(title.contains("←"), "write icon in title: {title}");
    assert!(
        lines[1]
            .spans
            .iter()
            .any(|s| s.bg == Some(palette::DIFF_ADD_BG)),
        "missing add bg"
    );
}

#[test]
fn tool_write_no_output_uses_inline() {
    let tb = ToolBlock::history("Write", "src/main.rs");
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    assert_eq!(lines.len(), 1);
}

#[test]
fn tool_streaming_write_shows_content() {
    let mut arg_preview = StreamBuf::new();
    arg_preview.feed("  1 + fn main() {\n");
    arg_preview.feed("  2 +     println!(\"hi\");\n");
    arg_preview.feed("partial line");
    let tb = ToolBlock {
        name: "Write".into(),
        summary: "src/main.rs".into(),
        output: vec![
            "  1 + fn main() {".into(),
            "  2 +     println!(\"hi\");".into(),
        ],
        artifact: None,
        arg_preview: Some(Box::new(arg_preview)),
        stream: None,
        is_done: false,
        end_summary: String::new(),
        is_expanded: false,
    };
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(all.contains("fn"), "streaming content: {all}");
    assert!(all.contains("partial line"), "partial line: {all}");
}

#[test]
fn tool_block_completed_patch_uses_artifact() {
    let mut tb = ToolBlock::history("apply_patch", "src/main.rs");
    tb.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some("  1 - old\n  1 + new".into()),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(all.contains("old"), "missing removed line: {all}");
    assert!(all.contains("new"), "missing added line: {all}");
}

#[test]
fn tool_block_artifact_collapsed_shows_expand_hint() {
    let mut tb = ToolBlock::history("Write", "src/main.rs");
    tb.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some(
                (1..=20)
                    .map(|i| format!("  {i} + line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let title: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
    assert!(
        title.contains("click to expand"),
        "missing expand hint: {title}"
    );
    assert!(
        lines.len() < 21,
        "collapsed artifact should not show full diff"
    );
}

#[test]
fn tool_block_artifact_expanded_shows_full_diff() {
    let mut tb = ToolBlock::history("apply_patch", "src/main.rs");
    tb.is_expanded = true;
    tb.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some(
                (1..=20)
                    .map(|i| format!("  {i} + line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });
    let mut st = RenderState::new();
    let lines = render_block(&Block::Tool(tb), &mut st, 80, 0);
    let title: String = lines[0].spans.iter().map(|s| s.text.as_str()).collect();
    assert!(
        title.contains("click to collapse"),
        "missing collapse hint: {title}"
    );
    assert!(lines.len() >= 21, "expanded artifact should show full diff");
}

#[test]
fn diff_line_colors() {
    let add = diff_line("  1 + added");
    let del = diff_line("  2 - removed");
    let ctx = diff_line("  3   context");
    let sep = diff_line("...");
    assert!(
        add.spans.iter().any(|s| s.bg == Some(palette::DIFF_ADD_BG)),
        "add bg"
    );
    assert!(
        del.spans.iter().any(|s| s.bg == Some(palette::DIFF_DEL_BG)),
        "del bg"
    );
    assert!(ctx.spans.iter().all(|s| s.bg.is_none()), "ctx no bg");
    assert!(
        sep.spans.iter().any(|s| s.text.contains("...")),
        "separator"
    );
}

#[test]
fn diff_line_syntax_highlight() {
    let line = diff_line("  1 + fn main() {");
    let all: String = line.spans.iter().map(|s| s.text.as_str()).collect();
    assert!(all.contains("fn"), "content: {all}");
    let fn_span = line.spans.iter().find(|s| s.text == "fn");
    assert!(fn_span.is_some(), "fn span missing");
    assert_ne!(fn_span.unwrap().fg, palette::FG, "fn should be highlighted");
}

#[test]
fn diff_line_ansi_has_bg() {
    let line = diff_line("  1 + let x = 42;");
    let mut buf = ScreenBuffer::new(80, 1, palette::BG);
    buf.write_line(&line, 0, 0, 80);
    let ansi = buf.render_row(0);
    assert!(
        ansi.contains("48;2;30;50;30"),
        "missing add bg in ANSI:\n{ansi}"
    );
}

#[test]
fn full_edit_tool_flow() {
    use crate::tui::document::Document;
    use crate::tui::view::ViewState;

    let mut doc = Document::new();
    let mut view = ViewState::new(80, 30);

    doc.tool_selected("Edit");
    doc.tool_start("Edit", "test.rs");
    doc.tool_artifact(
        "Edit",
        FileChangeArtifact {
            files: vec![FileArtifact {
                path: "test.rs".into(),
                operation: FileOp::Update,
                diff: Some(["  1   aaa", "  2 - bbb", "  2 + BBB", "  3   ccc"].join("\n")),
                preview: None,
            }],
            raw_input: None,
            error: None,
            status: ToolStatus::Done,
        },
    );
    doc.tool_end("Edit", "");

    view.prepare_frame(doc.blocks());
    let vis = view.collect_visible();
    let all: String = vis
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();

    assert!(all.contains("bbb"), "missing old line:\n{all}");
    assert!(all.contains("BBB"), "missing new line:\n{all}");

    for l in &vis {
        let text: String = l.spans.iter().map(|s| s.text.as_str()).collect();
        if text.contains("bbb") {
            assert!(
                l.spans.iter().any(|s| s.bg == Some(palette::DIFF_DEL_BG)),
                "del line missing bg: {text}"
            );
        }
        if text.contains("BBB") {
            assert!(
                l.spans.iter().any(|s| s.bg == Some(palette::DIFF_ADD_BG)),
                "add line missing bg: {text}"
            );
        }
    }
}

#[test]
fn code_fence_then_header_with_emoji() {
    let input = "\
**Phase 2: Freemium Launch**
```
- Open source core
- Nếu traction tốt → expand
```

---

### 💡 **Unique Value Props (cần define rõ):**

Để kiếm tiền, cần trả lời:";

    let tb = text_block_from(input);
    let mut st = RenderState::new();
    let lines = render_block(&Block::Text(tb), &mut st, 80, 0);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();

    assert!(
        all.contains("Unique Value Props"),
        "missing 💡 line after code fence:\n{all}"
    );
}

#[test]
fn incremental_code_fence_streaming() {
    let tokens = [
        "**Phase 2:**\n",
        "```\n",
        "- Open source\n",
        "```\n",
        "\n",
        "---\n",
        "\n",
        "### 💡 **Unique Value Props:**\n",
        "\n",
        "Answer here.\n",
    ];

    let mut tb = TextBlock::new();
    let mut st = RenderState::new();
    for (i, tok) in tokens.iter().enumerate() {
        tb.stream.feed(tok);
        let r = render_block(&Block::Text(tb.clone()), &mut st, 80, 0);
        let text: String = r
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect();
        if i >= 7 {
            assert!(
                text.contains("Unique Value Props"),
                "after token {i}, missing Unique Value Props"
            );
        }
    }
}

#[test]
fn emoji_bold_line_renders() {
    let mut tb = TextBlock::new();
    tb.stream.feed("### Strategy\n\n");
    tb.stream.feed("1. First point\n");
    tb.stream
        .feed("2. 💡 **Unique Value Props (cần define rõ):**\n");
    tb.stream.feed("3. Third point\n");
    tb.stream.flush();

    let mut st1 = RenderState::new();
    let full = render_block(&Block::Text(tb.clone()), &mut st1, 80, 0);
    let mut st2 = RenderState::new();
    let incr = render_block(&Block::Text(tb), &mut st2, 80, 0);

    let full_text: String = full
        .iter()
        .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let incr_text: String = incr
        .iter()
        .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        full_text.contains("Unique Value Props"),
        "full missing:\n{full_text}"
    );
    assert!(
        incr_text.contains("Unique Value Props"),
        "incr missing:\n{incr_text}"
    );
    assert_eq!(full.len(), incr.len());
}

#[test]
fn incremental_matches_full() {
    let mut tb = TextBlock::new();
    tb.stream.feed("# Hello\nparagraph\n- item\n");
    tb.stream.flush();

    let mut st1 = RenderState::new();
    let full = render_block(&Block::Text(tb.clone()), &mut st1, 80, 0);
    let mut st2 = RenderState::new();
    let incr = render_block(&Block::Text(tb), &mut st2, 80, 0);

    assert_eq!(full.len(), incr.len());
    for (f, i) in full.iter().zip(incr.iter()) {
        let ft: String = f.spans.iter().map(|s| s.text.as_str()).collect();
        let it: String = i.spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(ft, it);
    }
}

#[test]
fn incremental_appends_only_new() {
    let mut tb = TextBlock::new();
    tb.stream.feed("line1\nline2\n");

    let mut st = RenderState::new();
    let _r1 = render_block(&Block::Text(tb.clone()), &mut st, 80, 0);

    tb.stream.feed("line3\n");
    let r2 = render_block(&Block::Text(tb.clone()), &mut st, 80, 0);

    let mut st_full = RenderState::new();
    let full = render_block(&Block::Text(tb), &mut st_full, 80, 0);
    assert_eq!(r2.len(), full.len());
}

#[test]
fn incremental_width_change_invalidates() {
    let mut tb = TextBlock::new();
    tb.stream.feed("hello world\n");

    let mut st = RenderState::new();
    let _ = render_block(&Block::Text(tb.clone()), &mut st, 80, 0);
    let _ = render_block(&Block::Text(tb), &mut st, 40, 0);
    // Width changed — should have re-parsed (no panic = success)
}

#[test]
fn incremental_code_fence() {
    let mut tb = TextBlock::new();
    tb.stream.feed("```rust\nlet x = 1;\n```\nhello\n");
    tb.stream.flush();

    let mut st1 = RenderState::new();
    let full = render_block(&Block::Text(tb.clone()), &mut st1, 80, 0);
    let mut st2 = RenderState::new();
    let incr = render_block(&Block::Text(tb), &mut st2, 80, 0);
    assert_eq!(full.len(), incr.len());
}

#[test]
fn incremental_with_partial() {
    let mut tb = TextBlock::new();
    tb.stream.feed("done\npartial text");

    let mut st = RenderState::new();
    let r = render_block(&Block::Text(tb), &mut st, 80, 0);
    let texts: Vec<String> = r
        .iter()
        .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
        .collect();
    assert!(texts.iter().any(|t| t.contains("done")));
    assert!(texts.iter().any(|t| t.contains("partial text")));
}

#[test]
fn block_kind_discriminants() {
    assert!(Block::Text(TextBlock::new()).is_content());
    assert!(Block::Tool(ToolBlock::history("Bash", "")).is_content());
    assert!(!Block::Gap.is_content());
    assert!(!Block::Info("x".into()).is_content());
}

#[test]
fn same_content_group_logic() {
    let thinking = Block::Thinking(StreamBuf::new());
    let text = Block::Text(TextBlock::new());
    let tool1 = Block::Tool(ToolBlock::history("Bash", ""));
    let tool2 = Block::Tool(ToolBlock::history("Read", ""));

    // Thinking → Text: same group (no gap)
    assert!(thinking.same_content_group(&text));
    // Tool → Tool: same group (no gap)
    assert!(tool1.same_content_group(&tool2));
    // Text → Tool: different group
    assert!(!text.same_content_group(&tool1));
    // Thinking → Tool: different group
    assert!(!thinking.same_content_group(&tool1));
}

#[test]
fn snapshot_tool_tracks_artifact_content() {
    let mut tb = ToolBlock::history("Write", "src/main.rs");
    tb.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some("  1 + hello".into()),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });

    match Block::Tool(tb).snapshot() {
        Snapshot::Tool { fingerprint } => {
            assert_ne!(fingerprint, 0);
        }
        _ => panic!("expected tool snapshot"),
    }
}

#[test]
fn snapshot_tool_changes_when_artifact_changes() {
    let mut a = ToolBlock::history("Write", "src/main.rs");
    a.artifact = Some(FileChangeArtifact {
        files: vec![FileArtifact {
            path: "src/main.rs".into(),
            operation: FileOp::Update,
            diff: Some("  1 + hello".into()),
            preview: None,
        }],
        raw_input: None,
        error: None,
        status: ToolStatus::Done,
    });

    let mut b = a.clone();
    if let Some(artifact) = &mut b.artifact {
        artifact.files[0].diff = Some("  1 + world".into());
    }

    assert!(Block::Tool(a).snapshot() != Block::Tool(b).snapshot());
}

#[test]
fn thinking_renders_with_markdown() {
    let mut stream = StreamBuf::new();
    stream.feed("Let me think about **bold** and `code`\n");
    stream.feed("More thinking\n");
    let mut st = RenderState::new();
    let lines = render_block(&Block::Thinking(stream), &mut st, 80, 0);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(all.contains("Thinking:"), "missing thinking prefix: {all}");
    assert!(all.contains("bold"), "missing bold text: {all}");
    assert!(all.contains("code"), "missing code text: {all}");
}

#[test]
fn strip_ansi_basic() {
    use super::diff::strip_ansi;
    let plain = strip_ansi("hello \x1b[31mred\x1b[0m world");
    assert_eq!(plain, "hello red world");
    let clean = strip_ansi("no escapes");
    assert_eq!(clean, "no escapes");
}

#[test]
fn screen_welcome_lines_independent_from_doc() {
    use crate::tui::document::Document;
    use crate::tui::view::ViewState;

    // Welcome lines are built independently — doc stays empty
    let mut doc = Document::new();
    let mut view = ViewState::new(80, 30);

    assert_eq!(doc.blocks().len(), 0, "doc should be empty during Welcome");

    // Transition to Chat: just start using doc, no clear needed
    doc.user_message(&[crate::core::types::ContentBlock::Text {
        text: "hello world".into(),
    }]);
    view.prepare_frame(doc.blocks());
    let text: String = view
        .collect_visible()
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(text.contains("hello world"), "user msg missing: {text}");
}

#[test]
fn user_block_renders_chips_for_attachments() {
    use crate::core::types::ContentBlock;
    let mut st = RenderState::new();
    let content = vec![
        ContentBlock::Text {
            text: "fix this:".into(),
        },
        ContentBlock::Image {
            media_type: "image/png".into(),
            id: "img_1.png".into(),
        },
        ContentBlock::Paste {
            text: "line1\nline2\nline3".into(),
        },
    ];
    let block = Block::User(content);
    let lines = render_block(&block, &mut st, 80, 0);
    let all: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
        .collect();
    assert!(all.contains("Image 1"), "image chip: {all}");
    assert!(all.contains("Pasted ~3 lines"), "paste chip: {all}");
    assert!(all.contains("fix this:"), "text content: {all}");
}

fn text_block_from(input: &str) -> TextBlock {
    let mut tb = TextBlock::new();
    tb.stream.feed(input);
    tb.stream.flush();
    tb
}
