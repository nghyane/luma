# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
