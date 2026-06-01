use super::bridge::McpTool;
use super::config::{McpConfig, McpHttpServerEntry, McpServerEntry, McpStdioServerEntry};
use crate::core::registry::Registry;
use rmcp::model::RawContent;
use rmcp::model::Tool as RmcpTool;
use rmcp::service::{ClientInitializeError, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinSet;

const MCP_CONNECT_CONCURRENCY: usize = 4;
const MCP_STDIO_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const MCP_REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// Connection state for a single MCP server.
pub struct McpConnection {
    pub name: String,
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<RmcpTool>,
}

/// Status of an MCP server.
#[derive(Debug, Clone)]
pub enum McpStatus {
    Connected {
        tool_count: usize,
        transport: &'static str,
    },
    NeedsAuth {
        transport: &'static str,
        message: String,
    },
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
        let mut servers = config.servers.iter();
        let mut tasks = JoinSet::new();

        for _ in 0..MCP_CONNECT_CONCURRENCY {
            spawn_next_connect(&mut tasks, &mut servers);
        }

        while let Some(joined) = tasks.join_next().await {
            if let Ok(result) = joined {
                record_connect_result(result, &mut connections, &mut statuses);
            }
            spawn_next_connect(&mut tasks, &mut servers);
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

struct ConnectResult {
    name: String,
    transport: &'static str,
    outcome: anyhow::Result<McpConnection>,
}

fn spawn_next_connect<'a>(
    tasks: &mut JoinSet<ConnectResult>,
    servers: &mut std::collections::hash_map::Iter<'a, String, McpServerEntry>,
) {
    let Some((name, entry)) = servers.next() else {
        return;
    };
    let name = name.clone();
    let entry = entry.clone();
    let transport = entry.transport_name();
    tasks.spawn(async move {
        let outcome = connect(&name, &entry).await;
        ConnectResult {
            name,
            transport,
            outcome,
        }
    });
}

fn record_connect_result(
    result: ConnectResult,
    connections: &mut HashMap<String, McpConnection>,
    statuses: &mut HashMap<String, McpStatus>,
) {
    let ConnectResult {
        name,
        transport,
        outcome,
    } = result;
    match outcome {
        Ok(conn) => {
            let count = conn.tools.len();
            statuses.insert(
                name.clone(),
                McpStatus::Connected {
                    tool_count: count,
                    transport,
                },
            );
            crate::dbg_log!("mcp: connected to {name} via {transport} ({count} tools)");
            connections.insert(name, conn);
        }
        Err(e) => {
            let msg = format!("{transport} connection failed: {e:#}");
            crate::dbg_log!("mcp: failed to connect to {name}: {msg}");
            if is_needs_auth_message(&msg) {
                statuses.insert(
                    name,
                    McpStatus::NeedsAuth {
                        transport,
                        message: msg,
                    },
                );
            } else {
                statuses.insert(name, McpStatus::Failed(msg));
            }
        }
    }
}

async fn connect(name: &str, entry: &McpServerEntry) -> anyhow::Result<McpConnection> {
    match entry {
        McpServerEntry::Stdio(entry) => connect_stdio(name, entry).await,
        McpServerEntry::Http(entry) => connect_remote(name, entry).await,
    }
}

async fn connect_stdio(name: &str, entry: &McpStdioServerEntry) -> anyhow::Result<McpConnection> {
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

    let (transport, stderr_opt) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()?;

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

    let connect_result = tokio::time::timeout(MCP_STDIO_CONNECT_TIMEOUT, ().serve(transport)).await;

    let service = match connect_result {
        Err(_) => {
            let stderr_tail = stderr_tail(&stderr_buf);
            anyhow::bail!(
                "timeout connecting to MCP server '{name}' after {}s{}",
                MCP_STDIO_CONNECT_TIMEOUT.as_secs(),
                stderr_tail
            );
        }
        Ok(Err(e)) => {
            let stderr_tail = stderr_tail(&stderr_buf);
            anyhow::bail!("{}{}", e, stderr_tail);
        }
        Ok(Ok(svc)) => svc,
    };

    finish_connection(name, service).await
}

async fn connect_remote(name: &str, entry: &McpHttpServerEntry) -> anyhow::Result<McpConnection> {
    let headers = merged_remote_headers(name, entry)?;
    let mut config = StreamableHttpClientTransportConfig::with_uri(entry.url.clone())
        .custom_headers(headers)
        .reinit_on_expired_session(true);

    if entry.r#type == "sse" {
        config.allow_stateless = false;
    }

    let transport = StreamableHttpClientTransport::from_config(config);

    let service = match tokio::time::timeout(MCP_REMOTE_CONNECT_TIMEOUT, ().serve(transport)).await
    {
        Err(_) => anyhow::bail!(
            "timeout connecting to MCP server '{name}' after {}s",
            MCP_REMOTE_CONNECT_TIMEOUT.as_secs()
        ),
        Ok(Ok(service)) => service,
        Ok(Err(err)) => return handle_remote_connect_error(name, entry, err).await,
    };

    finish_connection(name, service).await
}

fn merged_remote_headers(
    server_name: &str,
    entry: &McpHttpServerEntry,
) -> anyhow::Result<std::collections::HashMap<http::HeaderName, http::HeaderValue>> {
    let mut merged = entry.headers.clone();

    if let Some(helper) = &entry.headers_helper {
        let dynamic = resolve_headers_helper(server_name, entry, helper)?;
        merged.extend(dynamic);
    }

    let auth = crate::mcp::auth::resolve_remote_auth(server_name, entry)?;
    if let Some(token) = auth.bearer_token {
        merged.insert(String::from("Authorization"), format!("Bearer {token}"));
    }

    merged
        .into_iter()
        .map(|(header_name, header_value)| {
            let parsed_name = header_name.parse().map_err(|e| {
                anyhow::anyhow!(
                    "invalid HTTP header name '{}' for MCP server '{}': {e}",
                    header_name,
                    server_name
                )
            })?;
            let parsed_value = header_value.parse().map_err(|e| {
                anyhow::anyhow!(
                    "invalid HTTP header value for MCP server '{}' header '{}': {e}",
                    server_name,
                    header_name
                )
            })?;
            Ok((parsed_name, parsed_value))
        })
        .collect()
}

fn resolve_headers_helper(
    server_name: &str,
    entry: &McpHttpServerEntry,
    helper: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", helper])
            .env("LUMA_MCP_SERVER_NAME", server_name)
            .env("LUMA_MCP_SERVER_URL", &entry.url)
            .output()?
    } else {
        std::process::Command::new("sh")
            .args(["-c", helper])
            .env("LUMA_MCP_SERVER_NAME", server_name)
            .env("LUMA_MCP_SERVER_URL", &entry.url)
            .output()?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "headersHelper failed for MCP server '{}': {}",
            server_name,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())?;
    let obj = parsed.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "headersHelper for MCP server '{}' must return a JSON object",
            server_name
        )
    })?;

    obj.iter()
        .map(|(k, v)| {
            let value = v.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "headersHelper for MCP server '{}' returned non-string value for key '{}'",
                    server_name,
                    k
                )
            })?;
            Ok((k.clone(), value.to_owned()))
        })
        .collect()
}

