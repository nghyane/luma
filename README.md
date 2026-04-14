# luma

> Lightweight terminal coding agent built with Rust.

Multi-provider AI support, local code tools, sessions, skills, and a fast TUI for day-to-day development work.

![demo](demo.gif)

## Quick start

1. Install `luma`
2. Log in to at least one provider
3. Run `luma` inside your project

macOS, Linux, WSL (aarch64, x86_64):

```bash
curl -fsSL https://raw.githubusercontent.com/nghyane/luma/master/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/nghyane/luma/master/install.ps1 | iex
```

Then:

```bash
luma login
luma
```

If you already use Claude Code or Codex CLI, `luma` can reuse those credentials automatically when available.

## First run

Useful commands when getting started:

```bash
# interactive login picker
luma login

# check which providers/accounts are available
luma auth
luma accounts

# refresh model catalog
luma sync

# start the TUI
luma
```

Inside the app:

- `Tab` switches mode: `Rush -> Smart -> Deep`
- `/model` opens the model picker
- `/new` starts a new thread
- `/resume` resumes the last session for the current workspace
- `/sessions` browses saved sessions
- `/accounts` shows the account pool
- `/exit` quits

## What luma can do

### Terminal coding workflow

- Chat with AI models in a full-screen TUI
- Work inside the current project directory
- Read, search, edit, and patch files through agent tools
- Run shell commands from the agent loop
- Resume prior sessions per workspace

### File and prompt context

- `@file` mention with path autocomplete
- Multiple file mentions in one prompt
- Inline pasted text blocks
- Image attachments in prompts
- Drag-and-drop image files into the terminal

### Tools

Depending on the active model/provider workflow, `luma` exposes a tool set built around local coding tasks:

- `Read`
- `Write`
- `Edit`
- `MultiEdit`
- `Bash`
- `Glob`
- `Grep`
- `apply_patch`
- `GhFile`
- `GhLs`
- `GhSearch`
- `WebFetch`
- web search when supported

### Providers

Current code supports multiple backends/gateways, including:

- Anthropic
- OpenAI
- Codex
- Kiro
- OpenCode Go

### Skills

`luma` can load Claude Code-style skills from:

- `.agents/skills/`
- `.claude/skills/`
- `~/.agents/skills/`
- `~/.claude/skills/`
- `~/.config/luma/skills/`

## Modes

`luma` has three user-facing modes. The exact default model can vary by your local model catalog and available providers, so the important distinction is workflow intent:

| Mode | Use case |
|------|----------|
| `Rush` | Quick fixes, small edits, simple questions |
| `Smart` | General coding work, debugging, code review |
| `Deep` | More involved analysis, research, multi-step changes |

Mode also influences the tool style used by the agent.

## Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Tab` | Cycle mode (`Rush -> Smart -> Deep`) |
| `Enter` | Send message |
| `Alt+Enter` | Insert newline |
| `Paste` | Insert text inline or as a block attachment for long pastes |
| `Ctrl+T` | Cycle thinking level |
| `Esc` | Interrupt streaming; press twice to force |
| `Ctrl+C` | Clear input, or quit when input is empty |
| `Up` / `Down` | Navigate history or picker/dropdown |

## Authentication

`luma` stores its own auth state in `~/.config/luma/auth.json` and can import/reuse credentials from upstream tools when present.

Supported login flows include:

- OAuth-based login for supported providers
- API key entry for API-key providers
- account health tracking and cooldown-aware selection

Commands:

```bash
luma login
luma auth
luma accounts
```

## Data and config

`luma` stores data under `~/.config/luma/`, including:

- preferences
- sessions
- auth data
- user skills

Debug logging:

```bash
LUMA_DEBUG=1 luma
```

Logs are written to your temp directory as `luma.log`.

## Maintenance commands

```bash
# update installed binary
luma update

# refresh model catalog
luma sync

# inspect agent behavior on saved sessions
luma audit sessions
luma audit incidents
luma audit packets
luma audit clusters
```

## Build from source

```bash
cargo build
cargo run
```

## License

MIT — see [LICENSE](LICENSE).
