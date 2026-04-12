mod agent;
mod commands;
mod dispatch;
mod input;
mod render;
mod state;

use state::{AgentHandle, AppConfig, PickerMode, Screen, UiComponents};

use crate::config::models;
use crate::core::provider::{Provider, ThinkingCapabilities};
use crate::core::types::ThinkingLevel;
use crate::event::Event;
use crate::tui::document::Document;
use crate::tui::picker::Picker;
use crate::tui::prompt::PromptState;
use crate::tui::renderer::{Region, Renderer};
use crate::tui::selection::Selection;
use crate::tui::status::StatusBar;
use crate::tui::text::{Line, Padding};
use crate::tui::theme::{CONTENT_PAD, OUTER_MARGIN, palette};
use crate::tui::view::ViewState;
use std::io::Write;
use std::time::Duration;
use termina::Terminal as _;

const TICK_INTERVAL: Duration = Duration::from_millis(80);
const SCROLL_STEP: usize = 3;
const ABORT_HINT_TICKS: u8 = 25;
const DRAIN_BUDGET: usize = 256;

const LOGO: &[&str] = &[
    "                                           ⢰⡇⠀⠀⣸⠃      ⣴⠟⠁⠈⢻⣦",
    "                                           ⣿⠀⠀⢠⡟    ⢠⡾⠃⠀⠀⣰⠟⠁",
    "                                          ⠉⠛⠓⠾⠁⠀⠀⣰⠟⠀⠀⢀⡾⠋     ⢀⣴⣆",
    "                            ⢀⣀⣀⣀⣠⣤⣤⣤⣄⣀⣀⡀        ⠙⠳⣦⣴⠟⠁   ⣠⡴⠋⠀⠀⠈⢷⣄",
    "                     ⣀⣤⣴⣶⣿⣿⣿⣿⡿⠿⠿⠿⠿⠿⠿⣿⣿⣿⣿⣷⣦⣤⣀         ⠀⣠⡾⠋⠀⠀⢀⣴⠟⠁",
    "                ⢀⣠⣶⣿⣿⡿⠟⠋⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀ ⠀⠈⠉⠙⠻⢿⣿⣿⣶⣄⡀    ⠺⣏⠀⠀⣀⡴⠟⠁⢀⣀",
    "              ⣠⣶⣿⣿⠿⠋⠁⠀⢀⣴⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢶⣬⡙⠿⣿⣿⣶⣄   ⠙⢷⡾⠋⢀⣤⠾⠋⠙⢷⡀",
    "            ⣠⣾⣿⡿⠋⠁⠀⠀⠀⢠⣾⡟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣰⣦⣠⣤⠽⣿⣦⠈⠙⢿⣿⣷⣄   ⠺⣏⠁⠀⠀⣀⣼⠿",
    "          ⢠⣾⣿⡿⠋⠀⠀⠀⠀⠀⣰⣿⠟⠀⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠉⣿⣧⠀⠀⠈⢿⣷⣄⠀⠙⢿⣿⣷⣄  ⠙⣧⡴⠟⠋",
    "         ⣴⣿⣿⠏⠀⠀⠀⠀⠀⠀⢷⣿⡟⠀⣰⡆⠀⢸⣿⠀⠀⠀⠀⠀⠀⠀⠀⣀⡀⠀⣿⣿⡀⠀⠀⠈⢿⣿⣦⠀⠀⠙⢿⣿⣦",
    "        ⣼⣿⡿⠁⠀⠦⣤⣀⠀⠀⢀⣿⣿⡇⢰⣿⠇⠀⢸⣿⡆⠀⠀⠀⠀⠀⠀⠀⣿⡇⠀⢸⣿⣿⣆⠀⠀⠈⣿⣿⣧⣠⣤⠾⢿⣿⣧",
    "       ⣸⣿⣿⣵⣿⠀⠀⠀⠉⠀⠀⣼⣿⢿⡇⣾⣿⠀⠀⣾⣿⡇⢸⠀⠀⠀⠀⠀⠀⣿⡇⠀⣼⣿⢻⣿⣦⠴⠶⢿⣿⣿⣇⠀⠀⠀⢻⣿⣧⣀",
    "      ⢀⣿⣿⣿⣿⠇⠀⠀⠀⠀⠀⢠⣿⡟⡌⣼⣿⣿⠉⢁⣿⣿⣷⣿⡗⠒⠚⠛⠛⢛⣿⣯⣯⣿⣿⠀⢻⣿⣧⠀⢸⣿⣿⣿⡄⠀⠀⠀⠙⢿⣿⣷⣤⣀",
    "      ⢸⣿⣿⣿⠏⠀⠀⠀⠀⠀⠀⢸⣿⡇⣼⣿⣿⣿⣶⣾⣿⣿⢿⣿⡇⠀⠀⠀⠀⢸⣿⠟⢻⣿⣿⣿⣶⣿⣿⣧⢸⣿⣿⣿⣧⠀⠀⠀⢰⣷⡈⠛⢿⣿⣿⣶⣦⣤⣤⣀",
    "   ⢀⣤⣾⣿⣿⢫⡄⠀⠀⠀⠀⠀⠀⣿⣿⣹⣿⠏⢹⣿⣿⣿⣿⣿⣼⣿⠃⠀⠀⠀⢀⣿⡿⢀⣿⣿⠟⠀⠀⠀⠹⣿⣿⣿⠇⢿⣿⡄⠀⠀⠈⢿⣿⣷⣶⣶⣿⣿⣿⣿⣿⡿",
    "⣴⣶⣶⣿⣿⣿⣿⣋⣴⣿⣇⠀⠀⠀⠀⢀⣿⣿⣿⣟⣴⠟⢿⣿⠟⣿⣿⣿⣿⣶⣶⣶⣶⣾⣿⣿⣿⠿⣫⣤⣶⡆⠀⠀⣻⣿⣿⣶⣸⣿⣷⡀⠀⠀⠸⣿⣿⣿⡟⠛⠛⠛⠉⠁",
    "⠻⣿⣿⣿⣿⣿⣿⡿⢿⣿⠋⠀⢠⠀⠀⢸⣿⣿⣿⣿⣁⣀⣀⣁⠀⠀⠉⠉⠉⠉⠉⠉⠉⠁⠀⠀⠀⠸⢟⣫⣥⣶⣿⣿⣿⠿⠟⠋⢻⣿⡟⣇⣠⡤⠀⣿⣿⣿⣿⡀",
    "   ⠉⠉⢹⣿⡇⣾⣿⠀⠀⢸⡆⠀⢸⣿⣿⡟⠿⠿⠿⠿⣿⣿⣿⣿⣷⣦⡄⠀⠀⠀⠀⠀⠀⢠⣾⣿⣿⣿⣿⣯⣥⣤⣄⣀⡀⢸⣿⠇⢿⢸⡇⠀⢹⣿⣿⣿⡇",
    "     ⣾⣿⡇⣿⣿⠀⠀⠸⣧⠀⢸⣿⣿⠀⢀⣀⣤⣤⣶⣾⣿⠿⠟⠛⠁⠀⠀⠀⠀⠀⠀⠀⠉⠉⠉⠙⠛⢛⣛⠛⠛⠛⠃⠸⣿⣆⢸⣿⣇⠀⢸⣿⣿⣿⣷",
    "     ⢻⣿⡇⢻⣿⡄⠀⠀⣿⡄⢸⣿⡷⢾⣿⠿⠟⠛⠉⠉⠀⠀⠀⢠⣶⣾⣿⣿⣿⣿⣿⣶⣶⠀⠀⢀⡾⠋⠁⢠⡄⠀⣤⠀⢹⣿⣦⣿⡇⠀⢸⣿⣿⣿⣿",
    "     ⢸⣿⣇⢸⣿⡇⠀⠀⣿⣧⠈⣿⣷⠀⠀⢀⣀⠀⢙⣧⠀⠀⠀⢸⣿⡇⠀⠀⠀⠀⢀⣿⡏⠀⠀⠸⣇⠀⠀⠘⠛⠘⠛⠀⢀⣿⣿⣿⡇⠀⣼⣿⢻⣿⡿",
    "     ⠸⣿⣿⣸⣿⣿⠀⠀⣿⣿⣆⢿⣿⡀⠀⠸⠟⠀⠛⣿⠃⠀⠀⢸⣿⡇⠀⠀⠀⠀⢸⣿⡇⠀⠀⠀⠙⠷⣦⣄⡀⠀⢀⣴⣿⡿⣱⣾⠁⠀⣿⣿⣾⣿⡇",
    "      ⢻⣿⣿⣿⣿⣇⠀⢿⢹⣿⣆⢸⣿⣧⣀⠀⠀⠴⠞⠁⠀⠀⠸⣿⡇⠀⠀⠀⠀⣿⣿⠀⠀⠀⠀⠀⠀⢀⣨⣽⣾⣿⣿⡏⢀⣿⣿⠀⣸⣿⣿⣿⡿",
    "      ⠈⢻⣿⣿⣿⣿⣆⢸⡏⠻⣿⣦⣿⣿⣿⣿⣶⣦⣤⣀⣀⣀⣀⣿⣷⠀⠀⠀⣸⣿⣏⣀⣤⣤⣶⣾⣿⣿⣿⠿⠛⢹⣿⣧⣼⣿⣿⣰⣿⣿⠛⠛",
    "        ⠉⠛⠙⣿⣿⣦⣷⠀⢻⣿⣿⣿⣿⡝⠛⠻⠿⢿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡿⠿⠟⠛⠛⠉⠁⠀⠀⠀⣼⣿⣿⣿⣿⣿⣿⣿⠃",
    "           ⠈⢻⣿⣿⣄⢸⣿⣿⣿⣿⣷⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⠿⠟⠻⣿⡿⠋⠁",
    "             ⠙⢿⣿⣿⣿⣿⡌⠙⠛⠁",
];

