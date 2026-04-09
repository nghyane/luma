mod agent;
mod commands;
mod dispatch;
mod input;
mod render;
mod state;

use state::{AgentHandle, AppConfig, PickerMode, Screen, UiComponents};

use crate::config::models;
use crate::core::types::ThinkingLevel;
use crate::event::Event;
use crate::tui::document::Document;
use crate::tui::picker::Picker;
use crate::tui::prompt::PromptState;
use crate::tui::renderer::{Region, Renderer};
use crate::tui::selection::Selection;
use crate::tui::status::StatusBar;
use crate::tui::term;
use crate::tui::text::{Line, Padding};
use crate::tui::theme::{CONTENT_PAD, OUTER_MARGIN, palette};
use crate::tui::view::ViewState;
use crossterm::terminal;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;

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
    tx: Option<mpsc::Sender<Event>>,
}

impl App {
    pub fn new(env_context: String) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
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
        prompt.add_command("exit", "quit luma");

        let mode = crate::config::prefs::load_mode();
        let model = models::resolve_default(mode);
        let thinking = crate::config::prefs::load_thinking();

        let ui = UiComponents {
            prompt,
            picker: Picker::new(),
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
        };
        app.update_status();
        if thinking != ThinkingLevel::Off {
            let label = match thinking {
                ThinkingLevel::Off => "off",
                ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High => "high",
            };
            app.ui.status.set_thinking_level(label);
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

    fn enter_terminal(out: &mut impl Write) -> anyhow::Result<()> {
        terminal::enable_raw_mode()?;
        crossterm::execute!(
            out,
            terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide,
        )?;
        // Disable alternate scroll mode: prevents the terminal from converting
        // mouse wheel events to cursor Up/Down key events in alternate screen.
        // Without this, Windows Terminal sends Up/Down keys instead of
        // ScrollUp/ScrollDown mouse events, breaking scroll.
        let _ = write!(out, "\x1b[?1007l");
        out.flush()?;
        Ok(())
    }

    fn exit_terminal(out: &mut impl Write) {
        // Re-enable alternate scroll mode before leaving alternate screen.
        let _ = write!(out, "\x1b[?1007h");
        let _ = crossterm::execute!(
            out,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen,
        );
        let _ = terminal::disable_raw_mode();
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let (tx, mut rx) = mpsc::channel::<Event>(1024);
        self.tx = Some(tx.clone());

        let mut out = term::buffered_stdout();
        Self::enter_terminal(&mut out)?;
        out.flush()?;
        self.renderer.clear_screen();

        if self.config.model.is_none() {
            self.doc.warn("no model — run 'luma sync'");
        }
        self.render();

        let tx_input = tx.clone();
        tokio::task::spawn_blocking(move || input::read_stdin_loop(tx_input));

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
                    Ok(event) => {
                        if self.process_event(event) {
                            self.render();
                            let mut out = term::buffered_stdout();
                            Self::exit_terminal(&mut out);
                            std::process::exit(0);
                        }
                        drained += 1;
                    }
                    Err(_) => break,
                }
            }
            self.render();
        }

        let mut out = term::buffered_stdout();
        Self::exit_terminal(&mut out);
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
