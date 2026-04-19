# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0-beta.1] - 2026-04-19

### Added
- **MCP remote transport & OAuth auth flow** — MCP servers can now run over HTTP+SSE with full OAuth 2.0 authorization (RFC in `docs/rfc/`)
- **MCP DCR fallback & auto-discovery** — dynamic client registration with automatic metadata discovery; `luma mcp revoke` command to clear stored tokens
- **MCP CIMD support (SEP-991)** — when auth server advertises `client_id_metadata_document_supported`, uses URL-based client ID instead of dynamic client registration
- **Bash artifact expansion** — `artifact://ev/{id}` references in shell commands are expanded to local blob paths before execution

### Fixed
- **MCP remote TLS** — enabled rustls for rmcp's reqwest transport; HTTPS connections to remote MCP servers (e.g. Figma) were silently failing
- **MCP OAuth DCR scope** — DCR requests now include `scopes_supported` from auth server metadata when entry scopes are empty (required by Figma)
- **MCP OAuth DCR client name** — use `Claude Code ({name})` format for DCR `client_name` to pass server-side allowlists
- **MCP CLI runtime panic** — `luma mcp auth` and `luma mcp revoke` no longer panic with "Cannot start a runtime from within a runtime"
- **Tab rendering in TUI** — tab characters (`\t`) were rendered as zero-width (skipped), causing files with tab indentation (common in JSX/TSX) to appear flush-left in diff output; tabs now expand to 4 spaces
- **Text selection excludes line numbers** — selecting diff/edit output no longer copies gutter (line numbers, +/- markers); clipboard via OSC 52 now contains clean code only
- **Scrollbar not copied on select** — scrollbar uses background color instead of block characters, so text selection is clean
- **Evidence preview marker** — clearer preview marker for single-line payloads
- **Tool-input JSON parse failures** — parse errors in tool inputs are now surfaced instead of silently dropped

## [0.4.0-beta.22] - 2026-04-17

### Changed
- **Skill discovery: multi-CLI interop** — now scans `.kiro/skills/` and `.codex/skills/` (workspace + user-level) alongside existing `.agents/skills/` and `.claude/skills/` roots; skills authored for Codex, Kiro, or Claude Code are picked up transparently
- **Recursive skill scan** — walks each root up to 6 levels deep so Codex nested layouts (`.system/plan/`, category folders) are discovered without extra config
- **Robust YAML frontmatter parser** — handles block scalars (`>` / `|`), quoted values, BOM, and unknown fields (`license`, `metadata`, `compatibility`) without errors; forward-compatible with agentskills.io schema
- **Skill name fallback** — when `name:` is missing from frontmatter, the containing folder name is used (Codex convention)
- **Skill directory hint on load** — reading a skill via `artifact://skill/{name}` now prepends `[skill-directory: /absolute/path]` to the output so the agent can reference companion files (scripts, templates, assets) by absolute path
- **Unified skill detection** — `turn.rs` (UI events) and `audit.rs` (metrics) share a single `parse_skill_read_path()` parser; audit `sessions_with_skill_loads` counter now matches UI exactly
- Skill name charset widened to `[A-Za-z0-9_.-]+` (adds `.` for `v1.2`-style names seen in the wild)
- Duplicate skill names across discovery roots now log a debug warning instead of silently shadowing

### Removed
- Legacy `SKILL.md` filesystem path detection (beta reset — only `artifact://skill/{name}` URI is recognised)
- `<directory>` and `<assets>` XML blocks from skill catalog (replaced by runtime directory hint)
- `artifact://skill/{name}/{subpath}` URI form (agent uses absolute paths from the hint instead)

## [0.4.0-beta.21] - 2026-04-17

### Added
- **MCP (Model Context Protocol) client integration (RFC 0015)** — agent can now call tools from external MCP servers alongside built-in tools
  - `src/mcp/` module: `config` (load + merge), `manager` (spawn + route calls), `bridge` (`McpTool` implementing `core::tool::Tool`), `cli` (subcommands)
  - Stdio transport via `rmcp 1.5` with `transport-child-process` (HTTP/SSE deferred to phase 2)
  - Config format matches Claude Code's `mcpServers` shape byte-for-byte; reads `.luma/mcp.json` (project) and `~/.config/luma/mcp.json` (user), with fallback to `.claude/settings.json`
  - Tool naming `mcp__{normalized_server}__{tool}` matches Claude Code convention
  - 30s connect timeout with stderr capture — failed servers surface subprocess stderr in the error message instead of a generic "connection closed"
  - Per-server failure isolation: one bad server does not block luma startup or affect other servers
