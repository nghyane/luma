//! `luma login` interactive flow.
//!
//! Single-binary subcommand; no TUI app integration. Uses raw-mode via
//! `termina` to drive an arrow-key provider picker, then dispatches:
//!
//! * OAuth providers → existing PKCE flow in `config::auth::login`.
//! * API-key providers → inline paste prompt, then
//!   `config::auth::upsert_api_key`.
//!
//! The menu restores cooked mode before printing results or spawning the
//! browser so users see normal terminal output. Raw mode only bounds the
//! picker loop itself.

use crate::config::auth::{self, AuthVendor};
use anyhow::{Context, Result};
use std::io::{self, Write};
use termina::{
    PlatformTerminal, Terminal,
    event::{KeyCode, KeyEventKind, Modifiers},
};

/// Entry point for `luma login [<arg>]`.
///
/// If `arg` is `Some`, route directly (CI-friendly shortcut); otherwise
/// show the picker. The picker always echoes the final result in cooked
/// mode regardless of which path was taken.
pub async fn run(arg: Option<&str>) -> Result<()> {
    let choice = match arg {
        Some(name) => Choice::parse(name)?,
        None => pick_choice()?,
    };

    match choice {
        Choice::Oauth(vendor) => dispatch_oauth(vendor).await,
        Choice::ApiKey(vendor) => dispatch_api_key(vendor),
    }
}

/// A selected provider + auth flow. Each variant carries exactly the
/// data the dispatcher needs; vendor parsing lives in `Choice::parse` so
/// CLI arg handling and menu selection share one source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Oauth(AuthVendor),
    ApiKey(AuthVendor),
}

impl Choice {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "anthropic" | "claude" => Ok(Self::Oauth(AuthVendor::Anthropic)),
            "openai" | "codex" => Ok(Self::Oauth(AuthVendor::OpenAI)),
            "opencode-go" => Ok(Self::ApiKey(AuthVendor::OpenCodeGo)),
            other => anyhow::bail!(
                "unknown provider: {other}\nusage: luma login [anthropic|openai|opencode-go]"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Oauth(AuthVendor::Anthropic) => "Claude        (OAuth · Claude.ai subscriber)",
            Self::Oauth(AuthVendor::OpenAI) => "Codex         (OAuth · ChatGPT)",
            Self::ApiKey(AuthVendor::OpenCodeGo) => "OpenCode Go   (API key)",
            // Placeholders — no other combinations wired today.
            Self::Oauth(AuthVendor::OpenCodeGo) => "OpenCode Go   (OAuth)",
            Self::ApiKey(AuthVendor::Anthropic) => "Anthropic     (API key)",
            Self::ApiKey(AuthVendor::OpenAI) => "OpenAI        (API key)",
        }
    }
}

/// All selectable choices, in display order.
const CHOICES: &[Choice] = &[
    Choice::Oauth(AuthVendor::Anthropic),
    Choice::Oauth(AuthVendor::OpenAI),
    Choice::ApiKey(AuthVendor::OpenCodeGo),
];

/// Open an arrow-key menu on stderr and return the selected choice.
/// Restores the terminal before returning, even on error.
fn pick_choice() -> Result<Choice> {
    let mut term = PlatformTerminal::new().context("could not open terminal for login menu")?;
    term.enter_raw_mode()
        .context("could not enter raw mode for login menu")?;
    let reader = term.event_reader();

    let result = run_picker(&reader);

    // Always restore cooked mode, even on error — don't leave the user
    // stuck with no echo.
    let _ = term.enter_cooked_mode();
    // Clear picker frame and park the cursor at column 0 before the
    // caller prints anything.
    let mut err = io::stderr();
    let _ = write!(err, "\r\x1b[J");
    let _ = err.flush();

    result
}

fn run_picker(reader: &termina::EventReader) -> Result<Choice> {
    let mut selected: usize = 0;
    // First render — newline so the block does not collide with any
    // shell prompt above.
    writeln!(io::stderr())?;
    render_menu(selected, false)?;

    loop {
        let raw = reader
            .read(|_| true)
            .context("terminal read failed in login menu")?;
        let termina::Event::Key(k) = raw else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 < CHOICES.len() {
                    selected += 1;
                }
            }
            KeyCode::Enter => return Ok(CHOICES[selected]),
            KeyCode::Escape | KeyCode::Char('q') => anyhow::bail!("cancelled"),
            KeyCode::Char('c') if k.modifiers.contains(Modifiers::CONTROL) => {
                anyhow::bail!("cancelled");
            }
            _ => continue,
        }
        render_menu(selected, true)?;
    }
}

/// Render the menu. On redraw, move the cursor back to the top of the
/// previously drawn block so each frame overwrites the last cleanly.
fn render_menu(selected: usize, redraw: bool) -> Result<()> {
    let lines = 3 + CHOICES.len() + 2; // title+blank + items + blank+help

    let mut out = io::stderr();
    if redraw {
        // Cursor up `lines` lines, then clear below.
        write!(out, "\x1b[{lines}A\x1b[J")?;
    }
    writeln!(out, "luma login — select provider")?;
    writeln!(out)?;
    for (i, choice) in CHOICES.iter().enumerate() {
        let arrow = if i == selected { ">" } else { " " };
        writeln!(out, " {arrow} {}", choice.label())?;
    }
    writeln!(out)?;
    writeln!(out, "   ↑/↓ move · enter select · esc cancel")?;
    out.flush()?;
    Ok(())
}

async fn dispatch_oauth(vendor: AuthVendor) -> Result<()> {
    eprintln!("logging in to {}…", vendor.as_str());
    match auth::login(vendor).await {
        Ok(outcome) => {
            let who = outcome.email.as_deref().unwrap_or(outcome.label.as_str());
            println!(
                "signed in as {who} ({}) · provider: {}",
                outcome.label,
                outcome.provider.as_str()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("login failed: {e}");
            std::process::exit(1);
        }
    }
}

fn dispatch_api_key(vendor: AuthVendor) -> Result<()> {
    eprint!("paste {} API key: ", vendor.as_str());
    io::stderr().flush().ok();

    let mut key = String::new();
    io::stdin()
        .read_line(&mut key)
        .context("could not read API key from stdin")?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("no key provided");
    }

    let label = auth::upsert_api_key(vendor, key);
    println!("saved · {label}");
    Ok(())
}
