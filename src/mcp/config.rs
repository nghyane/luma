use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// MCP server configuration file format.
///
/// Format matches Claude Code's `mcpServers` shape byte-for-byte so config
/// files roundtrip between the two tools without edits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: HashMap<String, McpServerEntry>,
}

/// A single MCP server entry. Stdio-only for now.
///
/// Shape matches Claude Code's `McpStdioServerConfig`:
///   - `type`: optional, defaults to "stdio" when omitted (backward compat).
///   - `command`: required, path or name of the executable.
///   - `args`: positional args.
///   - `env`: environment variables.
///
/// Unknown fields are ignored so future Claude Code additions
/// (sse/http transports, oauth, etc.) don't break config parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Transport type — "stdio" or omitted. Non-stdio types are rejected
    /// at load time with a clear error (we only implement stdio today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

/// Load and merge MCP configs. Project-local overrides user-global.
pub fn load() -> McpConfig {
    let mut merged = McpConfig::default();

    // User-global
    if let Some(path) = global_config_path() {
        merge_from(&mut merged, &path);
    }

    // Project-local
    merge_from(&mut merged, &PathBuf::from(".luma/mcp.json"));

    // Claude Code compat fallback (only servers not already defined)
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
        // Only stdio is supported today. Reject anything else with a log
        // line so users running a newer config (sse/http) know why their
        // server isn't showing up.
        if let Some(ref t) = entry.r#type
            && t != "stdio"
        {
            crate::dbg_log!(
                "mcp: skipping '{name}' from {} — transport '{t}' not supported (stdio only)",
                path.display()
            );
            continue;
        }
        config.servers.insert(name, entry);
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
        if let Ok(parsed) = serde_json::from_value::<McpServerEntry>(entry.clone()) {
            if let Some(ref t) = parsed.r#type
                && t != "stdio"
            {
                continue;
            }
            if !config.servers.contains_key(name) {
                config.servers.insert(name.clone(), parsed);
                found = true;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_format() {
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
        let entry = cfg.servers.get("laravel-boost").unwrap();
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
        let entry = cfg.servers.get("x").unwrap();
        assert_eq!(entry.r#type.as_deref(), Some("stdio"));
    }

    /// Legacy `servers` key (non-Claude-Code format) must be rejected so
    /// users are nudged toward the standard `mcpServers` format.
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

    /// Unknown fields (future Claude Code additions like sse/http) must
    /// not break parsing of the stdio entries we do understand.
    #[test]
    fn tolerates_unknown_fields() {
        let json = r#"{
            "mcpServers": {
                "x": {
                    "command": "echo",
                    "headers": { "X-Foo": "bar" },
                    "oauth": { "clientId": "abc" }
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
        let entry = cfg.servers.get("x").unwrap();
        assert_eq!(entry.env.get("API_KEY").map(String::as_str), Some("secret"));

        // Roundtrip must produce `mcpServers` and omit empty optional fields.
        let out = serde_json::to_string(&cfg).unwrap();
        assert!(out.contains("\"mcpServers\""));
        assert!(!out.contains("\"type\""));
    }
}
