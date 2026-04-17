use super::config::{McpConfig, McpServerEntry};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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
         \x20 luma mcp list                               list configured servers\n\
         \x20 luma mcp status                             same as list, with sources\n\
         \x20 luma mcp add <name> -- <cmd> [args...]      add a stdio server\n\
         \x20 luma mcp add -e KEY=VAL <name> -- <cmd>     with environment variables\n\
         \x20 luma mcp remove <name>                      remove a server\n\
         \n\
         examples:\n\
         \x20 luma mcp add github -- npx -y @modelcontextprotocol/server-github\n\
         \x20 luma mcp add -e GITHUB_TOKEN=ghp_xxx github -- npx -y @modelcontextprotocol/server-github\n\
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
        return Ok(());
    }
    println!("MCP servers ({}):", merged.servers.len());
    for (name, entry) in &merged.servers {
        let argv = std::iter::once(entry.command.as_str())
            .chain(entry.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        println!("  {name}: {argv}");
        for k in entry.env.keys() {
            println!("    env: {k}=***");
        }
    }
    Ok(())
}

fn status() -> anyhow::Result<()> {
    list()
}

/// Parse `luma mcp add [-e K=V]... <name> -- <cmd> [args...]`.
fn add(args: &[String]) -> anyhow::Result<()> {
    let mut env: HashMap<String, String> = HashMap::new();
    let mut i = 0;

    // Pre-positional env flags.
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
            _ => break,
        }
    }

    let name = args
        .get(i)
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?
        .to_owned();
    i += 1;

    // Expect `--` separator.
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

    let entry = McpServerEntry {
        r#type: None,
        command,
        args: cmd_args,
        env,
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

fn remove(args: &[String]) -> anyhow::Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing server name"))?;

    let mut config = load_user_config();
    if config.servers.remove(name).is_some() {
        save_user_config(&config)?;
        println!("removed MCP server '{name}'");
    } else {
        anyhow::bail!("MCP server '{name}' not found in user config");
    }
    Ok(())
}
