use super::Action;
/// Agent lifecycle — spawn, submit, done handling.
use super::state::RunState;
use crate::event::AgentCommand;
use crate::tui::status::StatusState;
use tokio_util::sync::CancellationToken;

impl super::App {
    /// Handle submit from prompt — command or chat.
    pub(super) fn on_submit(
        &mut self,
        content: Vec<crate::core::types::ContentBlock>,
        images: Vec<(String, Vec<u8>)>,
    ) -> Action {
        // Command: first text block starts with /
        if let Some(crate::core::types::ContentBlock::Text { text }) = content.first()
            && let Some(cmd) = text.strip_prefix('/')
        {
            return self.handle_command(cmd);
        }
        // Session load in flight — agent queue has a LoadSession ahead of any
        // Chat command. Drop this submit rather than queuing behind a session
        // replace that would discard the message anyway.
        if self.agent.is_loading_session {
            return Action::Continue;
        }
        if self.agent.state != RunState::Idle {
            self.agent.pending_content = Some(content);
            self.agent.pending_images = Some(images);
            self.agent.state = RunState::Aborting;
            if let Some(c) = &self.agent.cancel {
                c.cancel();
            }
            return Action::Render;
        }
        self.spawn_agent(content, images);
        Action::Render
    }

    /// Send user content to agent.
    pub(super) fn spawn_agent(
        &mut self,
        content: Vec<crate::core::types::ContentBlock>,
        images: Vec<(String, Vec<u8>)>,
    ) {
        if self.config.model.is_none() {
            self.doc.error("no model — run 'luma sync'");
            return;
        }
        self.ensure_agent_loop();
        self.commit_pending_config();
        self.enter_chat();
        self.doc.user_message(&content);
        self.sync_prompt_commands();
        self.agent.state = RunState::Streaming;
        self.ui.status.set_state(StatusState::Thinking);
        self.agent.turn_start = Some(std::time::Instant::now());

        // Extract file refs from text blocks
        let text = crate::core::types::Message::content_text(&content);
        let files = read_file_refs(&text);

        let images: Vec<crate::event::ImageAttach> = images
            .into_iter()
            .map(|(media_type, data)| crate::event::ImageAttach { media_type, data })
            .collect();

        let cancel = CancellationToken::new();
        self.agent.cancel = Some(cancel.clone());
        if let Some(agent_tx) = &self.agent.tx
            && agent_tx
                .try_send(AgentCommand::Chat {
                    content,
                    files,
                    images,
                    cancel,
                })
                .is_err()
        {
            self.agent.state = RunState::Idle;
            self.agent.cancel = None;
            self.ui.status.set_state(StatusState::Ready);
            self.doc.error("internal: agent queue is busy");
        }
    }

    /// Start async clipboard image read — result arrives via ClipboardImage event.
    ///
    /// Debounced 250ms: a single Cmd+V on iTerm2 fires both a raw key event
    /// and an empty bracketed paste, both of which try to trigger a clipboard
    /// read. Without throttling the user gets two image chips for one action.
    pub(super) fn paste_clipboard_image(&mut self) {
        if is_ssh_session() {
            self.doc
                .info("image paste not supported over SSH — use a file path instead");
            return;
        }
        let now = std::time::Instant::now();
        if let Some(prev) = self.agent.last_clipboard_paste
            && now.duration_since(prev) < std::time::Duration::from_millis(250)
        {
            crate::dbg_log!("clipboard paste debounced");
            return;
        }
        self.agent.last_clipboard_paste = Some(now);

        let Some(tx) = self.tx.clone() else { return };
        tokio::task::spawn_blocking(move || {
            let result = read_clipboard_image().map(|data| {
                let (media_type, _) = detect_image_format(&data);
                (media_type.to_owned(), data)
            });
            let _ = tx.blocking_send(crate::event::Event::ClipboardImage(result));
        });
    }

    /// Handle async clipboard image result.
    pub(super) fn on_clipboard_image(&mut self, result: Option<(String, Vec<u8>)>) {
        match result {
            Some((media_type, data)) => {
                let msg = format_attach_msg("image", &data);
                self.ui.prompt.attach_image(media_type, data);
                self.doc.info(&msg);
            }
            None => self.doc.info("no image in clipboard"),
        }
    }

    pub(super) fn paste_image_file(&mut self, path: &str) {
        let Ok(data) = std::fs::read(path) else {
            self.doc.info("cannot read image file");
            return;
        };
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        let msg = format_attach_msg(name, &data);
        let (media_type, _) = detect_image_format(&data);
        self.ui.prompt.attach_image(media_type.to_owned(), data);
        self.doc.info(&msg);
    }

