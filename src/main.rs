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

mod config;
mod core;
mod event;
mod provider;
mod tool;
mod tui;

use std::process::Command;

#[tokio::main]
async fn main() {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore terminal first — must happen before printing.
        let _ = crossterm::terminal::disable_raw_mode();
        {
            use std::io::Write;
            let _ = write!(std::io::stderr(), "\x1b[?1007h");
        }
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen,
        );

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
    let cmd = args.get(1).map(|s| s.as_str());

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
                config::auth::AuthProvider::Anthropic,
                config::auth::AuthProvider::OpenAI,
            ] {
                let name = match provider {
                    config::auth::AuthProvider::Anthropic => "anthropic",
                    config::auth::AuthProvider::OpenAI => "openai",
                };
                match config::auth::resolve(provider).await {
                    Ok(auth) => println!(
                        "{name}: {} (ok)",
                        if auth.is_oauth { "oauth" } else { "apikey" }
                    ),
                    Err(e) => println!("{name}: {e}"),
                }
            }
        }
        Some("update") => {
            if let Err(e) = self_update() {
                eprintln!("update failed: {e}");
                std::process::exit(1);
            }
        }
        Some("version" | "--version" | "-v") => println!("luma {}", env!("CARGO_PKG_VERSION")),
        Some("help" | "--help" | "-h") => {
            println!(
                "luma - lightweight coding agent\n\nusage:\n  luma              start TUI\n  luma sync         sync models\n  luma auth         show auth\n  luma update       update to latest\n  luma version      version"
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
            &[("python3", "--version"), ("python", "--version"), ("pip3", "--version"), ("pip", "--version")],
        ),
        (
            "pyproject.toml",
            &[("python3", "--version"), ("python", "--version"), ("pip3", "--version"), ("pip", "--version")],
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
            let ver = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or(cmd)
                .to_owned();
            tools.push(format!("{cmd} ({ver})"));
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

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple date from unix timestamp (no chrono dep)
    let days = now / 86400;
    let (year, month, day) = epoch_days_to_ymd(days);
    let date = format!("{year}-{month:02}-{day:02}");

    format!(
        "\n<env>\n  OS: {} {}\n  Shell: {shell}\n  CWD: {}\n  Git: {git_info}\n  Date: {date}\n  CLI: {}\n</env>",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cwd.display(),
        tools.join(", "),
    )
}

/// Convert days since epoch to (year, month, day).
fn epoch_days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm (Howard Hinnant)
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Run a command and check success.
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