struct Regions {
    output: Region,
    status: Region,
    input: Region,
}

const MIN_INPUT_HEIGHT: u16 = 5;
const INPUT_CHROME: u16 = 3; // top bar + mode line + bottom border

fn compute_regions(w: u16, h: u16) -> Regions {
    compute_regions_with_input(w, h, MIN_INPUT_HEIGHT)
}

/// Compute regions with a specific input height.
fn compute_regions_with_input(w: u16, h: u16, ih: u16) -> Regions {
    let mx = OUTER_MARGIN;
    let sh = 2u16;
    let oh = h.saturating_sub(sh + ih).max(1);
    let inner_w = w.saturating_sub(mx * 2);
    Regions {
        output: Region {
            row: 1,
            col: 1 + mx,
            width: inner_w,
            height: oh,
            bg: palette::BG,
            padding: Padding {
                left: CONTENT_PAD,
                right: CONTENT_PAD,
                top: 0,
                bottom: 1,
            },
        },
        status: Region {
            row: 1 + oh + ih,
            col: 1,
            width: w,
            height: sh,
            bg: palette::BG,
            padding: Padding {
                left: OUTER_MARGIN + CONTENT_PAD,
                right: OUTER_MARGIN + CONTENT_PAD,
                top: 0,
                bottom: 1,
            },
        },
        input: Region {
            row: 1 + oh,
            col: 1 + mx,
            width: inner_w,
            height: ih,
            bg: palette::SURFACE,
            padding: Padding::zero(),
        },
    }
}