- `luma mcp list` / `luma mcp add <name> -- <cmd> [args...]` / `luma mcp remove <name>` CLI subcommands, with `-e KEY=VAL` support for env vars
- `/mcp` TUI command showing connection status (connected + tool count, or failed + error) and prompt autocomplete entry

## [0.4.0-beta.20] - 2026-04-17

### Changed
- Agent panic hook now extracts detail from `&str` / `String` payloads before reporting, surfacing the real cause instead of "unknown"
- `expect` on event channel send replaced with graceful error return so a dropped receiver cannot abort the agent task
- Max-tokens escalation now emits `ToolOutput` (not `Token`) so the UI shows escalation notices as inline tool output rather than assistant text

## [0.4.0-beta.15] - 2026-04-16

### Added
- `context_usage_emitted` field on `StreamResponse` — providers declare whether they already emitted `Event::ContextUsage`, so the turn-layer fallback only fires for providers that report neither tokens nor context usage
- `provider::estimate_context_chars` shared utility — counts all content blocks (text, tool_use input, tool_result, thinking) plus tool spec JSON, matching Kiro CLI's `TokenCounter` algorithm

### Changed
- Context usage estimation now matches Kiro CLI exactly: token formula `(chars/4 + 5) / 10 * 10` with full content block counting instead of text-only `chars/4`
- Kiro provider now owns context usage reporting — emits server `contextUsageEvent` percentage when available, falls back to client-side Kiro CLI algorithm otherwise; turn-layer fallback no longer overwrites server-authoritative values

## [0.4.0-beta.14] - 2026-04-15

### Added
- RFC 0011: Image Attachment Routing
- Image preprocessing for `Read` via the `image` crate, including decode, resize, and compression before attachment
- AWS IAM Identity Center and Builder ID login support (SSO OIDC device flow)
- SQLite-backed auth repository (`auth.db`) with `BEGIN IMMEDIATE` for cross-process safety
- Streaming tool execution: `StreamRequest` gains `tool_use_tx` channel; Anthropic provider dispatches `ToolUse` blocks mid-stream so the turn loop can collect them before the response is fully assembled
- Error-as-message recovery: transient stream errors are injected into the conversation as system messages and the tool loop retries (up to 2 consecutive failures) instead of killing the turn
- Max-output-tokens recovery loop: when the model hits the output token limit the turn injects a "resume directly" nudge and retries up to 3 times, mirroring Claude Code's `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT`
- Kiro protocol chunk idle timeout (120 s) — emits retryable `StreamInterrupted` instead of hanging until OS TCP timeout (~7 min on macOS)

### Changed
- Tool-result image routing is now selected centrally per provider (`inline`, `user attachment`, or adapter-managed text fallback)
- Kiro now receives images from `Read` through the user attachment path instead of silently dropping image tool results
- OpenAI Chat keeps provider-specific text fallback for multimodal tool results while Anthropic and OpenAI Responses continue to send real image bytes inline
- Login/account persistence now flows through the new auth service and repository layer across CLI, config compatibility shims, and TUI integration
- Auth storage migrated from JSON file to SQLite — matches Kiro CLI architecture for multi-process safety
- Import command now merges accounts (upsert) instead of replacing all existing accounts
- Expired refresh tokens are now handled gracefully with automatic re-authentication prompts
- Agent tool loop changed from `for _ in 0..50` to unbounded `loop {}` — exits only when the model stops calling tools, the user cancels, or recovery limits are exhausted
- Stream retry budget increased from 2 attempts / 2 s delay to 4 attempts / 3 s delay for better resilience against transient network failures

