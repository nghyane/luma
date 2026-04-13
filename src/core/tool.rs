/// Trait for agent tools (read, write, bash, edit).
use crate::core::types::{FileChangeArtifact, ToolResultBody, ToolSchema};
use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Canonical marker appended when a tool result is tail-truncated.
///
/// Shared so downstream consumers (UI, evidence store) can reliably detect
/// truncation via a single token. Tools that use head+tail truncation
/// (e.g. `bash`) have their own self-describing marker and do not use this.
pub const TRUNCATION_MARKER: &str = "\n[truncated]";

/// Capabilities of the model currently driving the agent turn.
///
/// Passed to `Tool::execute` so tools can branch on what the model can
/// actually consume. Text-only model still receives a metadata fallback
/// rather than a 400 from the provider when a tool wants to attach an
/// image.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCaps {
    /// Whether the model accepts image input items (Anthropic `image`
    /// block, OpenAI `input_image`).
    pub vision: bool,
}

/// Structured result returned by a tool execution.
#[derive(Debug)]
pub struct ToolExecution {
    /// Text or multimodal body. Callers can build directly with
    /// `"...".into()` for plain text — the majority of tools keep this
    /// shape. Image-aware tools (e.g. `Read` on a PNG) emit
    /// `ToolResultBody::Items(...)`.
    pub result: ToolResultBody,
    pub artifact: Option<FileChangeArtifact>,
}

/// A tool the agent can invoke. Dyn-compatible via boxed future.
pub trait Tool: Send + Sync {
    /// Tool name as seen by the model.
    fn name(&self) -> &str;

    /// JSON schema for the model to call this tool.
    fn schema(&self) -> ToolSchema;
    /// Execute the tool with parsed arguments. Streams incremental output
    /// into `output_tx`. Returns the full result and any structured
    /// artifact. `caps` carries model capabilities so tools can pick the
    /// best representation (e.g. image bytes vs metadata text).
    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
        caps: ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>>;
}