pub struct App {
    screen: Screen,
    doc: Document,
    view: ViewState,
    ui: UiComponents,
    renderer: Renderer,
    regions: Regions,
    agent: AgentHandle,
    config: AppConfig,
    tx: Option<crate::event_bus::Sender>,
    term: Option<termina::PlatformTerminal>,
}

impl App {
    fn current_thinking_capabilities(&self) -> ThinkingCapabilities {
        let Some(model) = &self.config.model else {
            return ThinkingCapabilities::standard();
        };
        match model.source.as_str() {
            "anthropic" => crate::provider::claude::ClaudeProvider::new(&model.id, "", false, "")
                .thinking_capabilities(),
            "codex" => crate::provider::codex::CodexProvider::new(&model.id, "", None, "", "")
                .thinking_capabilities(),
            _ => crate::provider::openai::OpenAIProvider::new(&model.id, "", "")
                .thinking_capabilities(),
        }
    }

    pub fn new(env_context: String) -> Self {
        let term = termina::PlatformTerminal::new().ok();
        let (w, h) = term
            .as_ref()
            .and_then(|t| t.get_dimensions().ok())
            .map(|s| (s.cols, s.rows))
            .unwrap_or((80, 24));
        let regions = compute_regions(w, h);
        let mut renderer = Renderer::new(w, h);
        renderer.define("output", regions.output.clone());
        renderer.define("status", regions.status.clone());
        renderer.define("input", regions.input.clone());

        let view = ViewState::new(
            regions.output.content_width() as usize,
            regions.output.content_height() as usize,
        );
        let mut prompt = PromptState::new();
        prompt.add_command("new", "new thread");
        prompt.add_command("model", "switch model");
        prompt.add_command("resume", "resume last session");
        prompt.add_command("sessions", "browse sessions");
        prompt.add_command("accounts", "view account pool");
        prompt.add_command("login", "add account via browser");
        prompt.add_command("exit", "quit luma");

        let mode = crate::config::prefs::load_mode();
        let model = models::resolve_default(mode);
        let thinking = crate::config::prefs::load_thinking();

        let ui = UiComponents {
            prompt,
            picker: Picker::new(),
            dialog: crate::tui::dialog::Dialog::new(),
            status: StatusBar::new(),
            selection: Selection::new(),
            drag: None,
            last_output_width: 0,
        };
        let config = AppConfig {
            mode,
            model,
            env_context,
            thinking,
            picker_mode: PickerMode::Model,
        };

        let content_w = regions.output.content_width() as usize;
        let output_h = regions.output.content_height() as usize;

        let mut app = Self {
            screen: Screen::Welcome {
                lines: build_welcome(LOGO, content_w, output_h),
            },
            doc: Document::new(),
            view,
            ui,
            renderer,
            regions,
            agent: AgentHandle::new(),
            config,
            tx: None,
            term,
        };
        app.update_status();
        app.refresh_pool_health();
        let thinking_caps = app.current_thinking_capabilities();
        let thinking = thinking_caps.coerce(thinking);
        app.config.thinking = thinking;
        if thinking != ThinkingLevel::Off {
            app.ui
                .status
                .set_thinking_level(thinking_caps.label(thinking));
        }
        app
    }