### Removed
- Provider-specific ad hoc image-routing logic that previously lived inside individual adapters
- Auth daemon (`authd/`) — replaced by SQLite locking, no daemon needed
- Legacy JSON file auth store (`auth.json`) and V1/V2 migration code

## [0.4.0-beta.13] - 2026-04-14

### Added
- RFC 0009: Auth, Routing, and Provider Runtime Rearchitecture
- Shared non-SSE stream transport helper for immediate cancellation semantics

### Changed
- Auth architecture migrated to `AuthService` + `AuthRepository` with `src/auth/oauth/*` providers
- Claude, Codex, and Kiro login/resolve/refresh now route through the new auth source of truth
- API key accounts are now persisted and resolved through `AuthService` and `AuthRepository`
- `config::auth` has been reduced to a thin compatibility shim over the new auth system
- Streaming stop behavior is now immediate across SSE consumers, Kiro chunk streams, and the TUI abort path

### Removed
- Legacy singleton-backed auth business logic from `config::auth`
- Legacy PKCE implementation under `src/config/auth/pkce.rs`

## [0.4.0-beta.12] - 2026-04-13

### Fixed
- Windows: backslash path separators no longer stripped when pasting image paths

## [0.4.0-beta.11] - 2026-04-12

### Added
- **Kiro (Amazon Q) provider** — full gateway with AWS Event Stream protocol, OAuth login flow, account metadata, and session-stable conversation IDs
- **OpenCode Go provider** — support for kimi-k2.5, glm-5, mimo-v2, minimax models with arrow-key login picker in CLI
- Kiro model catalog expanded: Opus 4.5/4.6, Sonnet 4.6, Haiku 4.5
- Kiro web search routed through free MCP `web_search` tool for Kiro sessions
- `Read` tool gains capability-aware image support — passes images only when the active model declares `vision`
- Models auto-sync on staleness; `prompt_caching` flag captured per model from provider snapshot
- Kiro model list scanned live via `ListAvailableModels` API

### Changed
- Mode and model switches are now deferred until the next submit, matching the "staged change" UX of other picker dialogs
- Tool registry is rebuilt when the model changes across providers so tool IDs stay consistent
- System prompt and tool registry hot-swap on mode change without requiring a restart
- Prompt and tool axes split into `mode` (behavior) and tool style (provider) so the two can vary independently
- Kiro switched to Coral endpoint; `contextUsagePercentage` surfaced from server response
- `envState` shipped on every user turn for Kiro; context percentage derived from server value × model window
- Provider architecture refactored: per-gateway trait impls (one file per gateway), `ProtocolId` + per-binding `base_url`, `QuirkSet` bitflags, typed `ProviderUnauthorized` replacing keyword-classifier
- Model picker shows `{source}/{id}` and disambiguates models with the same ID across providers
- Preferences persist `{source}/{id}` for per-mode model selection; bare-id fallback removed
- `perf(kiro)`: bytes-identical request body enables server-side auto-cache

### Fixed
- Prompt input no longer inserts literal characters for Ctrl/Alt-modified keys on Windows consoles (fixes Delete rendering as `h` via legacy Ctrl+H); added proper handling for Delete (forward-delete), Home, End, Ctrl+H (backspace), and Ctrl+W (delete word)
- Mouse any-motion tracking (`?1003`) disabled and bare `Moved` events short-circuited, eliminating an event-bus flood that could stall keyboard/scroll input mid-stream on Windows
- Terminal restore sequence now resets SGR and clears the main screen after leaving the alternate screen, so the shell is clean on exit
- Stdin reader filters `Char('\0')` key events emitted by Windows for bare modifier presses
- Windows startup now forces console code page to UTF-8 (`chcp 65001`) so legacy `cmd.exe` renders box-drawing and decorative glyphs correctly
- Removed double gap between thinking and tool blocks caused by `render_thinking` appending a trailing empty line on top of `auto_gap`
- Removed the dark half-cell gap at the bottom of the prompt separator by switching to a lower half-block with swapped fg/bg
- Kiro auth: captured `login_option` now passed through token exchange correctly
- Kiro streaming fixed — real SSE streaming with stable conversation IDs across turns
- OpenCode Go: `/v1/messages` requires `x-api-key` header, not `Bearer`; OpenCode Go uses `Bearer` for OAuth, not API keys
- OpenCode Go models missing from picker; picker no longer leaves prior terminal content on exit
- OpenAI Chat: runtime owns `/v1` path prefix, not the gateway `base_url`
- Auth: `AuthKind` classified by vendor instead of `account_id`; actionable error on API-key 401, credentials never auto-deleted
- Skill block cache invalidated when skill name changes
- Multi-line paste rendering and cursor position corrected
- Stale `/compact` suggestion replaced with actionable guidance in error messages

