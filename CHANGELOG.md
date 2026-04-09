# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0-beta.6] - 2026-04-09

### Fixed
- Table detection false-positive on tree/diagram lines — `is_table_line()` matched nested ASCII trees like `|   |-- lib.rs` (2 pipes, starts with `|`); tightened to require start AND end with `|` plus at least 3 pipes (two columns minimum)
- Wide tables overflow terminal without wrapping — `flush_table()` pushed rendered lines directly, bypassing `wrap_line()`; now wraps table rows at terminal width like all other content

## [0.4.0-beta.5] - 2026-04-09

### Fixed
- Ctrl+C kills process on Windows instead of clearing input buffer — console sends `CTRL_C_EVENT` signal independently of VT key event; absorb signal with tokio handler so Ctrl+C is only processed as VT byte through terminal reader
- Bash tool panics on multi-byte UTF-8 output — `accumulate()` used raw byte indices to split/slice the head+tail rolling window; index landing inside a multi-byte char (e.g. box-drawing `│`) causes `not a char boundary` panic; now snaps to nearest valid boundary

### Performance
- Remove per-line heap allocations in markdown rendering — `normalise_lang()` no longer calls `to_ascii_lowercase()` on every code line (was 2x per line); `is_horizontal_rule()` replaced `collect::<String>()` + 3 iterator passes with single-pass zero-alloc counter

## [0.4.0-beta.4] - 2026-04-09

### Changed
- Replace crossterm with termina — fixes bracketed paste on Windows at the library level; VT input mode (`ENABLE_VIRTUAL_TERMINAL_INPUT`) means Windows Terminal now sends proper `\x1b[200~...\x1b[201~` paste sequences instead of individual key events
- Terminal instance created once in `App::new()` and reused in `run()` (was created twice)
- Panic hook delegates terminal restore to termina's `set_panic_hook` which opens a fresh PTY and restores original termios/console modes
- VT enable/restore sequences deduplicated into `VT_ENABLE`/`VT_RESTORE` constants
- Event dispatch consolidated — aborting vs normal branches merged into single match with `_ if aborting` guard
- Input reader filters Csi/Osc/Dcs escape sequence responses at source instead of forwarding to dispatch

### Fixed
- Paste on Windows triggers as line-by-line Enter — crossterm's Windows backend uses `ReadConsoleInputW` which decomposes bracketed paste into individual key events; termina enables `ENABLE_VIRTUAL_TERMINAL_INPUT` + `ReadConsoleInputA` so paste arrives as a single `Event::Paste`
- Panic leaves terminal in raw mode — `exit_terminal` now calls `enter_cooked_mode()` explicitly; `drop(term)` before `process::exit` ensures destructors run
- Terminal raw mode not restored on panic — crossterm had no way to call `disable_raw_mode()` from panic hook without the terminal instance; termina's `set_panic_hook` captures original termios and restores it via fresh PTY handle

## [0.4.0-beta.3] - 2026-04-09

### Fixed
- Mouse wheel scroll not working on Windows — Windows Terminal converts wheel events to cursor Up/Down keys in alternate screen (Alternate Scroll Mode); now disabled with `\x1b[?1007l` on enter, restored on exit

## [0.4.0-beta.2] - 2026-04-09

