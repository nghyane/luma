use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// MCP server configuration file format.
///
/// Format matches Claude Code's `mcpServers` shape so config files can
/// roundtrip between the two tools without edits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: HashMap<String, McpServerEntry>,
}

/// A single MCP server entry.
///
/// Supported transports:
/// - stdio (default when `type` is omitted)
/// - http (streamable HTTP)
/// - sse (mapped to the same rmcp streamable HTTP transport configuration)
///
/// Unknown fields are ignored so future Claude Code additions don't break
/// config parsing of supported entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerEntry {
    /// Backward-compatible stdio transport. Claude Code also omits
    /// `type: "stdio"` for this shape.
    Stdio(McpStdioServerEntry),
    /// Remote HTTP/SSE transports.
    Http(McpHttpServerEntry),
}

/// A stdio MCP server config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStdioServerEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// OAuth metadata for remote MCP servers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpOAuthConfig {
    #[serde(rename = "clientId", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(
        rename = "authServerMetadataUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_server_metadata_url: Option<String>,
}

/// A remote MCP server config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpHttpServerEntry {
    pub r#type: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(
        rename = "headersHelper",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub headers_helper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
}

impl McpServerEntry {
    /// Human-readable transport name for status and CLI output.
    pub fn transport_name(&self) -> &'static str {
        match self {
            Self::Stdio(_) => "stdio",
            Self::Http(entry) => {
                if entry.r#type == "sse" {
                    "sse"
                } else {
                    "http"
                }
            }
        }
    }
}

/// Load and merge MCP configs. Project-local overrides user-global.
pub fn load() -> McpConfig {
    let mut merged = McpConfig::default();

    if let Some(path) = global_config_path() {
        merge_from(&mut merged, &path);
    }

    merge_from(&mut merged, &PathBuf::from(".luma/mcp.json"));

    if merged.servers.is_empty() {
        for path in claude_code_paths() {
            if merge_claude_code(&mut merged, &path) {
                break;
            }
        }
    }

    merged
}

fn global_config_path() -> Option<PathBuf> {
    Some(crate::config::home_dir().join(".config/luma/mcp.json"))
}

fn claude_code_paths() -> Vec<PathBuf> {
    let home = crate::config::home_dir();
    vec![
        PathBuf::from(".claude/settings.json"),
        PathBuf::from(".claude.json"),
        home.join(".claude/settings.json"),
        home.join(".claude.json"),
    ]
}

fn merge_from(config: &mut McpConfig, path: &PathBuf) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return;
    };
    let parsed: McpConfig = match serde_json::from_str(&data) {
        Ok(c) => c,
        Err(e) => {
            crate::dbg_log!("mcp: failed to parse {}: {e}", path.display());
            return;
        }
    };
    for (name, entry) in parsed.servers {
        if supports_entry(&entry) {
            config.servers.insert(name, entry);
        } else {
            crate::dbg_log!(
                "mcp: skipping '{name}' from {} — transport '{}' not supported",
                path.display(),
                entry.transport_name()
            );
        }
    }
}

fn supports_entry(entry: &McpServerEntry) -> bool {
    match entry {
        McpServerEntry::Stdio(s) => s.r#type.as_deref().is_none_or(|t| t == "stdio"),
        McpServerEntry::Http(h) => matches!(h.r#type.as_str(), "http" | "sse"),
    }
}

/// Extract `mcpServers` from Claude Code settings files.
fn merge_claude_code(config: &mut McpConfig, path: &PathBuf) -> bool {
    let Ok(data) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) else {
        return false;
    };
    let Some(servers) = val.get("mcpServers").and_then(|v| v.as_object()) else {
        return false;
    };
    let mut found = false;
    for (name, entry) in servers {
        if let Ok(parsed) = serde_json::from_value::<McpServerEntry>(entry.clone())
            && supports_entry(&parsed)
            && !config.servers.contains_key(name)
        {
            config.servers.insert(name.clone(), parsed);
            found = true;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_stdio_format() {
        let json = r#"{
            "mcpServers": {
                "laravel-boost": {
                    "command": "php",
                    "args": ["artisan", "boost:mcp"]
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.servers.len(), 1);
        let McpServerEntry::Stdio(entry) = cfg.servers.get("laravel-boost").unwrap() else {
            panic!("expected stdio entry");
        };
        assert_eq!(entry.command, "php");
        assert_eq!(entry.args, vec!["artisan", "boost:mcp"]);
        assert!(entry.env.is_empty());
        assert!(entry.r#type.is_none());
    }

    #[test]
    fn parses_type_stdio_field() {
        let json = r#"{
            "mcpServers": {
                "x": { "type": "stdio", "command": "echo" }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let McpServerEntry::Stdio(entry) = cfg.servers.get("x").unwrap() else {
            panic!("expected stdio entry");
        };
        assert_eq!(entry.r#type.as_deref(), Some("stdio"));
    }

    #[test]
    fn parses_http_entry() {
        let json = r#"{
            "mcpServers": {
                "figma": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "headers": { "Authorization": "Bearer token" },
                    "oauth": { "clientId": "abc" },
                    "headersHelper": "print-headers"
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let McpServerEntry::Http(entry) = cfg.servers.get("figma").unwrap() else {
            panic!("expected http entry");
        };
        assert_eq!(entry.r#type, "http");
        assert_eq!(entry.url, "https://example.com/mcp");
        assert_eq!(
            entry.headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
        assert_eq!(
            entry.oauth.as_ref().and_then(|o| o.client_id.as_deref()),
            Some("abc")
        );
        assert_eq!(entry.headers_helper.as_deref(), Some("print-headers"));
    }

    #[test]
    fn parses_sse_entry() {
        let json = r#"{
            "mcpServers": {
                "remote": {
                    "type": "sse",
                    "url": "https://example.com/sse"
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let McpServerEntry::Http(entry) = cfg.servers.get("remote").unwrap() else {
            panic!("expected remote entry");
        };
        assert_eq!(entry.r#type, "sse");
        assert_eq!(cfg.servers.get("remote").unwrap().transport_name(), "sse");
    }

    #[test]
    fn ignores_legacy_servers_key() {
        let json = r#"{
            "servers": {
                "x": { "command": "echo" }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.servers.is_empty());
    }

    #[test]
    fn tolerates_unknown_fields() {
        let json = r#"{
            "mcpServers": {
                "x": {
                    "type": "http",
                    "url": "https://example.com/mcp",
                    "oauth": { "clientId": "abc", "callbackPort": 3000 }
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.servers.len(), 1);
    }

    #[test]
    fn env_roundtrips() {
        let json = r#"{
            "mcpServers": {
                "x": {
                    "command": "node",
                    "args": ["server.js"],
                    "env": { "API_KEY": "secret" }
                }
            }
        }"#;
        let cfg: McpConfig = serde_json::from_str(json).unwrap();
        let McpServerEntry::Stdio(entry) = cfg.servers.get("x").unwrap() else {
            panic!("expected stdio entry");
        };
        assert_eq!(entry.env.get("API_KEY").map(String::as_str), Some("secret"));

        let out = serde_json::to_string(&cfg).unwrap();
        assert!(out.contains("\"mcpServers\""));
        assert!(!out.contains("\"type\""));
    }
}