## [0.4.0-beta.10] - 2026-04-12

### Changed
- Chat rendering now uses explicit follow modes so normal frames still auto-follow while tool-stream frames preserve the current viewport offset
- Streaming `ToolInput` and `ToolOutput` events trigger an immediate partial render, keeping long-running tool output live without a full redraw pass
- Structured file-change blocks now render through the artifact path more consistently, with clearer collapsed and expanded output for multi-file diffs

### Fixed
- Tool streaming no longer snaps the chat view back to bottom when the user is reviewing older content mid-run
- Tool block snapshots now cover artifact-backed collapsed and expanded diff states, reducing regressions in structured tool rendering

## [0.4.0-beta.9] - 2026-04-11

### Added
- Multi-account auth pool with interactive OAuth login — PKCE flow for Anthropic and OpenAI/Codex (`src/config/auth/pkce.rs`) spins up a loopback listener, opens the system browser, and writes the new account straight into the shared pool; reachable from the CLI (`luma login [anthropic|openai]`, `luma accounts`) and the TUI (`/login`, `/accounts` dialog)
- `/accounts` centered modal dialog (`src/tui/dialog.rs`) — lists every known account with provider and health (`ok`, `cooling`, `relogin`, `off`); keyboard actions to toggle disabled and remove
- Automatic pool failover on HTTP 429 — turn-level retry loop marks the current account on cooldown via `auth::mark_rate_limited`, resolves the next healthy account for the same provider, and rebuilds the provider transparently; surfaces `all accounts cooling` only when every account for the provider is exhausted
- Typed `ProviderRateLimited` error so the turn loop can distinguish rate limiting from transient 5xx and drive the failover path without string-matching
- Rate-limit header parsing (`parse_rate_limit_headers`) for Anthropic (`anthropic-ratelimit-*`, HTTP-date resets) and OpenAI/Codex (`x-ratelimit-*`, epoch resets) — normalized into a shared `UsageSnapshot` and recorded against the account label that issued the request
- Pool-health chip in the status bar — only rendered when at least one account is cooling or needs relogin (zero visual noise when everything is healthy)
- `FileChangeArtifact` / `FileArtifact` / `FileOp` / `ToolStatus` data model in `core::types` — shared structured result for Write, Edit, and `apply_patch` tools, streamed to the TUI through a new `Event::ToolArtifact`
- `ToolExecution` wraps a tool's string result together with an optional artifact so file-changing tools can return structured data without losing the streaming path
- Built-in `models.catalog.json` overlaid onto discovered models — provides `display_name`, `context_window`, and `max_output_tokens` metadata when the provider snapshot omits them

