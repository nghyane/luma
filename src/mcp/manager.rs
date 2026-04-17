use super::bridge::McpTool;
use super::config::{McpConfig, McpServerEntry};
use crate::core::registry::Registry;
use rmcp::model::RawContent;
use rmcp::model::Tool as RmcpTool;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use std::collections::HashMap;
use tokio::process::Command;

/// Connection state for a single MCP server.
pub struct McpConnection {
    pub name: String,
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<RmcpTool>,
}

/// Status of an MCP server.
#[derive(Debug, Clone)]
pub enum McpStatus {
    Connected { tool_count: usize },
    Failed(String),
}

/// Manages all MCP server connections.
pub struct McpManager {
    connections: HashMap<String, McpConnection>,
    statuses: HashMap<String, McpStatus>,
}

impl McpManager {
    /// Spawn all configured MCP servers. Failures are captured per-server.
    pub async fn start(config: &McpConfig) -> Self {
        let mut connections = HashMap::new();
        let mut statuses = HashMap::new();

        for (name, entry) in &config.servers {
            match connect_stdio(name, entry).await {
                Ok(conn) => {
                    let count = conn.tools.len();
                    statuses.insert(name.clone(), McpStatus::Connected { tool_count: count });
                    crate::dbg_log!("mcp: connected to {name} ({count} tools)");
                    connections.insert(name.clone(), conn);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    crate::dbg_log!("mcp: failed to connect to {name}: {msg}");
                    statuses.insert(name.clone(), McpStatus::Failed(msg));
                }
            }
        }

        Self {
            connections,
            statuses,
        }
    }

    /// Register all MCP tools into an existing Registry.
    pub fn register_tools(&self, registry: &mut Registry) {
        for conn in self.connections.values() {
            for tool in &conn.tools {
                let prefixed = format!("mcp__{}__{}", normalize(&conn.name), &tool.name);
                let schema = serde_json::to_value(&*tool.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));
                let description = tool.description.as_deref().unwrap_or("").to_owned();
                registry.register(Box::new(McpTool {
                    server_name: conn.name.clone(),
                    original_tool_name: tool.name.to_string(),
                    prefixed_name: prefixed,
                    description,
                    input_schema: schema,
                }));
            }
        }
    }

    /// Call a tool on a specific server. Returns the text result.
    pub async fn call_tool(
        &self,
        server: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<String> {
        let conn = self
            .connections
            .get(server)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' not connected"))?;

        let mut params = rmcp::model::CallToolRequestParams::new(tool_name.to_owned());
        if let Some(obj) = args.as_object().cloned() {
            params = params.with_arguments(obj);
        }

        let result = conn.service.call_tool(params).await?;

        let text = result
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                RawContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error.unwrap_or(false) && !text.is_empty() {
            anyhow::bail!("{text}");
        }

        Ok(text)
    }

    /// Server statuses for UI display.
    pub fn statuses(&self) -> &HashMap<String, McpStatus> {
        &self.statuses
    }

    /// Shutdown all connections.
    #[allow(dead_code)]
    pub async fn shutdown(self) {
        for (name, conn) in self.connections {
            if let Err(e) = conn.service.cancel().await {
                crate::dbg_log!("mcp: error shutting down {name}: {e}");
            }
        }
    }

    /// Whether any servers are configured (even if failed).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }
}

async fn connect_stdio(name: &str, entry: &McpServerEntry) -> anyhow::Result<McpConnection> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    let env_snapshot: Vec<(String, String)> = entry
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let args_snapshot: Vec<String> = entry.args.clone();
    let cmd_snapshot = entry.command.clone();

    let cmd = Command::new(&cmd_snapshot).configure(move |cmd| {
        for arg in &args_snapshot {
            cmd.arg(arg);
        }
        for (k, v) in &env_snapshot {
            cmd.env(k, v);
        }
    });

    // Pipe stderr so we can include subprocess error output in connect failures.
    // Otherwise `Stdio::inherit()` (rmcp default) lets messages leak to the user's terminal.
    let (transport, stderr_opt) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()?;

    // Capture stderr into a shared buffer so connect errors can include the last output.
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut stderr) = stderr_opt {
        let buf = std::sync::Arc::clone(&stderr_buf);
        tokio::spawn(async move {
            let mut data = Vec::with_capacity(2048);
            let _ = stderr.read_to_end(&mut data).await;
            if !data.is_empty() {
                let text = String::from_utf8_lossy(&data).into_owned();
                if let Ok(mut guard) = buf.lock() {
                    *guard = text;
                }
            }
        });
    }

    let connect_result =
        tokio::time::timeout(std::time::Duration::from_secs(30), ().serve(transport)).await;

    let service = match connect_result {
        Err(_) => {
            let stderr_tail = stderr_tail(&stderr_buf);
            anyhow::bail!(
                "timeout connecting to MCP server '{name}' after 30s{}",
                stderr_tail
            );
        }
        Ok(Err(e)) => {
            let stderr_tail = stderr_tail(&stderr_buf);
            anyhow::bail!("{}{}", e, stderr_tail);
        }
        Ok(Ok(svc)) => svc,
    };

    let tools_result = service.list_all_tools().await?;

    Ok(McpConnection {
        name: name.to_owned(),
        service,
        tools: tools_result,
    })
}

/// Capture up to 400 chars of stderr tail, prefixed for inline error display.
fn stderr_tail(buf: &std::sync::Arc<std::sync::Mutex<String>>) -> String {
    let Ok(guard) = buf.lock() else {
        return String::new();
    };
    let text = guard.trim();
    if text.is_empty() {
        return String::new();
    }
    const MAX: usize = 400;
    let start = text.len().saturating_sub(MAX);
    // Align to char boundary.
    let start = text
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!(" — stderr: {}", &text[start..])
}

/// Normalize server name for tool prefix: non-alphanumeric → `_`, collapse runs.
fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out.trim_matches('_').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_names() {
        assert_eq!(normalize("my-server"), "my_server");
        assert_eq!(normalize("github"), "github");
        assert_eq!(normalize("my--server!!"), "my_server");
        assert_eq!(normalize("a.b.c"), "a_b_c");
    }
}