    pub(super) fn ensure_agent_loop(&mut self) {
        if self.agent.tx.is_some() {
            return;
        }
        let Some(model) = &self.config.model else {
            return;
        };
        let tx = self.tx.clone().expect("tx set in run()");

        let skills = crate::config::skills::discover();
        let skill_catalog = crate::config::skills::build_catalog(&skills);
        let project_instructions = crate::config::instructions::discover();
        let instructions_block =
            crate::config::instructions::build_instructions(&project_instructions);
        let style = crate::tool::ToolStyle::for_mode(self.config.mode, &model.source);
        let base_prompt = crate::config::prompt::build(self.config.mode, style);
        let system_prompt = format!(
            "{base_prompt}\n{}{skill_catalog}{instructions_block}",
            self.config.env_context
        );

        let config = crate::core::agent::AgentConfig {
            model_id: model.id.clone(),
            source: model.source.clone(),
            system_prompt,
            thinking: self.config.thinking,
            capabilities: model.capabilities.clone(),
        };

        let search_pref = crate::tool::search_preference_for(&model.source);
        let registry = crate::tool::build_registry(style, Self::search_backend(), search_pref);

        self.agent.tx = Some(crate::core::agent::spawn(config, registry, tx));
        self.agent.last_sent = Some(super::state::SentConfig {
            mode: self.config.mode,
            model_id: model.id.clone(),
            source: model.source.clone(),
            thinking: self.config.thinking,
        });
    }

    /// Pick a web-search backend based on available credentials/env,
    /// independent of which provider is active. Priority:
    /// Kiro MCP (free) → Exa → Tavily → SearXNG → None.
    pub(super) fn search_backend() -> Option<crate::tool::web_search::SearchBackend> {
        use crate::tool::web_search::SearchBackend;
        if crate::config::auth::has_kiro_credential() {
            return Some(SearchBackend::Kiro);
        }
        if let Ok(key) = std::env::var("EXA_API_KEY") {
            return Some(SearchBackend::Exa { api_key: key });
        }
        if let Ok(key) = std::env::var("TAVILY_API_KEY") {
            return Some(SearchBackend::Tavily { api_key: key });
        }
        if let Ok(url) = std::env::var("SEARXNG_URL") {
            return Some(SearchBackend::SearXNG { base_url: url });
        }
        None
    }

    pub(super) fn on_agent_done(&mut self) {
        // If a session load is in flight the doc is about to be cleared by
        // apply_loaded_session — skip cosmetic doc writes to avoid flicker.
        if !self.agent.is_loading_session {
            // Any tool/skill block that never received a matching end event
            // (provider retry discarded it, stream cut mid-tool, etc.) is
            // finalised here so the UI never shows a "preparing..." block
            // after the turn has returned to idle.
            self.doc.close_pending("");
            self.doc.newline();
            if let Some(start) = self.agent.turn_start.take() {
                let label = super::format_duration(start.elapsed());
                self.doc.divider_with_label(&label);
            } else {
                self.doc.divider();
            }
            self.ui.status.set_state(StatusState::Ready);
        } else {
            self.agent.turn_start = None;
        }

        self.agent.state = RunState::Idle;
        self.agent.cancel = None;

        // Surface any cooldowns set during the turn in the status bar.
        self.refresh_pool_health();

        if let Some(content) = self.agent.pending_content.take() {
            let images = self.agent.pending_images.take().unwrap_or_default();
            self.spawn_agent(content, images);
        }
    }

    pub(super) fn on_agent_error(&mut self, msg: &str) {
        if msg.contains("Aborted") {
            self.doc.warn("aborted");
        } else {
            self.doc.error(&format_provider_error(msg));
        }
        self.on_agent_done();
    }
}

fn format_provider_error(msg: &str) -> String {
    // retry.rs already formats HTTP errors with actionable guidance.
    // Only wrap raw 429 messages that bypass format_http_error.
    if has_actionable_guidance(msg) {
        return msg.to_owned();
    }
    if is_rate_limit_error(msg) {
        return format!(
            "provider rate limit hit (429)\n\n{}\n\nTry again in a bit, reduce request frequency, or switch model/provider.",
            msg.trim()
        );
    }
    msg.to_owned()
}

/// Whether the error message already contains provider-level guidance.
fn has_actionable_guidance(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("switch model/provider")
        || lower.contains("try another model/provider")
        || lower.contains("check your api key")
        || lower.contains("check your internet")
        || lower.contains("luma sync")
}

fn is_rate_limit_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests")
}

#[cfg(target_os = "macos")]
const APPLESCRIPT_CLIPBOARD_IMAGE: &str = r#"set theFile to POSIX file "{PATH}"
try
    set theImage to the clipboard as «class PNGf»
    set fileRef to open for access theFile with write permission
    set eof of fileRef to 0
    write theImage to fileRef
    close access fileRef
on error
    try
        close access theFile
    end try
    error "no image"
end try"#;

#[cfg(target_os = "macos")]
fn read_clipboard_image() -> Option<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("luma_clipboard_{}.png", std::process::id()));
    let script = APPLESCRIPT_CLIPBOARD_IMAGE.replace("{PATH}", &tmp.display().to_string());
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let data = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    if data.is_empty() {
        return None;
    }
    Some(data)
}

#[cfg(target_os = "windows")]
const PS_CLIPBOARD_IMAGE: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
$img = [System.Windows.Forms.Clipboard]::GetImage()
if ($img -eq $null) { exit 1 }
$ms = New-Object System.IO.MemoryStream
$img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
$stdout = [Console]::OpenStandardOutput()
$stdout.Write($ms.ToArray(), 0, $ms.Length)
$img.Dispose()
$ms.Dispose()
"#;