### Changed
- `config::auth` split into a proper module (`auth/mod.rs`, `auth/policy.rs`, `auth/pkce.rs`) replacing the single 500-line file — policy module owns `AuthFailureKind` classification and OAuth refresh-request building; account identity is `account_id` or `email` instead of a label string
- `pick_candidate` now ranks candidates (`email > account_id > refresh_token > expires_at`) instead of taking the first match — accounts with real identity beat anonymous placeholders and failover picks the best-credentialed account
- `upsert_by_label` merges by identity key so a fresh login that arrives with a different label still merges into the existing entry, preserving cooldown and usage state
- `ensure_bootstrapped` no longer short-circuits when the pool already has the provider — the identity-based merge path makes re-imports idempotent and picks up keychain rotations
- `dedup_accounts` drops anonymous legacy labels (`anthropic-1`) when a richer entry exists for the same provider
- `try_refresh` wraps the OAuth HTTP call in a 20-second timeout so a stuck network no longer freezes the agent loop
- Write, Edit, and `apply_patch` tools rewritten to return `ToolExecution` with a structured `FileChangeArtifact` instead of `(String, wire-format diff)` strings parsed by the TUI — removes string-based special casing and restores expand/collapse for every file-changing tool
- `render_tool` routes on `artifact.is_some()` instead of sniffing tool names, so any future tool that emits `FileChangeArtifact` gets the diff block renderer for free
- `post_sse` takes an `account_label` so the retry layer reports 429 events and usage snapshots back to the pool under the right label
- `is_auth_error` in `agent/turn.rs` takes the provider name as a parameter instead of hardcoding `"openai"`, so Anthropic auth errors classify with the correct policy
- `format_http_error` 401 message suggests `/login` instead of the removed `luma sync` command
- `StatusBar::reset_cache` renamed to `reset_usage` — now resets context tokens and pct together with cache counters when switching threads, fixing stale token count carry-over across `/new` and `/resume`
- `ThinkingLevel::as_str()` is a `const fn` returning `&'static str`, matching `AgentMode` / `AuthProvider` and removing manual match at every call site
- `AgentCommand::LoadSession` gains `is_new: bool` so the caller (App) owns new-vs-resume classification instead of the agent inferring from `messages.is_empty()`
- New-thread and resume flows unified — both go through `LoadSession`, the agent emits `SessionLoaded` after replacing state, and the TUI updates only from that acknowledgement (removes UI/agent state races and duplicated orchestration)
- Removed the background-refresher task — synchronous refresh in `resolve_inner` covers every case the background path did, without the extra complexity

### Fixed
- Account races on multi-client setups — when another CLI rotates the keychain, merging by identity key keeps a single pool entry and inherits the latest refresh token instead of creating a duplicate anonymous row
- `fix_orphaned_tool_uses` was called twice on the error path in `run_chat_turn` — only repair when `Aborted` (the happy path already repaired the message)
- `epoch_days_to_ymd` underflowed on negative epoch days (dates before 1970) — return type is now `(i32, u32, u32)` with signed era arithmetic so `Date::format` round-trips pre-epoch timestamps correctly
- Apply-patch error for a missing `@@` context now names the context hint and the path instead of a generic "failed to find context" — makes patch failures actionable from the tool output alone
- Stale token counter on `/new` and `/resume` — status bar now resets context tokens and pct, not just cache

### Internal
- `getrandom` dependency added for PKCE verifier / state generation
- `clippy::cast_lossless` enabled — widening numeric casts across hot paths (text buffer hash, JSON escape decoder, retry date parsing, render geometry) now use `From` instead of `as`; RULES.md section VI documents the numeric cast policy
- `format_http_error` 401 message updated to point at `/login`

## [0.4.0-beta.8] - 2026-04-10

### Fixed
- Write/Edit tool block stays blank during Anthropic's ~10s pause between the `path` and `content`/`new_string` fields — introduce `Event::ToolSelected` emitted by providers as soon as a tool_use block starts, so the UI shows a "preparing Write..." card immediately and the preview fills in as deltas arrive; tool lifecycle now flows `ToolSelected (provider) → ToolInput* (provider) → ToolStart (orchestrator) → ToolOutput* (orchestrator) → ToolEnd (orchestrator)` with each event owned by exactly one layer
- Pending tool blocks could stay in "preparing..." forever when a tool_use was discarded mid-turn (provider retry, max_tokens escalation, stream cut) — `Document::close_pending()` scans the full block list and finalises every unfinished tool/skill block; wired into `on_agent_done`, `on_agent_error`, `provider_retry`, and `abort` so every orchestration seam cleans up after itself
- `abort()` only walked backwards from the tail and broke at the first non-tool/skill block, missing any pending tool buried under later content — now reuses `close_pending` which walks the full document

## [0.4.0-beta.7] - 2026-04-10