    fn process_event(&mut self, event: Event) -> bool {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(event)));
        match result {
            Ok(Action::Continue | Action::Render) => false,
            Ok(Action::Quit) => true,
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_owned()
                };
                crate::dbg_log!("PANIC caught: {msg}");
                self.doc.error(&format!("internal error: {msg}"));
                false
            }
        }
    }

    /// Transition to Chat screen. Drops Welcome data if present.
    fn enter_chat(&mut self) {
        if !self.screen.is_chat() {
            self.screen = Screen::Chat;
        }
    }

    /// VT sequences to enable TUI features (alternate screen, mouse, paste, etc).
    const VT_ENABLE: &str = concat!(
        "\x1b[?1049h", // enter alternate screen
        "\x1b[?1000h", // enable mouse (button press/release)
        "\x1b[?1002h", // enable mouse drag tracking
        "\x1b[?1006h", // SGR mouse mode
        "\x1b[?2004h", // enable bracketed paste
        "\x1b[?25l",   // hide cursor
        "\x1b[?1007l", // disable alternate scroll mode
    );

    /// VT sequences to restore terminal (reverse of VT_ENABLE).
    const VT_RESTORE: &str = concat!(
        "\x1b[?1007h",    // re-enable alternate scroll mode
        "\x1b[?2004l",    // disable bracketed paste
        "\x1b[?1006l",    // disable SGR mouse mode
        "\x1b[?1002l",    // disable mouse drag tracking
        "\x1b[?1000l",    // disable mouse
        "\x1b[0 q",       // restore default cursor shape
        "\x1b]112\x1b\\", // restore default cursor color
        "\x1b[?25h",      // show cursor
        "\x1b[0m",        // reset SGR attributes
        "\x1b[?1049l",    // leave alternate screen
        "\x1b[2J",        // clear main screen (some terminals leave artifacts)
        "\x1b[H",         // cursor to top-left
    );

    fn enter_terminal(term: &mut termina::PlatformTerminal) -> anyhow::Result<()> {
        term.set_panic_hook(|handle| {
            use std::io::Write;
            let _ = write!(handle, "{}", Self::VT_RESTORE);
            let _ = handle.flush();
        });
        term.enter_raw_mode()?;
        write!(term, "{}", Self::VT_ENABLE)?;
        term.flush()?;
        Ok(())
    }

    /// Restore terminal state: write VT sequences and exit raw mode.
    fn exit_terminal(term: &mut termina::PlatformTerminal) {
        let _ = write!(term, "{}", Self::VT_RESTORE);
        let _ = term.flush();
        let _ = term.enter_cooked_mode();
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let (tx, mut rx) = crate::event_bus::channel();
        self.tx = Some(tx.clone());

        let mut term = self
            .term
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open terminal"))?;
        let reader = term.event_reader();
        Self::enter_terminal(&mut term)?;
        self.renderer.clear_screen();

        if self.config.model.is_none() {
            self.doc.warn("no model — run 'luma sync'");
        }
        self.render();

        // On Windows, the console sends CTRL_C_EVENT to the process even in raw
        // mode. Without a handler the default action terminates the process.
        // Spawn a task that absorbs the signal so Ctrl+C is only handled as a
        // key event delivered through the terminal reader.
        #[cfg(windows)]
        tokio::spawn(async {
            loop {
                _ = tokio::signal::ctrl_c().await;
            }
        });

        let tx_input = tx.clone();
        tokio::task::spawn_blocking(move || input::read_stdin_loop(reader, tx_input));

        let tx_tick = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK_INTERVAL);
            loop {
                interval.tick().await;
                if tx_tick.send(Event::Tick).await.is_err() {
                    break;
                }
            }
        });

        loop {
            let Some(event) = rx.recv().await else { break };
            if self.process_event(event) {
                break;
            }
            let mut drained = 1usize;
            while drained < DRAIN_BUDGET {
                match rx.try_recv() {
                    Some(event) => {
                        if self.process_event(event) {
                            self.render();
                            Self::exit_terminal(&mut term);
                            drop(term);
                            std::process::exit(0);
                        }
                        drained += 1;
                    }
                    None => break,
                }
            }
            self.render();
        }

        Self::exit_terminal(&mut term);
        drop(term);
        std::process::exit(0);
    }
}