async fn handle_remote_connect_error(
    name: &str,
    entry: &McpHttpServerEntry,
    err: ClientInitializeError,
) -> anyhow::Result<McpConnection> {
    let msg = format!("{err}");
    if let Some(discovered) = discover_auth_metadata_from_error(name, entry, &msg).await? {
        crate::dbg_log!(
            "mcp: discovered OAuth metadata for {name} via resource metadata {}",
            discovered
                .resource_metadata_url
                .as_deref()
                .unwrap_or("<unknown>")
        );
    }
    if is_auth_required_error(&msg)
        && let Some(token) = crate::mcp::auth::refresh_access_token(name, entry).await?
    {
        crate::dbg_log!("mcp: refreshed OAuth token for {name}");
        let headers = merged_remote_headers(name, entry)?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(entry.url.clone())
            .custom_headers(headers)
            .reinit_on_expired_session(true);
        if entry.r#type == "sse" {
            config.allow_stateless = false;
        }
        let transport = StreamableHttpClientTransport::from_config(config);
        let service = tokio::time::timeout(std::time::Duration::from_secs(30), ().serve(transport))
            .await
            .map_err(|_| {
                anyhow::anyhow!("timeout reconnecting to MCP server '{name}' after refresh")
            })??;
        let _ = token;
        return finish_connection(name, service).await;
    }

    if is_auth_required_error(&msg) {
        let auth = crate::mcp::auth::resolve_remote_auth(name, entry)?;
        if let Some(authorization_endpoint) = auth.authorization_endpoint {
            anyhow::bail!(
                "authentication required for MCP server '{name}' — run `luma mcp auth {name}` to authorize (authorization endpoint: {authorization_endpoint})"
            );
        }
        anyhow::bail!(
            "authentication required for MCP server '{name}' — store tokens with `luma mcp set-secret {name} --access-token ...` or configure OAuth metadata"
        );
    }

    if is_insufficient_scope_error(&msg) {
        anyhow::bail!(
            "insufficient OAuth scope for MCP server '{name}' — stored token is missing required permissions"
        );
    }

    Err(err.into())
}

fn is_auth_required_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("auth required")
        || lower.contains("authentication required")
        || lower.contains("www-authenticate")
        || lower.contains("401")
}

fn is_insufficient_scope_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("insufficient scope")
        || lower.contains("insufficient_scope")
        || lower.contains("403")
}

fn is_needs_auth_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("authentication required")
        || lower.contains("auth required")
        || lower.contains("needs auth")
        || lower.contains("store tokens")
}

async fn discover_auth_metadata_from_error(
    server_name: &str,
    entry: &McpHttpServerEntry,
    msg: &str,
) -> anyhow::Result<Option<crate::mcp::auth::McpOAuthEntry>> {
    if let Some(www_authenticate) = extract_www_authenticate_header(msg) {
        return crate::mcp::auth::discover_from_www_authenticate(
            server_name,
            entry,
            &www_authenticate,
        )
        .await;
    }
    Ok(None)
}

fn extract_www_authenticate_header(msg: &str) -> Option<String> {
    let lower = msg.to_ascii_lowercase();
    let key = "www-authenticate";
    let start = lower.find(key)?;
    let value = msg[start + key.len()..]
        .trim_start_matches([':', ' ', '='])
        .trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

async fn finish_connection(
    name: &str,
    service: RunningService<RoleClient, ()>,
) -> anyhow::Result<McpConnection> {
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
