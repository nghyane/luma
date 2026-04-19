use super::config::{
    McpConfig, McpHttpServerEntry, McpOAuthConfig, McpServerEntry, McpStdioServerEntry,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn oauth_repo() -> super::auth::SqliteMcpOAuthRepository {
    super::auth::SqliteMcpOAuthRepository::with_default_path()
}

/// Where to write user-global MCP config.
fn user_config_path() -> PathBuf {
    crate::config::home_dir().join(".config/luma/mcp.json")
}

/// Load the user-global config file directly (not merged).
fn load_user_config() -> McpConfig {
    let path = user_config_path();
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<McpConfig>(&s).ok())
        .unwrap_or_default()
}

/// Write the user-global config file, creating the parent dir if needed.
fn save_user_config(config: &McpConfig) -> anyhow::Result<()> {
    let path = user_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Dispatch `luma mcp <subcmd> [args...]`.
pub fn run(args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(String::as_str);
    match sub {
        Some("list") | None => list(),
        Some("add") => add(&args[1..]),
        Some("get") => get(&args[1..]),
        Some("auth") => auth(&args[1..]),
        Some("set-secret") => set_secret(&args[1..]),
        Some("clear-secret") => clear_secret(&args[1..]),
        Some("revoke") => revoke(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("status") => status(),
        _ => {
            usage();
            anyhow::bail!("unknown subcommand");
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n\
         \x20 luma mcp list                                                           list configured servers\n\
         \x20 luma mcp status                                                         same as list, with sources\n\
         \x20 luma mcp get <name>                                                     show a server config\n\
         \x20 luma mcp auth <name>                                                    run browser OAuth flow for a remote server\n\
         \x20 luma mcp add <name> -- <cmd> [args...]                                  add a stdio server\n\
         \x20 luma mcp add -e KEY=VAL <name> -- <cmd>                                 add a stdio server with env vars\n\
         \x20 luma mcp add --transport http <name> <url>                              add a remote streamable HTTP server\n\
         \x20 luma mcp add --transport sse <name> <url>                               add a remote SSE server\n\
         \x20 luma mcp add --transport http -H KEY=VAL <name> <url>                   add a remote server with headers\n\
         \x20 luma mcp add --transport http --client-id <id> <name> <url>             set oauth client_id in config\n\
         \x20 luma mcp add --headers-helper '<cmd>' --transport http <name> <url>     resolve dynamic headers via helper\n\
         \x20 luma mcp set-secret <name> --client-secret <value>                      store remote OAuth client secret\n\
         \x20 luma mcp clear-secret <name>                                            remove stored remote OAuth secrets\n\
         \x20 luma mcp revoke <name>                                                  revoke remote OAuth tokens and clear local tokens\n\
         \x20 luma mcp remove <name>                                                  remove a server\n\
         \n\
         examples:\n\
         \x20 luma mcp add github -- npx -y @modelcontextprotocol/server-github\n\
         \x20 luma mcp add --transport http figma https://mcp.figma.com/mcp\n\
         \x20 luma mcp add --transport sse sentry https://mcp.sentry.dev/sse\n\
         \x20 luma mcp add --transport http -H Authorization='Bearer token' figma https://mcp.figma.com/mcp\n\
         \x20 luma mcp add --transport http --client-id abc123 figma https://mcp.figma.com/mcp\n\
         \x20 luma mcp auth figma\n\
         \x20 luma mcp revoke figma\n\
         \x20 luma mcp set-secret figma --client-secret s3cr3t\n\
         \x20 luma mcp remove github"
    );
}

fn list() -> anyhow::Result<()> {
    let merged = super::config::load();
    if merged.servers.is_empty() {
        println!("no MCP servers configured");
        println!();
        println!("config files (checked in order):");
        println!("  .luma/mcp.json                    (project)");
        println!("  ~/.config/luma/mcp.json           (user)");
        println!("  ~/.claude/settings.json           (Claude Code fallback)");
        println!();
        println!("add one with: luma mcp add <name> -- <cmd> [args...]");
        println!("or:           luma mcp add --transport http <name> <url>");
        println!("or:           luma mcp add --transport sse <name> <url>");
        return Ok(());
    }
    println!("MCP servers ({}):", merged.servers.len());
    for (name, entry) in &merged.servers {
        match entry {
            McpServerEntry::Stdio(entry) => {
                let argv = std::iter::once(entry.command.as_str())
                    .chain(entry.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("  {name}: {argv} [stdio]");
                for k in entry.env.keys() {
                    println!("    env: {k}=***");
                }
            }
            McpServerEntry::Http(entry) => {
                println!("  {name}: {} [{}]", entry.url, entry.r#type);
                for k in entry.headers.keys() {
                    println!("    header: {k}=***");
                }
            }
        }
    }
    Ok(())
}

fn status() -> anyhow::Result<()> {
    list()
}

fn get(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;
    let merged = super::config::load();
    let entry = merged
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{name}' not found"))?;

    println!("{name}:");
    match entry {
        McpServerEntry::Stdio(entry) => {
            println!("  Type: stdio");
            println!("  Command: {}", entry.command);
            println!("  Args: {}", entry.args.join(" "));
            for key in entry.env.keys() {
                println!("  Env: {key}=***");
            }
        }
        McpServerEntry::Http(entry) => {
            println!("  Type: {}", entry.r#type);
            println!("  URL: {}", entry.url);
            for key in entry.headers.keys() {
                println!("  Header: {key}=***");
            }
            if let Some(helper) = &entry.headers_helper {
                println!("  Headers helper: {helper}");
            }
            if let Some(oauth) = &entry.oauth {
                if oauth.client_id.is_some() {
                    println!("  OAuth client_id: configured");
                }
                if oauth.auth_server_metadata_url.is_some() {
                    println!("  OAuth auth_server_metadata_url: configured");
                }
            }
            let key = super::auth::server_key(name, entry);
            if let Some(auth) = oauth_repo().get(&key)? {
                if auth.client_id.is_some() {
                    println!("  Stored OAuth client_id: configured");
                }
                if auth.client_secret.is_some() {
                    println!("  Stored OAuth client_secret: configured");
                }
                if auth.authorization_endpoint.is_some() {
                    println!("  Stored OAuth authorization_endpoint: configured");
                }
                if auth.revocation_endpoint.is_some() {
                    println!("  Stored OAuth revocation_endpoint: configured");
                }
                if auth.resource_metadata_url.is_some() {
                    println!("  Stored OAuth resource_metadata_url: configured");
                }
                if auth.access_token.is_some() || auth.refresh_token.is_some() {
                    println!("  Stored OAuth tokens: present");
                }
            }
            let temp_config = McpConfig {
                servers: [(name.clone(), McpServerEntry::Http(entry.clone()))]
                    .into_iter()
                    .collect(),
            };
            let status_hint = super::manager::McpManager::start(&temp_config);
            let status_hint = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(status_hint);
            if let Some(status) = status_hint.statuses().get(name) {
                match status {
                    super::manager::McpStatus::NeedsAuth { .. } => {
                        println!("  Status: needs auth");
                    }
                    super::manager::McpStatus::Connected { .. } => {
                        println!("  Status: connected");
                    }
                    super::manager::McpStatus::Failed(err) => {
                        println!("  Status: failed ({err})");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Parse either:
/// - `luma mcp add [-e K=V]... <name> -- <cmd> [args...]`
/// - `luma mcp add --transport http|sse [-H K=V]... [--client-id ID] [--auth-server-metadata-url URL] [--headers-helper CMD] <name> <url>`
fn add(args: &[String]) -> anyhow::Result<()> {
    let mut env: HashMap<String, String> = HashMap::new();
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut transport = String::from("stdio");
    let mut client_id = None;
    let mut auth_server_metadata_url = None;
    let mut headers_helper = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-e" | "--env" => {
                let pair = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("-e requires KEY=VALUE"))?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("invalid env '{pair}', expected KEY=VALUE"))?;
                env.insert(k.to_owned(), v.to_owned());
                i += 2;
            }
            "-H" | "--header" => {
                let pair = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("-H requires KEY=VALUE"))?;
                let (k, v) = pair.split_once('=').ok_or_else(|| {
                    anyhow::anyhow!("invalid header '{pair}', expected KEY=VALUE")
                })?;
                headers.insert(k.to_owned(), v.to_owned());
                i += 2;
            }
            "--transport" => {
                transport = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--transport requires a value"))?
                    .to_owned();
                i += 2;
            }
            "--client-id" => {
                client_id = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--client-id requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            "--auth-server-metadata-url" => {
                auth_server_metadata_url = Some(
                    args.get(i + 1)
                        .ok_or_else(|| {
                            anyhow::anyhow!("--auth-server-metadata-url requires a value")
                        })?
                        .to_owned(),
                );
                i += 2;
            }
            "--headers-helper" => {
                headers_helper = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--headers-helper requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            _ => break,
        }
    }

    let name = args
        .get(i)
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?
        .to_owned();
    i += 1;

    let entry = match transport.as_str() {
        "stdio" => {
            if client_id.is_some() || auth_server_metadata_url.is_some() || headers_helper.is_some()
            {
                anyhow::bail!(
                    "oauth and headers-helper options are only supported for remote transports"
                );
            }
            let sep = args.get(i).map(String::as_str);
            if sep != Some("--") {
                anyhow::bail!("expected '--' before command (got {:?})", sep);
            }
            i += 1;

            let command = args
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("missing command after '--'"))?
                .to_owned();
            i += 1;

            let cmd_args: Vec<String> = args[i..].to_vec();
            McpServerEntry::Stdio(McpStdioServerEntry {
                r#type: None,
                command,
                args: cmd_args,
                env,
            })
        }
        "http" | "sse" => {
            if !env.is_empty() {
                anyhow::bail!("environment variables are only supported for stdio transport");
            }
            let url = args
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("missing URL for remote transport"))?
                .to_owned();
            if args.get(i + 1).is_some() {
                anyhow::bail!("unexpected extra arguments after URL");
            }
            let oauth = if client_id.is_some() || auth_server_metadata_url.is_some() {
                Some(McpOAuthConfig {
                    client_id,
                    auth_server_metadata_url,
                })
            } else {
                None
            };
            McpServerEntry::Http(McpHttpServerEntry {
                r#type: transport,
                url,
                headers,
                headers_helper,
                oauth,
            })
        }
        other => anyhow::bail!("unsupported transport '{other}' (supported: stdio, http, sse)"),
    };

    let mut config = load_user_config();
    let replaced = config.servers.insert(name.clone(), entry).is_some();
    save_user_config(&config)?;

    if replaced {
        println!(
            "updated MCP server '{name}' in {}",
            user_config_path().display()
        );
    } else {
        println!(
            "added MCP server '{name}' to {}",
            user_config_path().display()
        );
    }
    Ok(())
}

fn auth(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;
    let merged = super::config::load();
    let McpServerEntry::Http(entry) = merged
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{name}' not found"))?
    else {
        anyhow::bail!("browser auth is only supported for remote MCP servers");
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(super::oauth::ensure_authorizable(name, entry))?;
    runtime
        .block_on(super::oauth::authorize(name, entry))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("authorized MCP server '{name}'");
    Ok(())
}

fn set_secret(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;
    let merged = super::config::load();
    let McpServerEntry::Http(entry) = merged
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{name}' not found"))?
    else {
        anyhow::bail!("stored OAuth secrets are only supported for remote MCP servers");
    };

    let mut i = 1;
    let mut client_secret = None;
    let mut client_id = None;
    let mut token_endpoint = None;
    let mut auth_server_metadata_url = None;
    let mut access_token = None;
    let mut refresh_token = None;
    while i < args.len() {
        match args[i].as_str() {
            "--client-secret" => {
                client_secret = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--client-secret requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            "--client-id" => {
                client_id = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--client-id requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            "--token-endpoint" => {
                token_endpoint = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--token-endpoint requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            "--auth-server-metadata-url" => {
                auth_server_metadata_url = Some(
                    args.get(i + 1)
                        .ok_or_else(|| {
                            anyhow::anyhow!("--auth-server-metadata-url requires a value")
                        })?
                        .to_owned(),
                );
                i += 2;
            }
            "--access-token" => {
                access_token = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--access-token requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            "--refresh-token" => {
                refresh_token = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--refresh-token requires a value"))?
                        .to_owned(),
                );
                i += 2;
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    let key = super::auth::server_key(name, entry);
    let current = oauth_repo().get(&key)?;
    let record = super::auth::McpOAuthEntry {
        server_key: key,
        server_name: name.clone(),
        server_url: entry.url.clone(),
        client_id: client_id
            .or_else(|| entry.oauth.as_ref().and_then(|x| x.client_id.clone()))
            .or_else(|| current.as_ref().and_then(|x| x.client_id.clone())),
        client_secret: client_secret
            .or_else(|| current.as_ref().and_then(|x| x.client_secret.clone())),
        access_token: access_token
            .or_else(|| current.as_ref().and_then(|x| x.access_token.clone())),
        refresh_token: refresh_token
            .or_else(|| current.as_ref().and_then(|x| x.refresh_token.clone())),
        auth_server_metadata_url: auth_server_metadata_url
            .or_else(|| {
                entry
                    .oauth
                    .as_ref()
                    .and_then(|x| x.auth_server_metadata_url.clone())
            })
            .or_else(|| {
                current
                    .as_ref()
                    .and_then(|x| x.auth_server_metadata_url.clone())
            }),
        token_endpoint: token_endpoint
            .or_else(|| current.as_ref().and_then(|x| x.token_endpoint.clone())),
        resource_metadata_url: current
            .as_ref()
            .and_then(|x| x.resource_metadata_url.clone()),
        authorization_endpoint: current
            .as_ref()
            .and_then(|x| x.authorization_endpoint.clone()),
        revocation_endpoint: current.as_ref().and_then(|x| x.revocation_endpoint.clone()),
        scopes: current.as_ref().and_then(|x| x.scopes.clone()),
        expires_at_unix_ms: current.as_ref().and_then(|x| x.expires_at_unix_ms),
    };
    oauth_repo().upsert(&record)?;
    println!("stored OAuth secret for MCP server '{name}'");
    Ok(())
}

fn clear_secret(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;
    let merged = super::config::load();
    let McpServerEntry::Http(entry) = merged
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{name}' not found"))?
    else {
        anyhow::bail!("stored OAuth secrets are only supported for remote MCP servers");
    };
    let key = super::auth::server_key(name, entry);
    oauth_repo().remove(&key)?;
    println!("cleared OAuth secret for MCP server '{name}'");
    Ok(())
}

fn revoke(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;
    let merged = super::config::load();
    let McpServerEntry::Http(entry) = merged
        .servers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("MCP server '{name}' not found"))?
    else {
        anyhow::bail!("revoke is only supported for remote MCP servers");
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(super::oauth::revoke(name, entry))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("revoked OAuth tokens for MCP server '{name}'");
    Ok(())
}

fn remove(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;

    let mut config = load_user_config();
    if let Some(entry) = config.servers.remove(name) {
        if let McpServerEntry::Http(http) = &entry {
            let key = super::auth::server_key(name, http);
            let _ = oauth_repo().remove(&key);
        }
        save_user_config(&config)?;
        println!("removed MCP server '{name}'");
    } else {
        anyhow::bail!("MCP server '{name}' not found in user config");
    }
    Ok(())
}