### Added
- Coalescing event bus (`src/event_bus.rs`) replacing bounded `mpsc` between provider and TUI — merges consecutive `Token`/`Thinking`/`ToolInput` deltas into queue tail so bursty streams never drop events and never stall the provider loop; soft cap on coalesced bytes + hard cap on unmergeable entries provides backpressure only under sustained UI stalls
- Streaming JSON string extractor (`src/provider/json_stream.rs`) — extracts a single top-level string field from `partial_json` tool-call deltas as bytes arrive, with correct escape + UTF-8 boundary handling
- Per-tool `streamable_arg` in `ToolSchema` — tools opt into live preview of one argument (`content` for Write, `new_string` for Edit, `command` for Bash); provider layer no longer hardcodes tool names
- `StopReason` on provider responses and per-request `max_tokens_override` — turn loop now escalates Claude output cap from 8192 → 64000 once on a `MaxTokens` stop, matching claude-code's `max_output_tokens_escalate` path
- `force_refresh` path in auth — after a 401 the client always round-trips the OAuth endpoint instead of trusting local expiration, fixing stale-cache auth failures when another CLI rotates the keychain
- Responsive markdown table rendering — wide tables shrink columns proportionally and wrap cell content to multiple lines per row (like HTML `<td>`) instead of overflowing the terminal
- `StreamRequest` / `StreamResponse` structs bundling provider call inputs/outputs — smaller `Provider` trait signature, easier to extend without churning every impl
- `supports_max_tokens_override` provider capability flag — callers skip the escalation retry for providers (Codex) whose backend ignores the override

### Changed
- Provider layer reworked around `StreamRequest` + coalescing event bus: Claude, OpenAI, and Codex rewritten to stream via the shared bus; SSE parser split into `SseLineBuffer` (pure byte→event) + background reader task with end-to-end backpressure
- Claude tool-call preview now uses `JsonStringExtractor` + `streamable_arg` instead of ad-hoc per-tool parsing; OpenAI/Codex follow the same path
- Prompt submission refactored — `PromptBuffer::take_content()` consumes text + image attachments atomically into `(blocks, images)` and clears the buffer, replacing separate `to_content()` / `take_images()` calls that could drift out of sync
- Auth resolve picks whichever source has the freshest refresh_token (managed cache vs local keychain) and fails loudly with actionable errors when no refresh is possible, instead of silently returning an expired credential
- Codex access token expiration extracted from the access_token JWT's `exp` claim directly (authoritative) instead of relying on `last_refresh` hints from `auth.json`
- Auth retry on 401 now shows `token rejected, refreshing…` and always force-refreshes, even if `is_expired` returns false
- `context_window()` returns a constant 200K default — removed unused `context_windows` map from model snapshot
- `Provider::thinking()` getter removed; trait is now smaller and `set_thinking` is called once before boxing
- Turn execution refactored around a `TurnCtx` struct — retries and escalation share one context instead of threading 7 parameters

### Fixed
- Token preview of long tool arguments stalls after first chunk — previous code re-parsed full accumulated JSON on every delta; `JsonStringExtractor` is incremental and correct at chunk boundaries, including mid-escape `\uXXXX`
- `take_images()` left empty text segments behind, causing a stale cursor position after submit — `take_content()` clears the buffer in one step
- Stream-level backpressure broken on fast providers — bounded `mpsc(1024)` could drop events under burst; event bus is lossless by construction
- File completion cache incorrectly gated on both "empty AND invalid" — completions now hide as soon as the cache is invalidated
- Auth cached-clear path could resurrect a stale credential on next resolve because local source was re-read without a refresh attempt; cache clear is no longer exposed — `force_refresh` is the only path
- Clipboard image send used `try_send` and could silently drop on a full channel — now uses `blocking_send` on the event bus so paste never goes missing

### Performance
- `ScreenBuffer::row_hash` switched from `DefaultHasher` (SipHash) to FxHash-style multiply-add packing all cell fields into 2 u64 folds per cell — 6.9x speedup (63 µs → 9 µs per row) on large sessions; per-frame diff is no longer the bottleneck in 65K+ token sessions (see ROADMAP)
- `flush_table` no longer double-wraps table rows — `render_table` now wraps to `max_width` directly, removing a redundant pass through `wrap_line`

### Internal
- `tokio-util` gains the `rt` feature for `AbortOnDropHandle` on the SSE reader task
- Layout bench harness under `#[cfg(test)] mod layout_bench` for ad-hoc profiling on large documents

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
