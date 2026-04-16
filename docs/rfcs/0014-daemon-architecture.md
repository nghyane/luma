# RFC 0014: Daemon Architecture — Unified Runtime with Relay

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0014                                         |
| Title            | Daemon Architecture — Unified Runtime with Relay |
| Status           | Draft                                        |
| Author(s)        | Nghia Hoang                                  |
| Created          | 2026-04-16                                   |
| Updated          | 2026-04-16                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Luma chuyển sang kiến trúc daemon: một process persistent duy nhất quản
lý tất cả agent sessions, expose qua WebSocket. TUI, web client, Discord
bot, và ACP adapter đều trở thành thin clients connect vào daemon. Relay
server (Cloudflare Durable Objects, E2E encrypted) cho phép phone/remote
access qua internet mà không cần mở port.

## Motivation

- **Session không sống sót khi thoát TUI.** Đóng terminal = mất agent
  đang chạy. Không thể bắt đầu task trên desktop rồi check từ phone.

- **3 runtime paths duplicate logic.** `luma` (TUI), `luma --acp`
  (Paseo/Zed), và daemon tương lai đều spawn agent loop, resolve auth,
  init provider riêng. Mỗi bug fix phải patch 3 chỗ.

- **ACP bó buộc.** Paseo spawn `luma --acp` mỗi agent → process mới →
  session mới. Không resume cross-session, không parallel sessions,
  không share auth pool. Paseo chỉ thấy Luma như generic ACP agent —
  mất hết đặc trưng (modes, multi-provider, evidence store).

- **Không có mobile access.** Phone không connect được localhost. Cần
  relay layer nhưng hiện tại không có entry point nào cho nó.

## Guide-level explanation

### Sau RFC này

```
$ luma
```

Luma check daemon đang chạy chưa. Nếu chưa, spawn background. TUI
connect WS tới daemon, hoạt động như cũ. Thoát TUI — agent vẫn chạy.
Mở lại — reconnect, thấy output đã chạy.

```
$ luma daemon
```

Chạy daemon foreground (cho server/debug). Bind `0.0.0.0:6800`, in QR
code để pair phone.

Phone scan QR → connect qua relay → E2E encrypted → full access: chat,
xem tool output, switch model, resume session.

Discord bot connect cùng daemon → `/luma refactor auth module` → agent
chạy, stream kết quả vào Discord thread.

### Daemon states

```
COLD (~1.5MB)                    HOT (~15-50MB)
  Socket listener only    ←───→   Agent loops + sessions
  0% CPU, epoll wait      idle    Provider connections
                          30min   Auth pool
```

Daemon always-on nhưng gần zero resource khi idle. First connection
trigger Cold→Hot (~200ms). 30 phút không client + không agent running →
Hot→Cold, flush sessions, drop memory.

### Auto-start

macOS: LaunchAgent, `RunAtLoad: true`, `KeepAlive: true`.
Linux: systemd user service, `Restart=always`.

User không cần biết daemon tồn tại.

## Reference-level explanation

### Process architecture

```
luma daemon
  ├── WS Server (tokio-tungstenite, :6800)
  │     ├── /ws           → agent WebSocket
  │     ├── /health       → health check
  │     └── /             → serve web client SPA
  │
  ├── Agent Manager
  │     ├── sessions: HashMap<String, ActiveSession>
  │     ├── create_session(config) → session_id
  │     ├── send_prompt(session_id, text)
  │     ├── cancel(session_id)
  │     ├── list_sessions() → Vec<SessionMeta>
  │     └── load_session(session_id)
  │
  ├── Auth Pool (shared across sessions)
  │
  ├── Relay Client (outbound WSS to relay server)
  │     ├── Control socket: receive client connect/disconnect
  │     └── Per-client data socket: E2E encrypted channel
  │
  └── State Manager
        ├── Cold/Hot transitions
        ├── Pidfile (~/.config/luma/daemon.pid)
        └── Session flush (lazy write-behind)
```

### WS Protocol

Client → Daemon:

```json
{"t":"prompt","sid":"optional-session-id","d":"refactor auth"}
{"t":"cancel","sid":"ses_abc"}
{"t":"list"}
{"t":"load","sid":"ses_abc"}
{"t":"set_model","d":"claude-opus-4-6"}
{"t":"set_mode","d":"deep"}
```

Daemon → Client:

