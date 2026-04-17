use crate::core::tool::{ModelCaps, Tool, ToolExecution};
use crate::core::types::ToolSchema;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::manager::McpManager;

/// Global handle to the MCP manager, set once at startup.
static MCP_MANAGER: std::sync::OnceLock<Arc<McpManager>> = std::sync::OnceLock::new();

/// Initialize the global MCP manager. Called once from main/app startup.
pub fn set_global_manager(manager: McpManager) {
    let _ = MCP_MANAGER.set(Arc::new(manager));
}

/// Get the global MCP manager.
pub fn global_manager() -> Option<&'static Arc<McpManager>> {
    MCP_MANAGER.get()
}

/// A tool backed by an MCP server. Implements `core::tool::Tool` so it
/// appears in the Registry identically to built-in tools.
pub struct McpTool {
    pub server_name: String,
    pub original_tool_name: String,
    pub prefixed_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.prefixed_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
            streamable_arg: None,
        }
    }

    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        _cancel: CancellationToken,
        _caps: ModelCaps,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ToolExecution>> + Send + '_>> {
        Box::pin(async move {
            let manager =
                global_manager().ok_or_else(|| anyhow::anyhow!("MCP manager not initialized"))?;

            let result = manager
                .call_tool(&self.server_name, &self.original_tool_name, args)
                .await?;

            let _ = output_tx.send(result.clone()).await;

            Ok(ToolExecution {
                result: result.into(),
                artifact: None,
            })
        })
    }
}
