/// Conversation document — owns blocks, mutation methods, gap rules.
/// No view imports. No scroll, layout, or renderer knowledge.
use crate::core::types::ContentBlock;
use crate::tui::block::diff::strip_ansi;
use crate::tui::block::{Block, SkillBlock, TextBlock, ToolBlock};
use crate::tui::stream::StreamBuf;

pub struct Document {
    blocks: Vec<Block>,
}

impl Document {
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Read-only access to blocks (for Layout rendering).
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Whether the document contains any user message.
    pub fn has_user_content(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::User(_)))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    // ── Content push ──

    /// Push a user message from structured content blocks.
    pub fn user_message(&mut self, content: &[ContentBlock]) {
        self.commit_last();
        if !matches!(self.blocks.last(), Some(Block::Gap)) {
            self.blocks.push(Block::Gap);
        }
        self.blocks.push(Block::User(content.to_vec()));
        self.blocks.push(Block::Gap);
    }

    pub fn append_thinking(&mut self, token: &str) {
        if !matches!(self.blocks.last(), Some(Block::Thinking(_))) {
            self.commit_last();
            self.auto_gap(&Block::Thinking(StreamBuf::new()));
            self.blocks.push(Block::Thinking(StreamBuf::new()));
        }
        self.feed_last(token);
    }

    pub fn append_token(&mut self, token: &str) {
        if matches!(self.blocks.last(), Some(Block::Text(_))) {
            self.feed_last(token);
            return;
        }
        self.commit_last();
        self.auto_gap(&Block::Text(TextBlock::new()));
        self.blocks.push(Block::Text(TextBlock::new()));
        self.feed_last(token);
    }

    pub fn info(&mut self, text: &str) {
        self.commit_last();
        self.blocks.push(Block::Info(text.to_owned()));
    }

    pub fn assistant_message(&mut self, text: &str) {
        let trimmed = text.trim_start_matches('\n');
        if trimmed.is_empty() {
            return;
        }
        self.commit_last();
        self.auto_gap(&Block::Text(TextBlock::new()));
        self.blocks.push(Block::Text(TextBlock {
            stream: StreamBuf::finished(trimmed),
        }));
    }

    /// Replay a saved message into the document — single entry point for
    /// history rendering.  Produces the same Block types that the live
    /// stream path creates, so resume and chat look identical.
    pub fn replay_message(&mut self, msg: &crate::core::types::Message) {
        use crate::core::types::{ContentBlock, Role};

        match msg.role {
            Role::System => {}
            Role::User => {
                if !msg.has_visible_content() {
                    return;
                }
                self.user_message(&msg.content);
            }
            Role::Assistant => {
                let mut text_buf = String::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            if !text_buf.is_empty() {
                                self.assistant_message(&text_buf);
                                text_buf.clear();
                            }
                            self.replay_thinking(thinking);
                        }
                        ContentBlock::Text { text } | ContentBlock::Paste { text }
                            if !text.is_empty() =>
                        {
                            if !text_buf.is_empty() {
                                text_buf.push('\n');
                            }
                            text_buf.push_str(text);
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            if !text_buf.is_empty() {
                                self.assistant_message(&text_buf);
                                text_buf.clear();
                            }
                            let summary = crate::core::agent::format_tool_summary(name, input);
                            let artifact = reconstruct_artifact(name, input);
                            self.replay_tool(name, &summary, artifact);
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::Text { .. }
                        | ContentBlock::Paste { .. }
                        | ContentBlock::Thinking { .. } => {}
                    }
                }

                if !text_buf.is_empty() {
                    self.assistant_message(&text_buf);
                }
            }
        }
    }

    /// Replay a completed tool — with artifact if reconstructable.
    fn replay_tool(
        &mut self,
        name: &str,
        summary: &str,
        artifact: Option<crate::core::types::FileChangeArtifact>,
    ) {
        self.commit_last();
        let mut tb = ToolBlock::history(name, summary);
        if let Some(art) = artifact {
            tb.artifact = Some(art);
        }
        let block = Block::Tool(tb);
        self.auto_gap(&block);
        self.blocks.push(block);
    }

    /// Push a finished thinking block (for history replay).
    fn replay_thinking(&mut self, text: &str) {
        self.commit_last();
        self.auto_gap(&Block::Thinking(StreamBuf::new()));
        self.blocks.push(Block::Thinking(StreamBuf::finished(text)));
    }

    pub fn error(&mut self, text: &str) {
        self.commit_last();
        self.blocks.push(Block::Error(text.to_owned()));
    }

    pub fn warn(&mut self, text: &str) {
        self.commit_last();
        self.blocks.push(Block::Warn(text.to_owned()));
    }

    pub fn provider_retry(
        &mut self,
        provider: &str,
        delay_secs: u64,
        attempt: u8,
        max_attempts: u8,
    ) {
        // Any tool block left pending from the failed attempt is now
        // stale — the provider is going to re-stream from scratch and
        // may issue a different tool call. Finalise them so the UI
        // doesn't carry a "preparing..." ghost into the next attempt.
        self.close_pending("retry");
        self.blocks.push(Block::Warn(format!(
            "{provider} temporary throttling — retrying in {delay_secs}s (attempt {attempt}/{max_attempts})"
        )));
    }

    pub fn divider(&mut self) {
        self.commit_last();
        self.blocks.push(Block::Gap);
    }

    pub fn divider_with_label(&mut self, label: &str) {
        self.commit_last();
        self.blocks.push(Block::Gap);
        self.blocks.push(Block::GapLabel(label.to_owned()));
    }

    // ── Tool lifecycle ──

    /// Create a pending tool block as soon as the provider starts a tool_use
    /// stream. Called before [`Self::tool_input`] / [`Self::tool_start`]
    /// reach the document, so the UI can show a spinner during the gap
    /// between tool selection and the first streamable-arg delta.
    ///
    /// If an active (not-done) block with the same name already exists,
    /// this is a no-op — the provider may re-announce the tool on retry,
    /// and we want to keep any partial args already collected.
    pub fn tool_selected(&mut self, name: &str) {
        if self.find_active_tool_mut(name).is_some() {
            return;
        }
        self.commit_last();
        let block = Block::Tool(ToolBlock::streaming(name, ""));
        self.auto_gap(&block);
        self.blocks.push(block);
    }

    /// Announce that a tool is about to execute with resolved arguments.
    /// Called by the agent turn loop once the provider's tool call is
    /// complete. Upgrades the summary of any existing pending block and
    /// initializes the output-phase stream buffer. Does **not** touch
    /// `arg_preview` — the streamed arg content must remain visible.
    pub fn tool_start(&mut self, name: &str, summary: &str) {
        if let Some(tb) = self.find_active_tool_mut(name) {
            tb.summary = summary.to_owned();
            tb.stream = Some(Box::new(StreamBuf::new()));
            return;
        }
        self.commit_last();
        let mut block = ToolBlock::streaming(name, summary);
        // No prior `tool_selected` — this is a non-streamable tool (no arg
        // preview, but needs an output stream).
        block.arg_preview = None;
        block.stream = Some(Box::new(StreamBuf::new()));
        let block = Block::Tool(block);
        self.auto_gap(&block);
        self.blocks.push(block);
    }

    /// Feed streamed argument characters into the active tool block's
    /// arg preview buffer. Called while the provider is still delivering
    /// the tool's input JSON.
    pub fn tool_input(&mut self, name: &str, chunk: &str) {
        if let Some(tb) = self.find_active_tool_mut(name)
            && let Some(preview) = &mut tb.arg_preview
        {
            preview.feed(chunk);
        }
    }

    pub fn tool_output(&mut self, name: &str, chunk: &str) {
        if let Some(tb) = self.find_active_tool_mut(name)
            && let Some(stream) = &mut tb.stream
        {
            stream.feed(chunk);
            for line in stream.committed.drain(..) {
                tb.output.push(strip_ansi(&line));
            }
        }
    }

    pub fn tool_artifact(&mut self, name: &str, artifact: crate::core::types::FileChangeArtifact) {
        if let Some(tb) = self.find_active_tool_mut(name) {
            tb.artifact = Some(artifact);
        } else if let Some(tb) = self.blocks.iter_mut().rev().find_map(|b| {
            if let Block::Tool(tb) = b
                && tb.name == name
            {
                Some(tb)
            } else {
                None
            }
        }) {
            tb.artifact = Some(artifact);
        }
    }

    pub fn tool_end(&mut self, name: &str, summary: &str) {
        self.commit_last();
        if let Some(tb) = self.find_active_tool_mut(name) {
            tb.is_done = true;
            tb.end_summary = summary.to_owned();
            if tb.artifact.is_some() {
                tb.output.clear();
            }
            tb.arg_preview = None;
            tb.stream = None;
        }
    }

    // ── Skill lifecycle ──

    pub fn skill_start(&mut self, name: &str) {
        self.commit_last();
        let block = Block::Skill(SkillBlock {
            name: name.to_owned(),
            is_done: false,
            end_summary: String::new(),
        });
        self.auto_gap(&block);
        self.blocks.push(block);
    }

    pub fn skill_end(&mut self, summary: &str) {
        self.commit_last();
        if let Some(Block::Skill(sb)) = self.blocks.last_mut()
            && !sb.is_done
        {
            sb.is_done = true;
            sb.end_summary = summary.to_owned();
        }
    }

    // ── State control ──

    /// Finalise every tool/skill block that never received a matching
    /// `tool_end` / `skill_end`. Called on abort, agent completion, or
    /// agent error to prevent pending blocks from staying in the
    /// "preparing..." state forever.
    ///
    /// Unlike [`Self::abort`], this scans the entire document — pending
    /// blocks can be left behind anywhere if a provider retry discards a
    /// tool_use in the middle of a turn.
    pub fn close_pending(&mut self, end_summary: &str) {
        self.commit_last();
        for block in self.blocks.iter_mut() {
            match block {
                Block::Tool(tb) if !tb.is_done => {
                    tb.is_done = true;
                    if tb.end_summary.is_empty() {
                        tb.end_summary = end_summary.to_owned();
                    }
                    tb.stream = None;
                }
                Block::Skill(sb) if !sb.is_done => {
                    sb.is_done = true;
                    if sb.end_summary.is_empty() {
                        sb.end_summary = end_summary.to_owned();
                    }
                }
                _ => {}
            }
        }
    }

    pub fn abort(&mut self) {
        self.close_pending("aborted");
    }

    pub fn newline(&mut self) {
        self.commit_last();
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    pub fn toggle_expand(&mut self, idx: usize) -> bool {
        if let Some(Block::Tool(tb)) = self.blocks.get_mut(idx) {
            let artifact_expandable = tb.artifact.as_ref().is_some_and(|artifact| {
                let file_count = artifact.files.len();
                let line_count: usize = artifact
                    .files
                    .iter()
                    .map(|file| {
                        file.diff
                            .as_ref()
                            .map(|text| text.lines().count())
                            .or_else(|| file.preview.as_ref().map(|text| text.lines().count()))
                            .unwrap_or(0)
                    })
                    .sum();
                file_count > 1 || line_count > 4
            });
            if !tb.is_done || (tb.output.len() <= 4 && !artifact_expandable) {
                return false;
            }
            tb.is_expanded = !tb.is_expanded;
            return true;
        }
        false
    }

    // ── Private ──

    fn auto_gap(&mut self, new_block: &Block) {
        if let Some(last) = self.blocks.last()
            && last.is_content()
            && new_block.is_content()
            && !last.same_content_group(new_block)
        {
            self.blocks.push(Block::Gap);
        }
    }

    fn feed_last(&mut self, token: &str) {
        match self.blocks.last_mut() {
            Some(Block::Thinking(s)) => s.feed(token),
            Some(Block::Text(tb)) => tb.feed(token),
            _ => {}
        }
    }

    fn commit_last(&mut self) {
        match self.blocks.last_mut() {
            Some(Block::Thinking(s)) if !s.is_empty() => s.flush(),
            Some(Block::Text(tb)) if !tb.is_empty() => tb.flush(),
            Some(Block::Tool(tb)) if !tb.is_done => {
                if let Some(stream) = &mut tb.stream {
                    stream.flush();
                    tb.output.append(&mut stream.committed);
                    tb.stream = None;
                }
            }
            _ => {}
        }
    }

    fn find_active_tool_mut(&mut self, name: &str) -> Option<&mut ToolBlock> {
        self.blocks.iter_mut().rev().find_map(|b| {
            if let Block::Tool(tb) = b
                && tb.name == name
                && !tb.is_done
            {
                Some(tb)
            } else {
                None
            }
        })
    }
}