#[cfg(target_os = "windows")]
fn read_clipboard_image() -> Option<Vec<u8>> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            PS_CLIPBOARD_IMAGE,
        ])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(output.stdout)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_clipboard_image() -> Option<Vec<u8>> {
    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let output = if is_wayland {
        std::process::Command::new("wl-paste")
            .args(["--type", "image/png"])
            .output()
            .ok()?
    } else {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .output()
            .ok()?
    };
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(output.stdout)
}

/// Detect if running inside an SSH session.
fn is_ssh_session() -> bool {
    std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok()
}

/// Format attach confirmation message as `attached: <name> (<size>)`.
/// Size shows `~KB` under 1 MB, `~MB` with one decimal above.
fn format_attach_msg(name: &str, data: &[u8]) -> String {
    let len = data.len();
    let size = if len < 1024 * 1024 {
        format!("{} KB", len.div_ceil(1024))
    } else {
        format!("{:.1} MB", len as f64 / (1024.0 * 1024.0))
    };
    format!("attached: {name} ({size})")
}

fn detect_image_format(data: &[u8]) -> (&'static str, &'static str) {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        ("image/png", "png")
    } else if data.starts_with(&[0xFF, 0xD8]) {
        ("image/jpeg", "jpg")
    } else if data.starts_with(b"GIF") {
        ("image/gif", "gif")
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        ("image/webp", "webp")
    } else {
        ("image/png", "png")
    }
}

fn read_file_refs(text: &str) -> Vec<crate::event::FileAttach> {
    parse_file_refs(text)
        .into_iter()
        .filter_map(|fref| {
            let content = std::fs::read_to_string(&fref.path).ok()?;
            Some(crate::event::FileAttach {
                path: fref.path,
                content,
            })
        })
        .collect()
}

struct FileRef {
    path: String,
}

fn parse_file_refs(text: &str) -> Vec<FileRef> {
    let mut refs = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            i += 1;
            let path_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'@'
                && bytes[i] != b','
                && bytes[i] != b';'
            {
                i += 1;
            }
            let path_str = &text[path_start..i];
            if !path_str.is_empty() {
                let p = std::path::Path::new(path_str);
                if p.is_file() {
                    refs.push(FileRef {
                        path: path_str.to_owned(),
                    });
                }
            }
        } else {
            i += 1;
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_refs_empty() {
        assert!(parse_file_refs("hello world").is_empty());
    }

    #[test]
    fn format_attach_msg_kb_rounds_up() {
        // 1 byte still shows 1 KB rather than 0 — div_ceil behavior.
        assert_eq!(
            format_attach_msg("x.png", &[0; 1]),
            "attached: x.png (1 KB)"
        );
    }

    #[test]
    fn format_attach_msg_kb_under_mb() {
        let data = vec![0u8; 512 * 1024];
        assert_eq!(format_attach_msg("img", &data), "attached: img (512 KB)");
    }

    #[test]
    fn format_attach_msg_mb_above_threshold() {
        let data = vec![0u8; 2 * 1024 * 1024 + 512 * 1024];
        assert_eq!(
            format_attach_msg("big.png", &data),
            "attached: big.png (2.5 MB)"
        );
    }

    #[test]
    fn parse_file_refs_finds_existing() {
        let refs = parse_file_refs("look at @Cargo.toml please");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "Cargo.toml");
    }

    #[test]
    fn email_not_treated_as_file_ref() {
        assert!(parse_file_refs("email user@example.com please").is_empty());
    }

    #[test]
    fn formats_rate_limit_error_for_tui() {
        let formatted = format_provider_error("429 Too Many Requests: quota exceeded");
        assert!(formatted.contains("provider rate limit hit (429)"));
        assert!(formatted.contains("Try again in a bit"));
        assert!(formatted.contains("switch model/provider"));
    }

    #[test]
    fn leaves_non_rate_limit_error_unchanged() {
        let msg = "500 Internal Server Error";
        assert_eq!(format_provider_error(msg), msg);
    }

    #[test]
    fn preserves_provider_hard_quota_message() {
        let msg = "claude hard quota exceeded (429): quota exceeded. Quota/billing must recover before retrying; try another model/provider if needed.";
        assert_eq!(format_provider_error(msg), msg);
    }

    #[test]
    fn preserves_provider_temporary_throttling_message() {
        let msg = "claude temporary throttling (429): too many requests. Wait a bit, reduce request frequency, or switch model/provider.";
        assert_eq!(format_provider_error(msg), msg);
    }

    #[test]
    fn preserves_auth_error_with_guidance() {
        let msg = "claude auth failed (401): invalid token. Check your API key or run 'luma sync' to refresh credentials.";
        assert_eq!(format_provider_error(msg), msg);
    }

    #[test]
    fn preserves_network_error_with_guidance() {
        let msg = "connection failed: Connection refused. Check your internet connection and any proxy/firewall settings.";
        assert_eq!(format_provider_error(msg), msg);
    }
}