```json
{"t":"session","sid":"ses_abc"}
{"t":"tok","d":"Hello"}
{"t":"thk","d":"Let me analyze..."}
{"t":"ts","n":"Read","s":"Cargo.toml"}
{"t":"to","d":"[package]\nname = \"luma\""}
{"t":"te","n":"Read","s":"read 45 lines"}
{"t":"done"}
{"t":"err","d":"provider timeout"}
{"t":"usage","i":1200,"o":350}
{"t":"sessions","d":[{"id":"ses_abc","title":"...","updated":"..."}]}
```

Compact keys (`t`, `d`, `sid`, `n`, `s`) — minimize bandwidth cho
mobile/relay. Mỗi message là 1 JSON line.

### Relay

Fork Paseo `packages/relay` — Cloudflare Durable Objects.

**Server (Cloudflare Worker):**
- 1 Durable Object per `serverId` (= 1 Luma daemon instance)
- WebSocket hibernation — zero cost khi idle
- Route encrypted bytes giữa daemon ↔ client
- Zero-knowledge: không đọc được content

**Crypto:**
- Key exchange: Curve25519 ECDH (`crypto_box` crate phía Rust,
  `tweetnacl` phía JS)
- Encryption: XSalsa20-Poly1305
- Pairing: QR code chứa `{relay, serverId, publicKey}`
- Per-session fresh keys

**Daemon relay client (Rust):**

```rust
// daemon/relay.rs
pub struct RelayClient {
    relay_url: String,
    server_id: String,
    keypair: crypto_box::SecretKey,
}
```

Daemon connect outbound → control socket. Khi client connect relay →
relay notify daemon → daemon open per-client data socket → E2E
handshake → encrypted channel → bridge to agent manager.

MUST: daemon connect outbound only — không cần mở port.
MUST: relay MUST NOT be able to read message content.
SHOULD: reconnect on disconnect with exponential backoff.

### TUI as client

```rust
// Trước (hiện tại):
let agent_tx = core::agent::spawn(config, registry, event_tx);

// Sau:
let ws = daemon::ensure_running().await;
let ws = connect(ws).await;
// TUI event loop: render từ WS events thay vì event_bus
```

TUI event handling giữ nguyên — chỉ đổi source. `Event` enum không
thay đổi; daemon serialize/deserialize cùng variants.

MUST: `luma` (không argument) MUST hoạt động giống hiện tại từ góc nhìn
user. Daemon start ngầm nếu cần.

SHOULD: TUI reconnect tự động nếu daemon restart.

### ACP adapter

Simplify thành WS↔stdio bridge:

```rust
// luma --acp
// stdin JSON-RPC → translate → WS ClientMsg → daemon
// daemon WS ServerMsg → translate → stdout JSON-RPC
```

Bỏ duplicate agent logic trong `acp/bridge.rs`. ACP adapter chỉ là
protocol translator.

### File layout

```
src/
  daemon/
    mod.rs              -- entry: `luma daemon`, Cold/Hot state machine
    ws_server.rs        -- WebSocket server, connection handling
    protocol.rs         -- ClientMsg/ServerMsg types
    agent_manager.rs    -- multi-session agent lifecycle
    relay.rs            -- outbound relay client + E2E crypto
    ensure.rs           -- auto-start daemon from TUI/ACP
  acp/
    bridge.rs           -- simplified: WS↔stdio bridge only
  core/                 -- unchanged
  tui/                  -- refactor: event_bus → WS client
```

### Dependencies

```toml
tokio-tungstenite = "0.24"   # WS server + client
crypto_box = "0.9"           # NaCl box (x25519 + xsalsa20poly1305)
qrcode = "0.14"              # QR code generation for pairing
```

### Clients (separate repos/dirs)

```
clients/
  web/          -- Svelte SPA, served by daemon at /
  discord/      -- Discord bot, connects daemon WS
```

### Boot sequence

```
luma daemon:
  1. Write pidfile
  2. Bind TCP :6800
  3. Enter Cold state (epoll wait only)
  4. On first connection → Cold→Hot
     a. Init tokio runtime pools
     b. Load auth pool
     c. Connect relay (if configured)
  5. Serve connections

luma (TUI):
  1. Check pidfile + health check
  2. If no daemon → spawn `luma daemon` background
  3. Wait ready (poll /health, max 3s)
  4. Connect WS
  5. Run TUI event loop against WS
```

