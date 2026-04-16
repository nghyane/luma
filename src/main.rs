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

mod acp;
mod auth;
mod cli_login;
mod config;
mod core;
mod event;
mod event_bus;
mod provider;
mod tool;
mod tui;
mod update;
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

    // ACP server mode: `luma --acp`
    if args.iter().any(|a| a == "--acp") {
        if let Err(e) = acp::bridge::run().await {
            eprintln!("ACP error: {e}");
            std::process::exit(1);
        }
        return;
    }

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
            use auth::domain::AccountHealth;
            use auth::repo::SqliteAuthRepository;
            use auth::service::AuthService;
            let svc = AuthService::new(SqliteAuthRepository::with_default_path());
            let accounts = svc.list_accounts().unwrap_or_default();
            if accounts.is_empty() {
                println!("no accounts · run 'luma login' to add one");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                for a in accounts {
                    let status = match a.health {
                        AccountHealth::Active => "ok",
                        AccountHealth::CoolingDown { .. } => "cooling",
                        AccountHealth::NeedsRelogin { .. } => "re-login",
                        AccountHealth::Disabled => "disabled",
                    };
                    let email = a.email.as_deref().unwrap_or("-");
                    println!("  {}  {}  {}", a.display_name, status, email);
                }
                let _ = now;
            }
        }
        Some("update") => {
            if let Err(e) = self_update().await {
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
                println!(
                    "sessions scanned:                  {}",
                    summary.sessions_scanned
                );
                println!(
                    "sessions with project instructions: {}",
                    summary.sessions_with_project_instructions
                );
                println!(
                    "sessions with skill loads:          {}",
                    summary.sessions_with_skill_loads
                );
                println!(
                    "wrong source sessions:              {}",
                    summary.wrong_source_sessions
                );
                println!(
                    "premature external research sessions: {}",
                    summary.premature_external_research_sessions
                );
                println!(
                    "missing verification sessions:      {}",
                    summary.missing_verification_sessions
                );
                println!(
                    "bash verify commands:              {}",
                    summary.bash_verify_commands
                );
                println!(
                    "shell local read commands:         {}",
                    summary.shell_local_read_commands
                );
                println!(
                    "shell file counting commands:      {}",
                    summary.shell_file_counting_commands
                );
                println!(
                    "shell verify output slicing cmds:  {}",
                    summary.shell_verify_output_slicing_commands
                );
                println!(
                    "shell patch-style search commands: {}",
                    summary.shell_patch_style_search_commands
                );
            }
            Some("incidents") => {
                let limit = args
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(30);
                for incident in crate::core::audit::audit_incidents(limit) {
                    println!(
                        "{}	{}	{}	{}	{}	{}",
                        incident.session_id,
                        incident.failure_type,
                        incident.severity,
                        incident.task_family,
                        incident.subsystem,
                        incident.title
                    );
                }
            }
            Some("packets") => {
                let limit = args
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(30);
                for packet in crate::core::audit::audit_packets(limit) {
                    println!("session: {}", packet.session_id);
                    println!("title:   {}", packet.title);
                    println!("task:    {}", packet.task_preview);
                    println!("family:  {}", packet.task_family);
                    println!("detector:{}", packet.detector_version);
                    println!("severity:{}", packet.severity);
                    println!("reviewer:{}", packet.reviewer_eligibility);
                    println!("source:  {}", packet.source_of_truth_classification);
                    println!("failures:");
                    for failure in packet.failure_types {
                        println!("  - {}", failure);
                    }
                    println!("tool sequence:");
                    for tool in packet.tool_sequence_summary {
                        println!("  - {}", tool);
                    }
                    println!("excerpts:");
                    for excerpt in packet.representative_excerpts {
                        println!("  - {}", excerpt);
                    }
                    println!("spans:");
                    for span in packet.representative_spans {
                        println!(
                            "  - message={} block={} kind={} preview={}",
                            span.message_index, span.block_index, span.kind, span.preview
                        );
                    }
                    println!(
                        "counts: tools={} local_reads={} remote_uses={} edits={} verify_signals={}",
                        packet.supporting_counts.tool_uses,
                        packet.supporting_counts.local_reads,
                        packet.supporting_counts.remote_uses,
                        packet.supporting_counts.edits,
                        packet.supporting_counts.verify_signals
                    );
                    println!();
                }
            }
            Some("clusters") => {
                let limit = args
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(30);
                for cluster in crate::core::audit::audit_clusters(limit) {
                    println!(
                        "{}	{}	{}	{}	{}	{}",
                        cluster.cluster_key,
                        cluster.failure_type,
                        cluster.count,
                        cluster.highest_severity,
                        cluster.task_family,
                        cluster.subsystem
                    );
                }
            }
            Some("show") => {
                let Some(session_id) = args.get(3) else {
                    eprintln!("usage: luma audit show <session-id>");
                    std::process::exit(1);
                };
                let Some(detail) = crate::core::audit::audit_show(session_id) else {
                    eprintln!("session not found: {session_id}");
                    std::process::exit(1);
                };
                println!("session: {}", detail.session_id);
                println!("title:   {}", detail.title);
                println!("task:    {}", detail.task_preview);
                println!("task family: {}", detail.task_family);
                println!("detector:    {}", detail.detector_version);
                println!("severity:    {}", detail.severity);
                println!("reviewer:    {}", detail.reviewer_eligibility);
                println!("source:      {}", detail.source_of_truth_classification);
                println!(
                    "failures:{}",
                    if detail.failure_types.is_empty() {
                        " none"
                    } else {
                        ""
                    }
                );
                for failure in detail.failure_types {
                    println!("  - {}", failure);
                }
                if let Some(local_read) = detail.representative_local_read {
                    println!("representative local read:");
                    println!("  - {}", local_read);
                }
                if let Some(remote_use) = detail.representative_remote_use {
                    println!("representative remote use:");
                    println!("  - {}", remote_use);
                }
                if let Some(edit) = detail.representative_edit {
                    println!("representative edit:");
                    println!("  - {}", edit);
                }
                if let Some(verify) = detail.representative_verify {
                    println!("representative verify:");
                    println!("  - {}", verify);
                }
                println!("tool uses:");
                for tool in detail.tool_uses.into_iter().take(25) {
                    println!("  - {}", tool);
                }
            }
            _ => {
                eprintln!(
                    "usage: luma audit sessions [limit]
       luma audit incidents [limit]
       luma audit packets [limit]
       luma audit clusters [limit]
       luma audit show <session-id>"
                );
                std::process::exit(1);
            }
        },
        Some("improve") => match args.get(2).map(std::string::String::as_str) {
            Some("propose") => {
                if args.get(3).map(std::string::String::as_str) != Some("--session") {
                    eprintln!("usage: luma improve propose --session <session-id>");
                    std::process::exit(1);
                }
                let Some(session_id) = args.get(4) else {
                    eprintln!("usage: luma improve propose --session <session-id>");
                    std::process::exit(1);
                };
                let Some(proposal) = crate::core::improve::propose_from_session(session_id) else {
                    eprintln!("session not found: {session_id}");
                    std::process::exit(1);
                };
                println!("suggested route: {}", proposal.route);
                println!("confidence:      {}", proposal.confidence);
                println!("affected layer: {}", proposal.affected_layer);
                if proposal.target_layers.is_empty() {
                    println!("target layers:   none");
                } else {
                    println!("target layers:");
                    for layer in proposal.target_layers {
                        println!("  - {}", layer);
                    }
                }
                println!("reason:          {}", proposal.reason);
                println!("note:            {}", proposal.note);
                println!("validation:      {}", proposal.suggested_validation);
            }
            _ => {
                eprintln!("usage: luma improve propose --session <session-id>");
                std::process::exit(1);
            }
        },
        Some("version" | "--version" | "-v") => println!("luma {}", env!("CARGO_PKG_VERSION")),
        Some("export") => {
            use auth::repo::{AuthRepository, SqliteAuthRepository};
            let repo = SqliteAuthRepository::with_default_path();
            match repo.load() {
                Ok(store) => {
                    let json = serde_json::to_string_pretty(&store).unwrap();
                    use base64::Engine;
                    println!(
                        "{}",
                        base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
                    );
                }
                Err(e) => {
                    eprintln!("export failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("import") => {
            use base64::Engine;
            let encoded = if let Some(arg) = args.get(2) {
                arg.clone()
            } else {
                eprintln!("paste base64 auth string (then press Enter):");
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).unwrap_or_else(|e| {
                    eprintln!("read error: {e}");
                    std::process::exit(1);
                });
                buf
            };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .unwrap_or_else(|e| {
                    eprintln!("invalid base64: {e}");
                    std::process::exit(1);
                });
            let store: auth::repo::AuthStore = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("invalid auth data: {e}");
                std::process::exit(1);
            });
            if store.accounts.is_empty() {
                eprintln!("no accounts found in payload");
                std::process::exit(1);
            }
            eprintln!("found {} account(s):", store.accounts.len());
            for acc in &store.accounts {
                eprintln!("  · {} ({})", acc.display_name, acc.key.vendor.as_str());
            }
            eprint!("import? [Y/n] ");
            use std::io::Write;
            std::io::stderr().flush().ok();
            let mut confirm = String::new();
            std::io::stdin().read_line(&mut confirm).ok();
            if matches!(confirm.trim(), "n" | "N" | "no") {
                eprintln!("cancelled");
                std::process::exit(0);
            }
            use auth::repo::SqliteAuthRepository;
            SqliteAuthRepository::with_default_path()
                .merge(&store.accounts)
                .unwrap_or_else(|e| {
                    eprintln!("import failed: {e}");
                    std::process::exit(1);
                });
            println!("imported {} accounts", store.accounts.len());
        }
        Some("help" | "--help" | "-h") => {
            println!(
                "luma - lightweight coding agent\n\nusage:\n  luma                     start TUI\n  luma sync                sync models\n  luma auth                show resolved auth per provider\n  luma login [provider]    add an account; omit for picker\n  luma accounts            list accounts in the pool\n  luma export              print auth as base64 (share via chat/slack)\n  luma import [string]     import auth (interactive paste if no arg)\n  luma update              update to latest\n  luma version             show version"
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

/// Update luma to the latest GitHub release using native download,
/// checksum verification, extraction, and atomic install.
async fn self_update() -> anyhow::Result<()> {
    crate::update::self_update::run().await
}

pub fn build_env_context() -> String {
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
