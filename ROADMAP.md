# Roadmap

Current: ~12K lines Rust, 290 tests, 3 providers, 12 tools. Cross-platform (macOS, Linux, Windows).

## v0.2 — Done

- [x] Tool diff colors + syntax highlighting
- [x] Wire name removal — tools carry API-native names
- [x] Prompt templates (`src/config/prompt/`)
- [x] Tool improvements: BufReader (Read), deadline timeout (Bash), skip unchanged (Write), curly-quote (Edit), ignore crate (Glob/Grep)
- [x] Web search — server tools + client fallback
- [x] Non-blocking input, lazy block rendering
- [x] Session resume, save on error/abort, auto-resume

## v0.3 — Architecture + Multimodal — Done

- [x] **TUI architecture redesign** — Document (model) + Layout (render cache) + ViewState (orchestrator). Unidirectional data flow, pull-based dirty detection via `Block::snapshot()`
- [x] **Block render pipeline** — `block/` module: mod, render, text, tool, chrome, diff. TextCache owned by Layout via `RenderState`. Read-only `&[Block]` rendering
- [x] **Screen architecture** — `Screen::Welcome { lines }` / `Screen::Chat`. Data on variant, Rust drops on transition. No `has_logo` flag
- [x] **Renderer FloatingLayer** — dropdown/picker as overlay layer in flush pipeline. No content cloning. Works on all screens
- [x] **Segment prompt buffer** — `PromptBuffer` with `Vec<Seg>`. Single source of truth for editing. Image/paste insert at cursor, backspace removes naturally
- [x] **ContentBlock pipeline** — `Vec<ContentBlock>` from prompt → app → doc → agent → session → resume → render. No XML serialization. `ContentBlock::Paste` variant
- [x] **Shared content rendering** — `content_lines()` for both prompt input and user chat bubble
- [x] **Abort handling** — tool results always pushed, `[user interrupted]` system message for LLM context, active tools marked "aborted"
- [x] **Input normalization** — `\r\n` / `\r` → `\n`. Image drag-drop with `file://` and quote stripping
- [x] **Esc/Ctrl+C separation** — Esc = interrupt streaming, Ctrl+C = clear buffer / quit
- [x] **Cross-platform** — Windows install (`install.ps1`), Git Bash support, CI paths-ignore for docs
- [x] **@file mention** — fuzzy autocomplete, file content injection, `@path` highlighting
- [x] **Image attach** — clipboard paste, drag-drop, multimodal provider serialization
- [x] **GitHub tools** — `GhFile`, `GhLs`, `GhSearch` for remote repository browsing
- [x] **WebFetch** — fetch and extract page content with relevance ranking
- [x] **Web search polish** — improved client fallback and structured result display
- [x] **Completion polish** — Tab fills dropdown item without immediately accepting
- [x] **Session resume polish** — hide `/resume` when already in-thread, fix blank resume screen
- [x] **UTF-8 streaming fixes** — robust SSE chunk handling and safer highlighter boundaries
- [x] **ContentBlock serialization fix** — pasted blocks serialized correctly to provider APIs
- [x] **Install/update polish** — PATH handling and platform-aware self-update
- [x] Legacy cleanup — removed custom deserializer, OutputLog, viewport, composite_overlay

## v0.4 — Performance & Polish

- [x] Diff algorithm: Myers O(nd) replacing LCS O(n*m)
- [x] Diff stats in tool output ("Updated file.rs +5 -3")
- [ ] Bare URL highlighting in markdown
- [ ] Streaming resize stability across providers
- [ ] Large-session profiling (65K+ tokens) — render throughput, memory, freeze points
- [ ] Tool output polish — clearer summaries and more consistent formatting
- [x] Provider error surfacing — clearer transient/network failure messages in TUI
- [x] Rate limit UX — surface provider `429` errors clearly in TUI with actionable guidance

## v0.5 — UX & Extensibility

- [ ] MCP server support — connect to external tool servers
- [ ] Custom tools via config — shell commands + schema
- [ ] File watcher — detect external changes, warn before overwrite
- [ ] Copy selection to clipboard
- [ ] Search within output (Ctrl+F)
- [ ] Themes — user-configurable color schemes
- [ ] Keybind customization

## v1.0 — Production

- [ ] Comprehensive error messages with actionable guidance
- [ ] Rate limit handling — backoff + retry
- [ ] Audit log — record all tool executions
- [ ] Config validation on startup
- [ ] man page and shell completions (bash, zsh, fish)
- [ ] Benchmark suite — render, throughput, memory

## Non-goals

- GUI — terminal tool
- Built-in editor — use your editor
- Multi-user — single user, single session
