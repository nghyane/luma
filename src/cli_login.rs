//! `luma login` interactive flow.

use crate::auth::repo::FileAuthRepository;
use crate::auth::service::AuthService;
use crate::config::auth::AuthVendor;
use anyhow::{Context, Result};
use std::io::{self, Write};
use termina::{
    PlatformTerminal, Terminal,
    event::{KeyCode, KeyEventKind, Modifiers},
};

pub async fn run(arg: Option<&str>) -> Result<()> {
    let choice = match arg {
        Some(name) => Choice::parse(name)?,
        None => pick_choice()?,
    };
    let svc = AuthService::new(FileAuthRepository::with_default_path());
    match choice {
        Choice::Oauth(vendor) => {
            eprintln!("logging in to {}…", vendor.as_str());
            let view = svc.login(vendor.into()).await?;
            let who = view.email.as_deref().unwrap_or(view.display_name.as_str());
            println!("signed in as {who} ({}) · provider: {}", view.display_name, view.vendor.as_str());
        }
        Choice::ApiKey(vendor) => {
            eprint!("paste {} API key: ", vendor.as_str());
            io::stderr().flush().ok();
            let mut key = String::new();
            io::stdin().read_line(&mut key).context("could not read API key")?;
            let key = key.trim();
            if key.is_empty() { anyhow::bail!("no key provided"); }
            let view = svc.save_api_key(vendor.into(), key)?;
            println!("saved · {}", view.display_name);
        }
        Choice::KiroBuilderId => {
            eprintln!("logging in via Builder ID…");
            let view = svc.login_device("https://view.awsapps.com/start", "us-east-1").await?;
            let who = view.email.as_deref().unwrap_or(view.display_name.as_str());
            println!("signed in as {who} · provider: kiro (builder-id)");
        }
        Choice::KiroIdc => {
            let (start_url, region) = prompt_idc_params()?;
            eprintln!("logging in via IAM Identity Center…");
            let view = svc.login_device(&start_url, &region).await?;
            let who = view.email.as_deref().unwrap_or(view.display_name.as_str());
            println!("signed in as {who} · provider: kiro (idc)");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Oauth(AuthVendor),
    ApiKey(AuthVendor),
    KiroBuilderId,
    KiroIdc,
}

impl Choice {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "anthropic" | "claude" => Ok(Self::Oauth(AuthVendor::Anthropic)),
            "openai" | "codex" => Ok(Self::Oauth(AuthVendor::OpenAI)),
            "opencode-go" => Ok(Self::ApiKey(AuthVendor::OpenCodeGo)),
            "kiro" => Ok(Self::Oauth(AuthVendor::Kiro)),
            "kiro-idc" | "idc" | "awsidc" => Ok(Self::KiroIdc),
            "kiro-builder" | "builder-id" => Ok(Self::KiroBuilderId),
            other => anyhow::bail!(
                "unknown provider: {other}\nusage: luma login [anthropic|openai|kiro|kiro-idc|builder-id|opencode-go]"
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Oauth(AuthVendor::Anthropic) => "Claude        (OAuth · Claude.ai subscriber)",
            Self::Oauth(AuthVendor::OpenAI) => "Codex         (OAuth · ChatGPT)",
            Self::Oauth(AuthVendor::Kiro) => "Kiro          (OAuth · Google/GitHub)",
            Self::KiroBuilderId => "Kiro          (Builder ID · free)",
            Self::KiroIdc => "Kiro          (IAM Identity Center · pro)",
            Self::ApiKey(AuthVendor::OpenCodeGo) => "OpenCode Go   (API key)",
            Self::Oauth(AuthVendor::OpenCodeGo) => "OpenCode Go   (OAuth)",
            Self::ApiKey(AuthVendor::Anthropic) => "Anthropic     (API key)",
            Self::ApiKey(AuthVendor::OpenAI) => "OpenAI        (API key)",
            Self::ApiKey(AuthVendor::Kiro) => "Kiro          (API key)",
        }
    }
}

const CHOICES: &[Choice] = &[
    Choice::Oauth(AuthVendor::Anthropic),
    Choice::Oauth(AuthVendor::OpenAI),
    Choice::Oauth(AuthVendor::Kiro),
    Choice::KiroBuilderId,
    Choice::KiroIdc,
    Choice::ApiKey(AuthVendor::OpenCodeGo),
];

fn pick_choice() -> Result<Choice> {
    let mut term = PlatformTerminal::new().context("could not open terminal for login menu")?;
    term.enter_raw_mode().context("could not enter raw mode for login menu")?;
    let reader = term.event_reader();
    let result = run_picker(&reader);
    let _ = term.enter_cooked_mode();
    let mut err = io::stderr();
    let _ = write!(err, "\r\x1b[J");
    let _ = err.flush();
    result
}

fn run_picker(reader: &termina::EventReader) -> Result<Choice> {
    let mut selected: usize = 0;
    write!(io::stderr(), "\x1b[2J\x1b[H")?;
    io::stderr().flush()?;
    render_menu(selected, false)?;
    loop {
        let raw = reader.read(|_| true).context("terminal read failed")?;
        let termina::Event::Key(k) = raw else { continue };
        if k.kind != KeyEventKind::Press { continue; }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 < CHOICES.len() { selected += 1; }
            }
            KeyCode::Enter => return Ok(CHOICES[selected]),
            KeyCode::Escape | KeyCode::Char('q') => anyhow::bail!("cancelled"),
            KeyCode::Char('c') if k.modifiers.contains(Modifiers::CONTROL) => anyhow::bail!("cancelled"),
            _ => continue,
        }
        render_menu(selected, true)?;
    }
}

fn render_menu(selected: usize, redraw: bool) -> Result<()> {
    let lines = 3 + CHOICES.len() + 2;
    let mut out = io::stderr();
    if redraw { write!(out, "\x1b[{lines}A\r\x1b[J")?; } else { write!(out, "\r")?; }
    write!(out, "luma login — select provider\r\n\r\n")?;
    for (i, choice) in CHOICES.iter().enumerate() {
        let arrow = if i == selected { ">" } else { " " };
        write!(out, " {arrow} {}\r\n", choice.label())?;
    }
    write!(out, "\r\n   ↑/↓ move · enter select · esc cancel\r\n")?;
    out.flush()?;
    Ok(())
}

fn prompt_idc_params() -> Result<(String, String)> {
    eprint!("Start URL (e.g. https://d-xxxxxxxxxx.awsapps.com/start): ");
    io::stderr().flush().ok();
    let mut url = String::new();
    io::stdin().read_line(&mut url)?;
    let url = url.trim().to_owned();
    if url.is_empty() { anyhow::bail!("no start URL provided"); }
    eprint!("Region [us-east-1]: ");
    io::stderr().flush().ok();
    let mut region = String::new();
    io::stdin().read_line(&mut region)?;
    let region = region.trim();
    Ok((url, if region.is_empty() { "us-east-1" } else { region }.to_owned()))
}