/// Build static welcome screen lines: vertically centered logo.
fn build_welcome(logo: &[&str], width: usize, height: usize) -> Vec<Line> {
    use crate::tui::text::Span;
    use smallvec::smallvec;

    let max_w = logo
        .iter()
        .map(|l| crate::tui::text::display_width(l))
        .max()
        .unwrap_or(0);
    let pad = (width.saturating_sub(max_w) * 2 / 5) as u16;
    let logo_lines: Vec<Line> = logo
        .iter()
        .map(|l| {
            let mut line = Line::new(smallvec![Span::new(l.to_string(), palette::MUTED)]);
            line.indent = pad;
            line
        })
        .collect();

    let top_pad = height.saturating_sub(logo_lines.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.resize(top_pad, Line::empty());
    lines.extend(logo_lines);
    lines.resize(height, Line::empty());
    lines
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = d.as_secs() / 60;
        let s = d.as_secs() % 60;
        format!("{m}m {s}s")
    }
}

enum Action {
    Continue,
    Render,
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_regions_basic() {
        let r = compute_regions(80, 24);
        assert_eq!(r.output.height, 17);
        assert_eq!(r.input.height, 5);
        assert_eq!(r.status.height, 2);
    }

    #[test]
    fn format_duration_short() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs_f64(3.456)),
            "3.5s"
        );
    }

    #[test]
    fn format_duration_long() {
        assert_eq!(
            format_duration(std::time::Duration::from_secs(95)),
            "1m 35s"
        );
    }
}