/// Reconstruct a `FileChangeArtifact` from a ToolUse's input args.
///
/// For Edit/Write tools the ToolUse block already carries `path`,
/// `old_string`, `new_string` — enough to derive the diff without
/// reading the filesystem or persisting extra data.
fn reconstruct_artifact(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<crate::core::types::FileChangeArtifact> {
    use crate::core::types::{FileArtifact, FileChangeArtifact, FileOp, ToolStatus};

    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path.is_empty() {
        return None;
    }

    match tool_name {
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if old.is_empty() && new.is_empty() {
                return None;
            }
            let (op, diff) = if old.is_empty() {
                // Create new file — show all as added.
                (FileOp::Add, crate::tool::diff::make_diff("", new))
            } else {
                // Edit — pure old→new diff (no file context, but shows the change).
                (FileOp::Update, crate::tool::diff::make_diff(old, new))
            };
            Some(FileChangeArtifact {
                files: vec![FileArtifact {
                    path: path.to_owned(),
                    operation: op,
                    diff: Some(diff.join("\n")),
                    preview: None,
                }],
                raw_input: None,
                error: None,
                status: ToolStatus::Done,
            })
        }
        "Write" => {
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            // Write creates the full file — show first ~20 lines as preview.
            let preview: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
            let truncated = content.lines().count() > 20;
            let display = if truncated {
                format!(
                    "{preview}\n... ({} more lines)",
                    content.lines().count() - 20
                )
            } else {
                preview
            };
            Some(FileChangeArtifact {
                files: vec![FileArtifact {
                    path: path.to_owned(),
                    operation: FileOp::Add,
                    diff: None,
                    preview: Some(display),
                }],
                raw_input: None,
                error: None,
                status: ToolStatus::Done,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_content(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: s.to_owned() }]
    }

    #[test]
    fn user_message_has_gap_around() {
        let mut doc = Document::new();
        doc.user_message(&text_content("hi"));
        assert!(matches!(&doc.blocks[0], Block::Gap));
        assert!(matches!(&doc.blocks[1], Block::User(_)));
        assert!(matches!(&doc.blocks[2], Block::Gap));
    }

    #[test]
    fn auto_gap_between_content_groups() {
        let mut doc = Document::new();
        doc.append_thinking("hmm");
        doc.newline();
        doc.tool_start("Bash", "$ ls");
        let has_gap = doc
            .blocks
            .windows(2)
            .any(|w| matches!(&w[0], Block::Gap) && matches!(&w[1], Block::Tool(_)));
        assert!(has_gap, "missing gap between Thinking and Tool");
    }

    #[test]
    fn gap_thinking_to_text() {
        let mut doc = Document::new();
        doc.append_thinking("hmm\n");
        doc.append_token("answer");
        let has_gap = doc
            .blocks
            .windows(2)
            .any(|w| matches!(&w[0], Block::Gap) && matches!(&w[1], Block::Text(_)));
        assert!(has_gap, "expected gap between Thinking and Text");
    }

    #[test]
    fn tool_full_lifecycle() {
        let mut doc = Document::new();
        doc.tool_start("Bash", "$ ls");
        doc.tool_output("Bash", "file1\nfile2\n");
        doc.tool_end("Bash", "exit 0");
        let tb = doc
            .blocks
            .iter()
            .find_map(|b| {
                if let Block::Tool(tb) = b {
                    Some(tb)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(tb.is_done);
        assert_eq!(tb.output, vec!["file1", "file2"]);
        assert_eq!(tb.end_summary, "exit 0");
    }

    #[test]
    fn write_streaming_lifecycle() {
        // Simulate the Claude Write flow: provider emits ToolSelected ->
        // ToolInput chunks (arg preview) -> orchestrator emits ToolStart
        // with path -> ToolArtifact -> ToolEnd.
        let mut doc = Document::new();

        doc.tool_selected("Write");
        let tb = active_tool(&doc, "Write").unwrap();
        assert!(!tb.is_done);
        assert!(tb.summary.is_empty());
        assert!(tb.arg_preview.as_ref().is_some_and(|s| s.is_empty()));
        assert!(tb.stream.is_none());

        doc.tool_input("Write", "line one\n");
        doc.tool_input("Write", "line two\n");
        doc.tool_input("Write", "parti");
        let tb = active_tool(&doc, "Write").unwrap();
        let preview = tb.arg_preview.as_ref().unwrap();
        assert_eq!(preview.committed, vec!["line one", "line two"]);
        assert_eq!(preview.partial(), "parti");

        // Orchestrator resolves the call and starts execution. arg_preview
        // survives (user keeps seeing content); stream is freshly created
        // for output phase.
        doc.tool_start("Write", "/tmp/foo.txt");
        let tb = active_tool(&doc, "Write").unwrap();
        assert_eq!(tb.summary, "/tmp/foo.txt");
        let preview = tb.arg_preview.as_ref().expect("preview preserved");
        assert_eq!(preview.committed, vec!["line one", "line two"]);
        assert!(tb.stream.as_ref().is_some_and(|s| s.is_empty()));

        doc.tool_artifact(
            "Write",
            crate::core::types::FileChangeArtifact {
                files: vec![crate::core::types::FileArtifact {
                    path: "/tmp/foo.txt".into(),
                    operation: crate::core::types::FileOp::Add,
                    diff: Some("  1 + line one\n  2 + line two".into()),
                    preview: None,
                }],
                raw_input: None,
                error: None,
                status: crate::core::types::ToolStatus::Done,
            },
        );
        doc.tool_end("Write", "Created /tmp/foo.txt");

        let tb = doc
            .blocks
            .iter()
            .find_map(|b| {
                if let Block::Tool(tb) = b {
                    Some(tb)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(tb.is_done);
        assert!(tb.artifact.is_some());
        assert_eq!(tb.end_summary, "Created /tmp/foo.txt");
        // Both buffers released on tool_end.
        assert!(tb.arg_preview.is_none());
        assert!(tb.stream.is_none());
    }

    #[test]
    fn tool_artifact_is_attached_to_active_tool() {
        let mut doc = Document::new();
        doc.tool_start("apply_patch", "src/main.rs");
        doc.tool_artifact(
            "apply_patch",
            crate::core::types::FileChangeArtifact {
                files: vec![crate::core::types::FileArtifact {
                    path: "src/main.rs".into(),
                    operation: crate::core::types::FileOp::Update,
                    diff: Some("  1 - old\n  1 + new".into()),
                    preview: None,
                }],
                raw_input: None,
                error: None,
                status: crate::core::types::ToolStatus::Done,
            },
        );

        let tb = active_tool(&doc, "apply_patch").unwrap();
        assert!(tb.artifact.is_some());
        assert!(tb.output.is_empty());
    }

    #[test]
    fn tool_artifact_keeps_streamed_output_until_end() {
        let mut doc = Document::new();
        doc.tool_start("Write", "src/main.rs");
        doc.tool_output("Write", "  1 + hello\n");
        doc.tool_artifact(
            "Write",
            crate::core::types::FileChangeArtifact {
                files: vec![crate::core::types::FileArtifact {
                    path: "src/main.rs".into(),
                    operation: crate::core::types::FileOp::Update,
                    diff: Some("  1 + hello".into()),
                    preview: None,
                }],
                raw_input: None,
                error: None,
                status: crate::core::types::ToolStatus::Done,
            },
        );

        let tb = active_tool(&doc, "Write").unwrap();
        assert_eq!(tb.output, vec!["  1 + hello"]);

        doc.tool_end("Write", "done");
        let tb = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Tool(tb) if tb.name == "Write" => Some(tb),
                _ => None,
            })
            .unwrap();
        assert!(tb.output.is_empty());
    }

    /// Regression: tool_start must not wipe arg_preview. Previously it
    /// reset `stream` after the provider had already streamed the full
    /// Write/apply_patch content into it, causing the content to appear
    /// as a single block only at tool_end.
    #[test]
    fn tool_start_preserves_streamed_arg_preview() {
        let mut doc = Document::new();
        doc.tool_selected("Write");
        doc.tool_input("Write", "fn main() {\n");
        doc.tool_input("Write", "    println!(\"hi\");\n");
        doc.tool_input("Write", "partial");

        // tool_start fires once the provider finishes the tool_use block.
        doc.tool_start("Write", "src/main.rs");

        let tb = active_tool(&doc, "Write").unwrap();
        let preview = tb.arg_preview.as_ref().expect("arg_preview survives");
        let seen: String = preview
            .committed
            .iter()
            .cloned()
            .chain(std::iter::once(preview.partial().to_owned()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(seen.contains("fn main()"), "lost streamed content: {seen}");
        assert!(seen.contains("partial"), "lost partial: {seen}");
    }

    #[test]
    fn toggle_expand_works_for_file_change_artifact() {
        let mut doc = Document::new();
        doc.tool_start("Write", "src/main.rs");
        doc.tool_artifact(
            "Write",
            crate::core::types::FileChangeArtifact {
                files: vec![crate::core::types::FileArtifact {
                    path: "src/main.rs".into(),
                    operation: crate::core::types::FileOp::Update,
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
                status: crate::core::types::ToolStatus::Done,
            },
        );
        doc.tool_end("Write", "");

        let idx = doc
            .blocks
            .iter()
            .position(|b| matches!(b, Block::Tool(_)))
            .unwrap();
        assert!(doc.toggle_expand(idx));
    }

    fn active_tool<'a>(doc: &'a Document, name: &str) -> Option<&'a crate::tui::block::ToolBlock> {
        doc.blocks.iter().rev().find_map(|b| {
            if let Block::Tool(tb) = b
                && tb.name == name
                && !tb.is_done
            {
                Some(tb)
            } else {
                None
            }
        })
    }

    #[test]
    fn toggle_expand() {
        let mut doc = Document::new();
        doc.tool_start("Bash", "$ ls");
        for i in 0..20 {
            doc.tool_output("Bash", &format!("line{i}\n"));
        }
        doc.tool_end("Bash", "");
        let idx = doc
            .blocks
            .iter()
            .position(|b| matches!(b, Block::Tool(_)))
            .unwrap();
        assert!(doc.toggle_expand(idx));
    }

    #[test]
    fn clear_resets() {
        let mut doc = Document::new();
        doc.info("hello");
        doc.clear();
        assert_eq!(doc.len(), 0);
    }

    #[test]
    fn abort_finalizes_tool() {
        let mut doc = Document::new();
        doc.tool_start("Bash", "$ ls");
        doc.abort();
        let tb = doc
            .blocks
            .iter()
            .find_map(|b| {
                if let Block::Tool(tb) = b {
                    Some(tb)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(tb.is_done);
    }

    #[test]
    fn close_pending_finalises_all_unfinished_tools() {
        // Simulate a turn where a provider retry leaves a pending Write
        // block buried under later content.
        let mut doc = Document::new();
        doc.tool_selected("Write");
        // Add another block "after" the pending Write so it isn't the tail.
        doc.append_token("some assistant text\n");
        doc.newline();
        doc.tool_start("Bash", "$ ls");
        doc.tool_output("Bash", "out\n");
        doc.tool_end("Bash", "exit 0");

        doc.close_pending("retry");

        let pending_count = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Tool(tb) if !tb.is_done))
            .count();
        assert_eq!(pending_count, 0, "all pending tools must be finalised");

        let write_block = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Tool(tb) if tb.name == "Write" => Some(tb),
                _ => None,
            })
            .unwrap();
        assert!(write_block.is_done);
        assert_eq!(write_block.end_summary, "retry");

        // Existing completed tool should keep its original end_summary.
        let bash_block = doc
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Tool(tb) if tb.name == "Bash" => Some(tb),
                _ => None,
            })
            .unwrap();
        assert_eq!(bash_block.end_summary, "exit 0");
    }

    #[test]
    fn streaming_tokens() {
        let mut doc = Document::new();
        doc.append_token("hello ");
        doc.append_token("world\n");
        doc.append_token("line2");
        doc.newline();
        assert_eq!(doc.len(), 1); // single Text block
    }

    #[test]
    fn has_user_content_empty() {
        let doc = Document::new();
        assert!(!doc.has_user_content());
    }

    #[test]
    fn has_user_content_after_message() {
        let mut doc = Document::new();
        doc.info("welcome");
        assert!(!doc.has_user_content());
        doc.user_message(&text_content("hello"));
        assert!(doc.has_user_content());
    }

    #[test]
    fn has_user_content_resets_on_clear() {
        let mut doc = Document::new();
        doc.user_message(&text_content("hello"));
        doc.clear();
        assert!(!doc.has_user_content());
    }
}