### Idle management

- Client count tracked via `AtomicUsize`
- Agent running state tracked per session
- Reaper task checks every 60s
- Hot→Cold when: 0 clients AND 0 running agents AND idle > 30min
- Cold→Hot: ~200ms (load config + init pools)
- Sessions flushed to disk before Cold transition

## Drawbacks

- **Complexity.** Daemon + relay + multi-client adds moving parts.
  Current single-process TUI is simpler to debug.

- **Daemon crash = all sessions lost.** Mitigated by write-behind
  persistence, but in-flight turns are lost. TUI fallback mode
  (embedded, no daemon) could mitigate but adds yet another code path.

- **Port conflict.** `:6800` may clash. SHOULD support config and
  auto-increment.

- **Security surface.** WS server on network interface. MUST bind
  `127.0.0.1` by default. Relay access MUST require E2E pairing.

## Rationale and alternatives

### Alternative 1: Paseo as host

Đã thử. Paseo spawn `luma --acp` per agent — process isolation nhưng
mất session continuity, mất Luma-specific features, bó buộc vào ACP
protocol limitations. Paseo là generic multi-agent orchestrator; Luma
cần native runtime.

### Alternative 2: HTTP REST API thay vì WebSocket

REST không support streaming. Agent output là real-time stream (tokens,
tool progress). SSE là one-way. WebSocket là bidirectional streaming —
đúng primitive cho use case này.

### Alternative 3: gRPC

Overkill. Cần protobuf codegen, heavier dependencies. JSON over WS đủ
cho message rate (~100 msg/s peak). gRPC chỉ win ở >10k msg/s hoặc
strict schema enforcement — không phải bottleneck ở đây.

### Impact of doing nothing

Mobile access không có. Session mất khi thoát TUI. ACP adapter tiếp tục
duplicate logic. Mỗi feature mới phải implement 3 lần.

## Prior art

- **Paseo** (`getpaseo/paseo`): daemon + relay + multi-client. Relay
  design (Cloudflare DO, E2E NaCl box, QR pairing) là direct reference.
  Phân tích chi tiết trong conversation trước RFC này.

- **Docker daemon**: `dockerd` runs persistent, `docker` CLI connects
  via socket. User không cần biết daemon tồn tại. Auto-start pattern.

- **LSP (Language Server Protocol)**: editor spawns language server,
  communicates via JSON-RPC over stdio. ACP follows this pattern.
  Limitation: 1:1 process per session, no shared state.

- **Neovim remote UI**: Neovim runs headless, UI connects via
  MessagePack-RPC. TUI is just one possible UI. Same separation of
  concerns proposed here.

## Unresolved questions

1. **Web client framework?** Svelte (smallest bundle), React (ecosystem),
   or vanilla JS (zero deps)? Default: Svelte.

2. **Discord bot language?** Rust (same repo, shared types) or
   TypeScript (richer Discord SDK ecosystem)? Default: TypeScript with
   `discord.js`, connects daemon WS.

3. **Auth for WS server?** Local connections (127.0.0.1) trusted by
   default (Docker model). Remote via relay requires E2E pairing. Direct
   remote (0.0.0.0) needs token auth — defer to future RFC.

4. **Relay hosting?** Self-hosted vs managed. Default: Cloudflare
   Workers free tier. Self-hosted option via `wrangler dev` or
   standalone Node server.

## Future possibilities

- **Team mode.** Multiple users connect same daemon, shared sessions
  with role-based access.

- **Agent orchestration.** Daemon manages multiple agents that
  coordinate via shared context (handoff, review loops).

- **Plugin system.** Third-party tools register with daemon at runtime
  via WS, extending agent capabilities without rebuilding.

- **Persistent provider connections.** Keep LLM API connections warm
  across sessions. Reduce first-token latency to near-zero.

- **Web terminal.** Embed terminal emulator in web client, stream
  daemon's tool execution output with full ANSI rendering.

## Implementation status

Phase 1 — ACP adapter (completed):
- `e8b7b18` fix(acp): correct ACP protocol compliance for Paseo integration
- `d06ef61` feat: add ACP server mode (`luma --acp`)

Phase 2 — Daemon + WS server: pending this RFC.
Phase 3 — Web client: pending.
Phase 4 — Relay: pending.
Phase 5 — Discord bot: pending.
Phase 6 — TUI refactor to WS client: pending.
