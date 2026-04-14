#![warn(clippy::cast_lossless)]

/// Debug log to temp dir — enabled by LUMA_DEBUG=1.
#[macro_export]
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if std::env::var("LUMA_DEBUG").is_ok() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true).append(true)
                .open(std::env::temp_dir().join("luma.log"))
            {
                let _ = writeln!(f, "[{:.3}] {}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default().as_secs_f64() % 100000.0,
                    format!($($arg)*)
                );
            }
        }
    };
}

mod cli_login;
mod config;
mod core;
mod event;
mod event_bus;
mod provider;
mod tool;
mod tui;
mod util;

use std::process::Command;

#[tokio::main]
async fn main() {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Terminal restore is handled by termina's panic hook (set in enter_terminal).
        // Write crash log for diagnostics.
        let crash_path = std::env::temp_dir().join("luma-crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_path)
        {
            use std::io::Write;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(f, "[{ts}] {info}");
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{bt}\n");
        }

        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(std::string::String::as_str);

    match cmd {
        Some("sync") => {
            println!("syncing models...");
            match config::models::sync().await {
                Ok(count) => println!("synced {count} models"),
                Err(e) => {
                    eprintln!("sync failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("auth") => {
            for provider in [
                config::auth::AuthVendor::Anthropic,
                config::auth::AuthVendor::OpenAI,
                config::auth::AuthVendor::OpenCodeGo,
                config::auth::AuthVendor::Kiro,
            ] {
                let name = provider.as_str();
                match config::auth::resolve(provider).await {
                    Ok(auth) => {
                        let kind = if auth.is_oauth { "oauth" } else { "apikey" };
                        if provider == config::auth::AuthVendor::Kiro {
                            match probe_kiro_chat(&auth).await {
                                Ok(()) => println!("{name}: {kind} (ok, chat probe ok)"),
                                Err(e) => {
                                    println!("{name}: {kind} (resolve ok, chat probe failed: {e})")
                                }
                            }
                        } else {
                            println!("{name}: {kind} (ok)");
                        }
                    }
                    Err(e) => println!("{name}: {e}"),
                }
            }
        }
        Some("login") => {
            if let Err(e) = cli_login::run(args.get(2).map(std::string::String::as_str)).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some("accounts") => {
            let accounts = config::auth::list_accounts();
            if accounts.is_empty() {
                println!("no accounts · run 'luma login' to add one");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for a in accounts {
                    let status = match a.health {
                        config::auth::AccountHealth::Ok => "ok",
                        config::auth::AccountHealth::Cooldown { .. } => "cooling",
                        config::auth::AccountHealth::NeedsRelogin => "re-login",
                    };
                    let email = a.email.as_deref().unwrap_or("-");
                    println!("  {}  {}  {}", a.label, status, email);
                }
                let _ = now; // used above via Cooldown pattern if needed later
            }
        }
        Some("update") => {
            if let Err(e) = self_update() {
                eprintln!("update failed: {e}");
                std::process::exit(1);
            }
        }
        Some("audit") => match args.get(2).map(std::string::String::as_str) {
            Some("sessions") => {
                let limit = args
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(30);
                let summary = crate::core::audit::audit_sessions(limit);
                println!("sessions scanned:                  {}", summary.sessions_scanned);
                println!("sessions with project instructions: {}", summary.sessions_with_project_instructions);
                println!("sessions with skill loads:          {}", summary.sessions_with_skill_loads);
                println!("mixed local/remote source sessions: {}", summary.mixed_local_remote_source_sessions);
                println!("premature external research sessions: {}", summary.premature_external_research_sessions);
                println!("edited without verify sessions:    {}", summary.edited_without_verify_sessions);
                println!("bash verify commands:             {}", summary.bash_verify_commands);
                println!("bash file-inspection commands:    {}", summary.bash_file_inspection_commands);
            }
            _ => {
                eprintln!("usage: luma audit sessions [limit]");
                std::process::exit(1);
            }
        },
        Some("version" | "--version" | "-v") => println!("luma {}", env!("CARGO_PKG_VERSION")),
        Some("help" | "--help" | "-h") => {
            println!(
                "luma - lightweight coding agent\n\nusage:\n  luma                     start TUI\n  luma sync                sync models\n  luma auth                show resolved auth per provider\n  luma login [provider]    add an account (anthropic|openai|opencode-go); omit for picker\n  luma accounts            list accounts in the pool\n  luma update              update to latest\n  luma version             show version"
            );
        }
        Some(unknown) => {
            eprintln!("unknown command: {unknown}\nrun 'luma help'");
            std::process::exit(1);
        }
        None => {
            if !config::models::has_synced() {
                println!("first run — syncing models...");
                if let Err(e) = config::models::sync().await {
                    eprintln!("sync failed: {e}");
                    std::process::exit(1);
                }
                println!("done");
            } else {
                // Stale-snapshot or post-upgrade refresh. Non-blocking —
                // the UI launches against the existing snapshot; newly
                // rolled models show up on the next start.
                config::models::sync_in_background();
            }

            let env_context = build_env_context();
            let app = tui::app::App::new(env_context);
            if let Err(e) = app.run().await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn probe_kiro_chat(auth: &config::auth::Credential) -> anyhow::Result<()> {
    let profile_arn = auth
        .profile_arn
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing profile_arn"))?;
    let body = serde_json::json!({
        "conversationState": {
            "conversationId": "00000000-0000-4000-8000-000000000001",
            "chatTriggerType": "MANUAL",
            "history": [],
            "currentMessage": {
                "userInputMessage": {
                    "content": "ping",
                    "modelId": "auto",
                    "origin": "KIRO_CLI",
                    "userInputMessageContext": {
                        "envState": {
                            "operatingSystem": std::env::consts::OS,
                            "currentWorkingDirectory": std::env::current_dir()
                                .unwrap_or_default()
                                .display()
                                .to_string(),
                        }
                    }
                }
            }
        },
        "profileArn": profile_arn,
    });

    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .post("https://q.us-east-1.amazonaws.com/")
        .header("Authorization", format!("Bearer {}", auth.token))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header(
            "X-Amz-Target",
            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
        )
        .header(
            "User-Agent",
            "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 os/macos lang/rust/1.92.0 app/AmazonQ-For-CLI",
        )
        .header(
            "X-Amz-User-Agent",
            "aws-sdk-rust/1.3.14 ua/2.1 api/codewhispererstreaming/0.1.14474 os/macos lang/rust/1.92.0 app/AmazonQ-For-CLI",
        )
        .header("X-Amzn-Codewhisperer-Optout", "false")
        .json(&body)
        .send()
        .await?;

    if resp.status().is_success() {
        return Ok(());
    }

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let detail: String = body.chars().take(200).collect();
    anyhow::bail!("HTTP {}: {}", status.as_u16(), detail);
}

/// Self-update: download and run install script.
#[cfg(unix)]
fn self_update() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("current: v{current}");
    println!("updating...");
    let status = Command::new("sh")
        .arg("-c")
        .arg("curl -fsSL https://raw.githubusercontent.com/nghyane/luma/master/install.sh | sh")
        .status()?;
    if !status.success() {
        anyhow::bail!("install script failed");
    }
    Ok(())
}

#[cfg(windows)]
fn self_update() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("current: v{current}");
    println!("updating...");
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "[Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; irm https://raw.githubusercontent.com/nghyane/luma/master/install.ps1 | iex",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("install script failed");
    }
    Ok(())
}

fn build_env_context() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let shell = std::env::var("SHELL")
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_else(|_| "unknown".into());

    // Git info
    let is_git = cmd_ok(&cwd, "git", &["rev-parse", "--is-inside-work-tree"]);
    let git_branch = if is_git {
        cmd_stdout(&cwd, "git", &["rev-parse", "--abbrev-ref", "HEAD"])
    } else {
        None
    };
    let git_remote = if is_git {
        cmd_stdout(&cwd, "git", &["remote", "get-url", "origin"])
    } else {
        None
    };

    // Detect CLIs based on project files — only check tools relevant to this project.
    let mut cli_candidates: Vec<(&str, &str)> = vec![
        ("rg", "--version"),
        ("git", "--version"),
        ("gh", "--version"),
    ];

    let project_markers: &[(&str, &[(&str, &str)])] = &[
        (
            "Cargo.toml",
            &[("cargo", "--version"), ("rustc", "--version")],
        ),
        (
            "package.json",
            &[
                ("node", "--version"),
                ("npm", "--version"),
                ("pnpm", "--version"),
                ("yarn", "--version"),
                ("bun", "--version"),
            ],
        ),
        ("Dockerfile", &[("docker", "--version")]),
        ("docker-compose.yml", &[("docker", "--version")]),
        (
            "requirements.txt",
            &[
                ("python3", "--version"),
                ("python", "--version"),
                ("pip3", "--version"),
                ("pip", "--version"),
            ],
        ),
        (
            "pyproject.toml",
            &[
                ("python3", "--version"),
                ("python", "--version"),
                ("pip3", "--version"),
                ("pip", "--version"),
            ],
        ),
        ("go.mod", &[("go", "version")]),
        ("Makefile", &[("make", "--version")]),
    ];

    let mut seen = std::collections::HashSet::new();
    for (marker, cmds) in project_markers {
        if cwd.join(marker).exists() {
            for &(cmd, flag) in *cmds {
                if seen.insert(cmd) {
                    cli_candidates.push((cmd, flag));
                }
            }
        }
    }

    let mut tools = Vec::new();
    for (cmd, flag) in &cli_candidates {
        if let Ok(out) = Command::new(cmd).arg(flag).output()
            && out.status.success()
        {
            // Record presence only, not `--version` output. Version
            // strings bump whenever the user updates a CLI and would
            // otherwise invalidate the system-prompt cache prefix on
            // every agent session after an upgrade. Agents that need a
            // specific version can run `{tool} --version` via Bash.
            tools.push((*cmd).to_owned());
        }
    }

    // Build git line
    let git_info = if is_git {
        let mut parts = vec!["yes".to_owned()];
        if let Some(b) = &git_branch {
            parts.push(format!("branch={b}"));
        }
        if let Some(r) = &git_remote {
            parts.push(format!("remote={r}"));
        }
        parts.join(", ")
    } else {
        "no".into()
    };

    // Drop the Date line and volatile CLI version strings from the
    // system-prompt <env> block: every midnight (or CLI upgrade) they
    // invalidate the cached prefix Anthropic charges ~10× less to
    // re-read. An agent that genuinely needs the date can call Bash
    // `date` — one pull beats a daily cache rebuild on every session.

    format!(
        "\n<env>\n  OS: {} {}\n  Shell: {shell}\n  CWD: {}\n  Git: {git_info}\n  CLI: {}\n</env>\nShell commands execute in CWD by default — do not prefix `cd <cwd> && …`; paths inside CWD can be relative.",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cwd.display(),
        tools.join(", "),
    )
}

fn cmd_ok(cwd: &std::path::Path, cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a command and return trimmed stdout on success.
fn cmd_stdout(cwd: &std::path::Path, cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}
