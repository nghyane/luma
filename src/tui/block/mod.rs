/// Block types — content blocks for the conversation document.
/// Pure data. No render logic.
mod chrome;
pub mod diff;
mod render;
mod text;
mod tool;

pub use render::{RenderState, render_block};

use crate::core::types::ContentBlock;
use crate::core::types::FileChangeArtifact;
use crate::tui::stream::StreamBuf;
use std::hash::{Hash, Hasher};

/// A content block in the conversation document.
#[derive(Debug, Clone)]
pub enum Block {
    Gap,
    GapLabel(String),
    Info(String),
    Error(String),
    Warn(String),
    User(Vec<ContentBlock>),
    Thinking(StreamBuf),
    Text(TextBlock),
    Tool(ToolBlock),
    Skill(SkillBlock),
}

/// Content-group discriminant for auto_gap logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Chrome,
    Thinking,
    Text,
    Tool,
    Skill,
}

impl Block {
    /// Whether this block carries conversational content (for gap insertion).
    pub fn is_content(&self) -> bool {
        matches!(
            self.kind(),
            BlockKind::Thinking | BlockKind::Text | BlockKind::Tool | BlockKind::Skill
        )
    }

    /// Whether two blocks belong to the same content group (no gap needed between them).
    /// Only consecutive Tool blocks are grouped — every other content transition
    /// gets an explicit `Block::Gap`.
    pub fn same_content_group(&self, other: &Block) -> bool {
        let a = self.kind();
        a == other.kind() && a == BlockKind::Tool
    }

    /// Content-group discriminant.
    fn kind(&self) -> BlockKind {
        match self {
            Block::Thinking(_) => BlockKind::Thinking,
            Block::Text(_) => BlockKind::Text,
            Block::Tool(_) => BlockKind::Tool,
            Block::Skill(_) => BlockKind::Skill,
            _ => BlockKind::Chrome,
        }
    }
}

/// Assistant text block — StreamBuf only. No render cache.
#[derive(Debug, Clone)]
pub struct TextBlock {
    pub stream: StreamBuf,
}

impl TextBlock {
    /// Create a new empty text block.
    pub fn new() -> Self {
        Self {
            stream: StreamBuf::new(),
        }
    }

    /// Feed a streaming token.
    pub fn feed(&mut self, token: &str) {
        self.stream.feed(token);
    }

    /// Flush partial into committed.
    pub fn flush(&mut self) {
        self.stream.flush();
    }

    /// Whether there's any content.
    pub fn is_empty(&self) -> bool {
        self.stream.is_empty()
    }
}

/// Tool invocation block.
#[derive(Debug, Clone)]
pub struct ToolBlock {
    pub name: String,
    pub summary: String,
    pub output: Vec<String>,
    pub artifact: Option<FileChangeArtifact>,
    /// Streamed tool-arg preview (e.g. `Write.content`, `apply_patch.patch`).
    /// Fed by `tool_input` while the provider is still delivering the
    /// tool's input JSON. Survives across `tool_start` so the content the
    /// user already saw is never wiped.
    ///
    /// Boxed so `ToolBlock` stays small; allocation happens only when the
    /// tool is actively streaming.
    pub arg_preview: Option<Box<StreamBuf>>,
    /// Streamed tool output (stdout/stderr) during execution. Created at
    /// `tool_start` and consumed into `output` by `tool_output`.
    pub stream: Option<Box<StreamBuf>>,
    pub is_done: bool,
    pub end_summary: String,
    pub is_expanded: bool,
}

impl ToolBlock {
    /// Create a streaming tool block (active tool call).
    pub fn streaming(name: &str, summary: &str) -> Self {
        Self {
            name: name.to_owned(),
            summary: summary.to_owned(),
            output: Vec::new(),
            artifact: None,
            arg_preview: Some(Box::new(StreamBuf::new())),
            stream: None,
            is_done: false,
            end_summary: String::new(),
            is_expanded: false,
        }
    }

    /// Create a completed tool block (history replay).
    pub fn history(name: &str, summary: &str) -> Self {
        Self {
            name: name.to_owned(),
            summary: summary.to_owned(),
            output: Vec::new(),
            artifact: None,
            arg_preview: None,
            stream: None,
            is_done: true,
            end_summary: String::new(),
            is_expanded: false,
        }
    }
}

/// Skill activation block.
#[derive(Debug, Clone)]
pub struct SkillBlock {
    pub name: String,
    pub is_done: bool,
    pub end_summary: String,
}

/// Fingerprint for pull-based dirty detection in Layout.
/// Two equal snapshots mean no re-render needed.
#[derive(Clone)]
pub enum Snapshot {
    /// Always re-render (active spinner, etc.).
    Volatile,
    /// Never changes after creation.
    Immutable,
    /// Streaming content — track committed + partial lengths.
    Stream { committed: usize, partial: usize },
    /// Completed tool — fingerprint of state that affects rendering.
    Tool { fingerprint: u64 },
    /// Skill — track completion.
    Skill { is_done: bool },
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Volatile, _) | (_, Self::Volatile) => false,
            (Self::Immutable, Self::Immutable) => true,
            (
                Self::Stream {
                    committed: a,
                    partial: b,
                },
                Self::Stream {
                    committed: c,
                    partial: d,
                },
            ) => a == c && b == d,
            (Self::Tool { fingerprint: a }, Self::Tool { fingerprint: b }) => a == b,
            (Self::Skill { is_done: a }, Self::Skill { is_done: b }) => a == b,
            _ => false,
        }
    }
}

impl Block {
    /// Snapshot for dirty detection. Layout compares old vs new.
    pub fn snapshot(&self) -> Snapshot {
        match self {
            Block::Gap
            | Block::GapLabel(_)
            | Block::Info(_)
            | Block::Error(_)
            | Block::Warn(_)
            | Block::User(_) => Snapshot::Immutable,
            Block::Thinking(s) => Snapshot::Stream {
                committed: s.committed.len(),
                partial: s.partial().len(),
            },
            Block::Text(tb) => Snapshot::Stream {
                committed: tb.stream.committed.len(),
                partial: tb.stream.partial().len(),
            },
            Block::Tool(tb) if tb.is_done => Snapshot::Tool {
                fingerprint: tool_snapshot_fingerprint(tb),
            },
            Block::Tool(_) => Snapshot::Volatile,
            Block::Skill(sb) => Snapshot::Skill {
                is_done: sb.is_done,
            },
        }
    }
}

fn tool_snapshot_fingerprint(tb: &ToolBlock) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tb.name.hash(&mut hasher);
    tb.summary.hash(&mut hasher);
    tb.end_summary.hash(&mut hasher);
    tb.is_done.hash(&mut hasher);
    tb.is_expanded.hash(&mut hasher);

    if let Some(artifact) = &tb.artifact {
        hash_file_change_artifact(artifact, &mut hasher);
    } else {
        tb.output.hash(&mut hasher);
    }

    hasher.finish()
}

fn hash_file_change_artifact(
    artifact: &FileChangeArtifact,
    hasher: &mut std::collections::hash_map::DefaultHasher,
) {
    artifact.status.hash(hasher);
    artifact.raw_input.hash(hasher);
    artifact.error.hash(hasher);
    artifact.files.len().hash(hasher);
    for file in &artifact.files {
        file.path.hash(hasher);
        file.operation.hash(hasher);
        file.diff.hash(hasher);
        file.preview.hash(hasher);
    }
}

#[cfg(test)]
mod tests;
