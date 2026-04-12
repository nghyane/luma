/// Trait for agent tools (read, write, bash, edit).
use crate::core::types::{FileChangeArtifact, ToolSchema};
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

/// Structured result returned by a tool execution.
#[derive(Debug)]
pub struct ToolExecution {
    pub result: String,
    pub artifact: Option<FileChangeArtifact>,
}

/// A tool the agent can invoke. Dyn-compatible via boxed future.
pub trait Tool: Send + Sync {
    /// Tool name as seen by the model.
    fn name(&self) -> &str;

    /// JSON schema for the model to call this tool.
    fn schema(&self) -> ToolSchema;
    /// Execute the tool with parsed arguments. Streams incremental output
    /// into `output_tx`. Returns the full result and any structured artifact.
    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>>;
}