### Fixed
- Spinner flicker on Windows — spinner chars (`·`, `✽`) have `east_asian_width=Ambiguous`, rendering as 2 columns on CJK terminals while cell buffer counted 1; now padded to fixed 2-column width (same approach as Claude Code's `<Box width={2}>`)
- Scroll not working during streaming — arrow keys (Up/Down/PageUp/PageDown) now scroll output while agent is responding; previously went to prompt history
- Scroll up silently ignored during streaming — removed bounce detection that depended on stale state; `scroll_up` now always locks viewport

### Changed
- Spinner chars match Claude Code style (`·✢✳✶✻✽`) with platform-specific substitution (`✳→*` on non-macOS)
- `ScrollView` simplified to 2 fields (`offset`, `is_user_scrolled`) — no more `just_hit_bottom`, `last_bottom_max`, or `cached_total`

## [0.4.0-beta.1] - 2026-04-09

### Added
- Myers O(nd) diff algorithm replacing LCS O(n*m) — faster diffs for large files
- Diff stats in tool output (`Updated file.rs +5 -3`)
- Actionable error messages for 401, 403, 429, 529 HTTP status codes with provider-specific guidance
- Network error formatting — connection failures and timeouts include troubleshooting hints
- 529 (Anthropic overloaded) treated as retryable with automatic backoff
- Stream-level retry with mid-turn session save — recovers from transient network failures
- Global panic hook — restores terminal (raw mode, cursor, alternate screen) on any crash
- Crash diagnostics — panic info + backtrace written to `luma-crash.log` in temp directory
- Dynamic input height — prompt area grows/shrinks with content, scroll indicator when overflow

### Changed
- `install.ps1` rewritten for Windows PowerShell 5.1+ — uses `curl.exe`/`tar.exe` (Win10 built-in), `WM_SETTINGCHANGE` broadcast for cmd.exe PATH, TLS 1.2 forced
- `install.sh` rewritten with structured functions, `curl`/`wget` fallback, `unzip`/`python3` fallback, colored output, fish shell support
- Self-update adds `-ExecutionPolicy Bypass` and TLS 1.2 for Windows PowerShell 5.1
- Scroll bounce detection no longer depends on stale cached layout size — uses `last_bottom_max` from the most recent scroll-down that hit bottom
- `ViewState` removes `cached_total` field; scroll operations read `layout.total_lines()` directly
- Synchronized output (Mode 2026) enabled on all platforms — fixes spinner flicker on Windows Terminal, harmlessly ignored by legacy terminals

### Fixed
- Mouse scroll not working on Windows — removed region bounds guard that failed with crossterm's `parse_relative_y` on Windows
- Scroll-up during streaming silently ignored — stale `cached_total` caused bounce detection to always trigger, preventing scroll lock from engaging
- Mouse scroll and keyboard input dropped during heavy streaming — `blocking_send` replaces `try_send` in stdin reader
- Session resume fails with 400 Bad Request after crash — orphaned `tool_use` blocks now repaired with `[aborted]` placeholder on `LoadSession`
- Spinner flicker on Windows status bar — Mode 2026 was compile-time disabled for all Windows builds
- Panic crashes leave terminal in broken raw mode state
- `luma update` fails on Windows PowerShell 5.1 (`New-TemporaryFile`, inline `if`, `Expand-Archive` incompatible)
- Install scripts not updating PATH for cmd.exe
- Fish shell PATH hint using wrong syntax (`export` instead of `fish_add_path`)
- Install script fails on systems without `curl`
- Clipboard copy using OSC 52 — now uses `pbcopy` on macOS, `clip.exe` on Windows
- Prompt input not wrapping at region boundary
- Cursor position not tracking actual wrap boundaries
- Non-portable escape sequences crash on Windows conhost
- Cancel in-flight turn not triggered on mode switch
- Partial SSE stream message lost on incomplete stream

## [0.3.0-beta.9] - 2026-04-09

### Fixed
- Spinner flicker on Windows status bar — synchronized output (Mode 2026) was disabled at compile time for all Windows builds; now enabled unconditionally (harmless on terminals that don't support it, fixes flicker on Windows Terminal)
- Session resume fails with 400 Bad Request when previous session crashed mid-tool-execution — orphaned `tool_use` blocks without matching `tool_result` now repaired on `LoadSession`
- Panic crashes leave terminal in broken state (raw mode, no cursor) — added global panic hook that restores terminal before exit
- Crash diagnostics: panic info + backtrace written to `luma-crash.log` in temp directory

## [0.3.0-beta.8] - 2026-04-09

### Fixed
- Mouse scroll and keyboard input silently dropped during heavy streaming — `try_send` replaced with `blocking_send` in stdin reader so terminal events are never lost
- `luma update` fails on Windows PowerShell 5.1 — `New-TemporaryFile`, inline `if` assignment, and `Expand-Archive` are PS 5.0+/7.0+ only
- Install scripts not updating PATH for cmd.exe — added `WM_SETTINGCHANGE` broadcast and cmd-compatible PATH hint
- Install script (`install.sh`) fails on systems without `curl` — added `wget` fallback
- Fish shell PATH hint incorrectly using `export PATH=...` instead of `fish_add_path`

### Changed
- `install.ps1` rewritten to use `curl.exe`/`tar.exe` (built into Windows 10+) instead of `Invoke-WebRequest`/`Expand-Archive`, eliminating PowerShell version compatibility issues
- `install.sh` rewritten with structured functions (`detect_os`, `detect_arch`, `resolve_version`), colored output, and `unzip`/`python3` fallback for zip extraction
- Self-update command adds `-ExecutionPolicy Bypass` and forces TLS 1.2 for Windows PowerShell 5.1

## [0.3.0-beta.7] - 2026-04-09

### Added
- Actionable error messages for 401 (auth hint + `luma sync`), 403 (permission check), 529 (Anthropic overloaded)
- Network error formatting — connection failures and timeouts now include guidance instead of raw reqwest output
- 529 (overloaded) treated as retryable in `send_with_retry`

### Changed
- `format_http_error` dispatches on status code instead of only handling 429
- TUI-side `format_provider_error` simplified — defers to `has_actionable_guidance` to avoid double-wrapping

### Fixed
- Mouse scroll not working on Windows — removed region bounds guard on `ScrollUp`/`ScrollDown` events that failed when `parse_relative_y` produced coordinates outside region bounds

## [0.3.0-beta.4] - 2026-04-08

### Added
- Provider retry events surfaced in TUI during temporary throttling
- Shared provider retry module with backoff, jitter, and provider-aware guidance

### Changed
- Rate limit handling now distinguishes temporary throttling from hard quota exhaustion
- Retry delay selection now prefers `Retry-After`, then OpenAI/Codex `x-ratelimit-reset-*`, then exponential backoff

### Fixed
- Claude, OpenAI, and Codex provider flows now surface clearer `429` guidance
- OpenAI/Codex retry handling is consistent across providers

## [0.3.0-beta.3] - 2026-04-08

### Added
- **GitHub tools**: `GhFile`, `GhLs`, `GhSearch` for browsing remote repositories
- **WebFetch tool**: fetch and extract web page content with BM25 relevance ranking
- **WebSearch**: improved client-side search with structured result display
- **Tab completion**: Tab fills dropdown item without accepting; preserves mode cycling
- `/resume` command hidden when already in a thread

### Fixed
- SSE stream corrupting multi-byte UTF-8 characters (e.g. Vietnamese diacritics) when chunk boundary splits a codepoint
- UTF-8 panic in syntax highlighter when operator follows multi-byte character
- `ContentBlock::Paste` not serialized to API, causing 400 empty content errors
- Token counting and cache display in status bar
- Session resume showing blank screen due to lazy block rendering
- Install scripts not adding PATH, platform-aware self-update

## [0.2.0] - 2026-04-06

### Added
- **Web search** with capability-based server tool architecture
  - Built-in server search: Anthropic (`web_search_20250305`), Codex (`web_search`)
  - Client-side fallback: Exa, Tavily, SearXNG adapters (via `EXA_API_KEY`, `TAVILY_API_KEY`, `SEARXNG_URL`)
  - Unified UI: query shown immediately, 1-line results with title and domain
- **Read tool**: file size guard (10MB), binary file detection, "did you mean?" suggestions, BOM handling, total line count
- **Edit tool**: curly-quote normalization, file size guard, "did you mean?" suggestions, skip unchanged files
- **Write tool**: skip unchanged files, distinguish "Created" vs "Updated" in response
- **Bash tool**: head+tail output truncation (preserves errors at end), deadline-based timeout
- **Glob/Grep**: `ignore` crate integration (respects `.gitignore`, skips hidden/binary files)
- Syntax highlighting for tool diff blocks with language detection from file path
- Function/method call coloring in code highlighting
- JS builtins (`console`, `Promise`, `Array`, etc.) as highlighted keywords
- Emoji rule enforced across all prompt modes including Rush

### Changed
- **Tool naming**: tools carry API-native names directly (`Read`, `Write`, `Edit`, `Bash`, `Glob`, `Grep` for Claude; `exec_command`, `apply_patch` for Codex) — no wire name mapping layer
- **Registry**: simplified from 95 to 50 lines; removed `wire_name_map`, `wire_to_canonical`, `canonical_to_wire`, `set_wire_names`
- **Server tools**: capability-based architecture — Registry declares capabilities, each Provider maps to its native schema format at call time
- **Prompt system**: extracted to template files (`src/config/prompt/`), shared sections composed via `include_str!` + `{tools}` placeholders
- **Tool icons**: simplified to two types — `->` (read/output tools) and `<-` (write/input tools)
- `BashTool` parameterized for dual naming: `BashTool::claude()` / `BashTool::codex()`
- Edit tool uses single scan instead of `contains()` + `matches().count()`
- Write/Edit diff output uses `send().await` instead of `try_send` (no more silent data loss)

### Fixed
- Tool diff blocks not showing syntax highlighting or background colors (wire name case mismatch)
- Bash timeout resetting on each output chunk (now uses fixed deadline)
- Read tool loading entire file into memory for large files with offset (now uses `BufReader`)
- Long lines in Read output not truncated (now capped at 2000 chars)

## [0.1.0] - 2026-04-05

### Added
- Initial release of LUMA
- Multi-provider support (Anthropic Claude, OpenAI Codex)
- Terminal User Interface (TUI) with interactive agent
- Three operation modes:
  - **Rush**: Fast responses with Claude Haiku (fallback Sonnet)
  - **Smart**: Balanced responses with Claude Opus (fallback Sonnet)
  - **Deep**: Advanced analysis with Codex (fallback Opus)
- Token usage tracking per session
- Skill system compatible with Claude Code format
- Session persistence and resumption
- Built-in tools: `read`, `write`, `edit`, `bash`, `grep`, `glob`, `apply_patch`
- Zero-config authentication (reuses existing Claude Code and Codex credentials)
- Automatic OAuth token refresh
- Syntax highlighting for code blocks
- Keyboard shortcuts and slash command system
