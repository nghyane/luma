# RFC 0002: Provider Architecture — Gateway / Protocol / Quirks

| Field            | Value                                                      |
| ---------------- | ---------------------------------------------------------- |
| RFC              | 0002                                                       |
| Title            | Provider Architecture — Gateway / Protocol / Quirks        |
| Status           | Accepted                                                   |
| Author(s)        | Nghia / Luma                                               |
| Created          | 2026-04-13                                                 |
| Updated          | 2026-04-13 (amended: pull-based streaming)                 |
| Tracking issue   | N/A                                                        |
| Supersedes       | N/A                                                        |
| Superseded by    | N/A                                                        |

## Summary

Tách kiến trúc provider thành ba trục độc lập — **Gateway**
(transport), **Protocol** (wire format), **Quirks** (vendor-specific
middleware). Đơn vị đăng ký chuyển từ `Model` sang `ModelBinding =
(gateway, protocol, model_id, quirks, thinking)`. `build_provider`
thành registry lookup, không còn match `source: String`. Mục tiêu:
thêm gateway mới (OpenCode Go/Zen, OpenRouter, Bedrock, Moonshot…) =
thêm rows trong catalog, không sửa code provider.

RFC này được triển khai qua hai PR: **PR1** refactor không đổi
behavior, **PR2** add OpenCode Go như binding-only change.

## Motivation

### Vấn đề hiện tại

- `ClaudeProvider` (`src/provider/claude.rs`, 1218 dòng),
  `CodexProvider` (788 dòng), `OpenAIProvider` (384 dòng) mỗi cái
  gộp 3 concern: wire encoding, HTTP transport, vendor quirks.
- `BASE_URL` hardcoded `const` trong mỗi provider; không có
  `with_base_url`.
- `src/core/agent/turn.rs::build_provider` match
  `AgentConfig.source: String` với 3 nhánh cứng
  (`"anthropic" | "codex" | _`).
- `AuthProvider` enum (`src/config/auth/mod.rs`) chỉ có
  `Anthropic | OpenAI`, gắn cứng với vendor.
- `src/config/models.catalog.json` assume `source: String` → 1
  provider impl, không biểu diễn được gateway đa protocol.

### Case study: OpenCode Go

OpenCode Go (https://opencode.ai/docs/go/) là gateway expose:

- `/zen/go/v1/chat/completions` — OpenAI Chat protocol, phục vụ GLM,
  Kimi, MiMo.
- `/zen/go/v1/messages` — Anthropic Messages protocol, phục vụ
  MiniMax M2.5 / M2.7.

Một gateway, hai protocol, không Claude Code quirks. Không map được
vào mô hình hiện tại nếu không tạo 2 source giả
(`opencode-go-openai`, `opencode-go-anthropic`) — leak abstraction.

### Observation tổng quát

Cùng `model_id` xuất hiện ở nhiều gateway với protocol khác nhau:

| model_id          | gateway     | protocol          | quirks                       |
| ----------------- | ----------- | ----------------- | ---------------------------- |
| `claude-sonnet-*` | anthropic   | anthropic         | claude-code, cache, adaptive |
| `claude-sonnet-*` | bedrock     | anthropic         | (none; AWS SigV4 auth)       |
| `claude-sonnet-*` | openrouter  | openai-chat       | (none)                       |
| `kimi-k2.5`       | opencode-go | openai-chat       | (none)                       |
| `kimi-k2.5`       | moonshot    | anthropic         | (none)                       |
| `minimax-m2.7`    | opencode-go | anthropic         | (none)                       |
| `gpt-5.4`         | openai      | openai-responses  | (none)                       |
| `gpt-5.4`         | codex       | openai-responses  | codex-session                |

→ Protocol là thuộc tính của cặp `(gateway, model)`, không phải của
`model`. Đây là lý do đơn vị đăng ký MUST là binding.

### Vì sao workaround nhỏ không đủ

Thêm `with_base_url` + 1–2 source mới giải được OpenCode Go trước
mắt nhưng:

- Lần thêm gateway kế tiếp lặp lại cùng câu hỏi.
- Claude quirks vẫn dính chặt vào `ClaudeProvider` → không dùng được
  cho Bedrock Claude.
- Debt tích lũy; dự án đang beta, chi phí refactor chỉ tăng theo
  thời gian.

## Guide-level explanation

Sau RFC này, thêm một model mới từ gateway có sẵn = thêm một dòng
JSON. Thêm gateway mới = một dòng JSON nữa + một credential flow
(nếu auth scheme đã hỗ trợ).

Ví dụ catalog sau khi ship PR2:

```jsonc
{
  "gateways": [
    { "id": "anthropic",   "base_url": "https://api.anthropic.com",
      "auth": "oauth_or_api_key" },
    { "id": "opencode-go", "base_url": "https://opencode.ai/zen/go",
      "auth": "api_key" }
  ],
  "bindings": [
    { "display_id": "anthropic/claude-sonnet-4-6",
      "gateway": "anthropic", "model_id": "claude-sonnet-4-6",
      "protocol": "anthropic",
      "quirks": ["anthropic_betas","claude_user_agent",
                 "oauth_system_rewrite","cache_breakpoint",
                 "adaptive_thinking","claude_fingerprint"],
      "thinking": "adaptive", "priority": 100 },
    { "display_id": "opencode-go/kimi-k2.5",
      "gateway": "opencode-go", "model_id": "kimi-k2.5",
      "protocol": "openai-chat", "quirks": [],
      "thinking": "off_only", "priority": 50 },
    { "display_id": "opencode-go/minimax-m2.7",
      "gateway": "opencode-go", "model_id": "minimax-m2.7",
      "protocol": "anthropic", "quirks": [],
      "thinking": "off_only", "priority": 50 }
  ]
}
```

Runtime lookup: user gõ `opencode-go/kimi-k2.5` → registry trả
`ModelBinding` → `ProviderRuntime` lắp ráp từ `(Gateway,
Protocol, QuirkSet)` → stream như `Provider` hiện nay.

## Reference-level explanation

### Streaming model: pull-based

Industry standard cho LLM streaming là async stream of events
(OpenAI/Anthropic SDKs, Vercel AI SDK). Kiến trúc hiện tại của Luma
dùng push model (provider tự gọi `EventSender::send` inline khi
parse SSE), trộn 3 concern trong một provider: decode wire format,
assemble message state, emit UI events.

RFC này chuyển sang **pull model** ngay trong PR1:

- `Protocol::decode_stream` là hàm pure, nhận byte stream, trả
  `Stream<Item = Result<StreamEvent>>`. Zero I/O, testable bằng
  fixture bytes.
- `Provider::stream` trả `BoxStream<'a, Result<StreamEvent>>`. Caller
  consume.
- `MessageAssembler` (helper, protocol-agnostic) tiêu thụ event
  stream, dựng `Message` + `Usage` + `StopReason`.
- `turn.rs` điều phối: consume stream, feed assembler, emit UI
  events qua `EventSender`. UI concern KHÔNG leak vào provider.

Lợi ích:

- Backpressure tự nhiên qua poll.
- Cancel = drop stream, không race với sender.
- Retry/logging/response quirks là stream adapter composable.
- Decode test bằng bytes tĩnh, không cần mock channel.
- Loại bỏ duplicate state machine (blocks/pending) trong mỗi provider.

### `StreamEvent`

```rust
pub enum StreamEvent {
    // Reasoning / chain-of-thought.
    ThinkingDelta { index: u32, text: String },
    ThinkingSignature { index: u32, sig: String },

    // Assistant text.
    TextDelta { index: u32, text: String },

    // Tool calls requested by model.
    ToolUseStart { index: u32, id: String, name: String },
    ToolUseDelta { index: u32, json_delta: String },
    ToolUseStop  { index: u32 },

    // Server-side tool invocations (e.g. Claude web_search).
    ServerToolCall   { name: String, input: serde_json::Value },
    ServerToolResult { name: String, output: serde_json::Value },

    // Bookkeeping.
    UsageUpdate(Usage),
    Done { stop: StopReason },
}
```

- `index` = ordinal content block position, cho phép assembler khôi
  phục interleaving (quan trọng cho Claude thinking signature).
- Protocol-specific quirks KHÔNG được thêm variant mới ở đây; nếu
  cần, encode vào `ServerToolCall` hoặc mở rộng enum qua RFC mới.
- `Done` MUST là event cuối; stream MUST terminate sau đó.

### Trait `Protocol`

```rust
pub trait Protocol: Send + Sync {
    fn id(&self) -> ProtocolId;
    fn endpoint_path(&self) -> &str;

    /// Pure: build request body + protocol-specific headers.
    fn encode_request(&self, req: &StreamRequest, ctx: &RequestCtx)
        -> (serde_json::Value, http::HeaderMap);

    /// Pure: decode raw byte stream into normalized events.
    /// MUST NOT perform I/O; MUST NOT touch EventSender.
    fn decode_stream(
        &self,
        bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    ) -> BoxStream<'static, anyhow::Result<StreamEvent>>;
}
```

- MUST có ba impl ban đầu: `AnthropicMessages`, `OpenAIChat`,
  `OpenAIResponses`.
- MUST NOT import vendor-specific constants (Claude betas, Codex
  session shape). Các quirks đó thuộc về middleware.

### Trait `Provider` (pull model)

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn thinking_capabilities(&self) -> ThinkingCapabilities;
    fn set_thinking(&mut self, level: ThinkingLevel);
    fn server_tool_schemas(&self, caps: &[String]) -> Vec<serde_json::Value>;
    fn supports_max_tokens_override(&self) -> bool { true }

    /// Open a stream of normalized events. Caller drives consumption.
    fn stream<'a>(&'a self, req: StreamRequest<'a>)
        -> BoxStream<'a, anyhow::Result<StreamEvent>>;
}
```

`StreamRequest` MUST bỏ field `tx` và `cancel` — consumer
(`turn.rs`) giữ chúng. Cancel = drop stream future.

### `MessageAssembler`

Helper protocol-agnostic:

```rust
pub struct MessageAssembler { /* blocks, pending, usage, stop */ }

impl MessageAssembler {
    pub fn new() -> Self;
    pub fn feed(&mut self, event: &StreamEvent);
    pub fn finish(self) -> StreamResponse;  // message + usage + stop
}
```

Test assembler bằng event sequences tĩnh, không cần mock protocol.

### Caller loop (`turn.rs`)

```rust
let mut stream = provider.stream(req);
let mut asm = MessageAssembler::new();
while let Some(evt) = tokio::select! {
    e = stream.next() => e,
    _ = cancel.cancelled() => None,  // drop stream => cancel
} {
    let evt = evt?;
    asm.feed(&evt);
    emit_ui(&tx, &evt);
    if matches!(evt, StreamEvent::Done { .. }) { break; }
}
let response = asm.finish();
```

### `Gateway`

```rust
pub struct Gateway {
    pub id: GatewayId,
    pub base_url: String,
    pub auth: AuthScheme,
    pub default_headers: HeaderMap,
}

pub enum AuthScheme {
    ApiKey { header: &'static str, prefix: Option<&'static str> },
    OAuthBearer,
    CodexSession,
    AwsSigV4 { region: &'static str, service: &'static str },
}
```

- Gateway MUST NOT chứa logic protocol.
- `default_headers` SHOULD dùng cho headers ổn định (UA mặc định,
  `accept`), KHÔNG cho headers phụ thuộc request (auth token,
  session id).

### Quirks

```rust
bitflags! {
    pub struct QuirkSet: u32 {
        const ANTHROPIC_BETAS       = 1 << 0;
        const CLAUDE_USER_AGENT     = 1 << 1;
        const OAUTH_SYSTEM_REWRITE  = 1 << 2;
        const CACHE_BREAKPOINT      = 1 << 3;
        const ADAPTIVE_THINKING     = 1 << 4;
        const CODEX_SESSION         = 1 << 5;
        const CLAUDE_FINGERPRINT    = 1 << 6;
    }
}

pub trait Quirk {
    fn apply_request(&self, body: &mut serde_json::Value,
                     headers: &mut HeaderMap, ctx: &RequestCtx);
    fn apply_response(&self, _event: &mut StreamEvent) {}
}
```

- Quirks MUST tách nhỏ (một flag = một concern). Lý do: Bedrock
  Claude dùng Anthropic protocol nhưng không có betas/OAuth; Claude
  Code cần toàn bộ; mix-and-match đòi granularity thấp.
- Thứ tự apply MUST xác định bởi iteration order cố định của
  `QuirkSet::iter()` (theo bit position).
- Một quirk MUST NOT giả định quirk khác đang active.

### `ProviderRuntime`

```rust
pub struct ProviderRuntime {
    gateway:    Arc<Gateway>,
    protocol:   Arc<dyn Protocol>,
    quirks:     QuirkSet,
    model_id:   String,
    thinking:   ThinkingCaps,
    credential: Credential,
    // process-level state: fingerprint seed, session id, ...
    state:      Arc<ProviderState>,
}

impl Provider for ProviderRuntime { /* stream() như §2.5 RFC cũ */ }
```

`ProviderRuntime` MUST là impl `Provider` duy nhất sau khi PR1 ship.

### `ModelBinding` và registry

```rust
pub struct ModelBinding {
    pub display_id: String,     // MUST khớp format "{gateway}/{model_id}"
    pub gateway:    GatewayId,
    pub model_id:   String,
    pub protocol:   ProtocolId,
    pub quirks:     QuirkSet,
    pub thinking:   ThinkingCaps,
    pub priority:   i32,
}

pub struct Registry {
    gateways:  HashMap<GatewayId, Arc<Gateway>>,
    protocols: HashMap<ProtocolId, Arc<dyn Protocol>>,
    bindings:  Vec<ModelBinding>,
}

impl Registry {
    pub fn resolve(&self, query: &str,
                   creds: &CredentialStore) -> Option<&ModelBinding>;
    pub fn build(&self, binding: &ModelBinding,
                 cred: &Credential) -> ProviderRuntime;
}
```

Resolver rules (MUST):

1. Nếu `query` có dạng `{gateway}/{model_id}` và khớp `display_id` →
   trả binding đó.
2. Ngược lại, lọc bindings có `model_id == query` hoặc
   `display_id.contains(query)`.
3. Giữ lại bindings có credential khả dụng.
4. Sắp xếp giảm dần theo `priority`, tie-break lexicographic theo
   `display_id`.
5. Trả binding đầu; `None` nếu rỗng.

### Auth

```rust
pub enum AuthKind {
    ApiKey,
    OAuthBearer { refresh_token: String, expires_at: u64 },
    CodexSession { account_id: String, session_id: String },
}

pub struct Credential {
    pub gateway: GatewayId,
    pub label:   String,
    pub kind:    AuthKind,
    pub token:   String,
}
```

- Credential MUST gắn với `gateway`, không gắn với vendor.
- Multi-credential per gateway MUST được hỗ trợ (giữ `label` hiện
  tại). Active credential chọn theo `label` trong config.

### Catalog

Single file `src/config/models.catalog.json` với schema:

```jsonc
{
  "gateways": [ /* GatewayDef */ ],
  "bindings": [ /* ModelBinding */ ]
}
```

- `GatewayDef.auth` MUST map 1-1 với một `AuthScheme` variant.
- `ModelBinding.quirks` MUST là subset các flag được code support.
  Unknown flag → error khi load, không silent ignore.

### File layout

The original plan:

```text
src/provider/
  mod.rs                   // trait Provider (giữ), ProviderError
  runtime.rs               // ProviderRuntime
  protocol/
    mod.rs                 // trait Protocol, ProtocolId
    anthropic.rs
    openai_chat.rs
    openai_responses.rs
  gateway/
    mod.rs                 // Gateway, AuthScheme, GatewayId
    defs.rs                // builtin definitions
  quirks/
    mod.rs                 // QuirkSet, apply_request/apply_response
    anthropic_betas.rs
    claude_user_agent.rs
    oauth_system_rewrite.rs
    cache_breakpoint.rs
    adaptive_thinking.rs
    codex_session.rs
    claude_fingerprint.rs
  sse.rs, json_stream.rs, retry.rs  // giữ

src/config/
  registry.rs              // Registry, loader, resolver
  models.catalog.json
  auth/mod.rs              // AuthKind refactor
```

As-built (see §Implementation status for why it diverged):

```text
src/provider/
  mod.rs
  binding.rs               // resolve, build_provider, ModelBinding, ProtocolId
  gateway.rs               // trait Gateway, GatewayId
  gateways/
    mod.rs                 // GATEWAYS &[&dyn Gateway] + lookup()
    anthropic.rs
    codex.rs
    openai.rs
    opencode_go.rs
  protocol/
    mod.rs
    anthropic.rs           // AnthropicRuntime + decode_anthropic_sse
    openai_chat.rs         // OpenAIChatRuntime  + decode_chat_sse
    openai_responses.rs    // OpenAIResponsesRuntime + decode_responses_sse
  quirks/
    mod.rs                 // QuirkSet bitflags
    adaptive_thinking.rs
    cache_breakpoint.rs
    claude_identity.rs     // user-agent + session-id + fingerprint (merged)
    oauth_system_rewrite.rs
  sse.rs, json_stream.rs, retry.rs   // retry.rs also hosts typed errors

src/config/
  auth/mod.rs              // Credential, AuthVendor, upsert_api_key, …
  auth/pkce.rs
  auth/policy.rs           // build_refresh_request only
  models.catalog.json      // builtin model metadata (no gateway/quirk data)

src/cli_login.rs           // `luma login` arrow-key picker + api-key paste
```

Key differences from the original plan:

- No `runtime.rs`, no `ProviderRuntime` enum. Dropped as scaffolding
  (commit `277220d`).
- No separate `gateway/defs.rs`; gateways are one file each under
  `gateways/`.
- No `config/registry.rs` / JSON catalog loader. Each gateway owns
  its catalog (e.g. OpenCode Go's per-model protocol table lives in
  `gateways/opencode_go.rs`).
- `AuthKind` enum introduced in commit 10 and subsequently removed
  (`dc5f26f`): wire header shape is a **protocol endpoint** concern,
  not a credential one, so it lives at the runtime.
- `claude_user_agent.rs` + `claude_fingerprint.rs` merged into
  `claude_identity.rs` (single identity surface).
- Quirks for `anthropic_betas` and `codex_session` are not standalone
  files; they're flag constants in `QuirkSet` consumed inline by the
  protocol runtime.

### Migration plan

Chiến lược: **big-bang**. Khi cần đối chiếu behavior cũ → git
history (`git show <sha>:src/provider/claude.rs`). Xóa 3 file
legacy trong cùng PR với runtime mới.

Commit order trong PR1 (mỗi commit MUST pass
`cargo fmt && cargo clippy -- -D warnings && cargo test`):

1. `refactor(provider): define StreamEvent and pull-based Protocol trait`
2. `refactor(provider): extract build_request_body helpers`
3. `refactor(provider): extract consume_chat_stream (openai)`
4. `refactor(provider): extract Gateway and AuthScheme`
5. `refactor(provider/quirks): extract claude_user_agent + fingerprint`
6. `refactor(provider/quirks): extract oauth_system_rewrite + mcp_noop`
7. `refactor(provider/quirks): extract cache_breakpoint`
8. `refactor(provider/quirks): extract adaptive_thinking`
9. `refactor(provider/quirks): extract codex_session`
10. `refactor(provider): impl Protocol + MessageAssembler, migrate pull,
    add ProviderRuntime + registry, delete legacy modules`
    (large atomic commit — the streaming-architecture cutover)
11. `refactor(auth): replace AuthProvider enum with AuthKind`

### Test plan

Test hiện có trong `claude.rs` MUST được di chuyển nguyên văn sang
module quirks tương ứng, không nới lỏng assertion:

| Test hiện tại                 | Đích                                   |
| ----------------------------- | -------------------------------------- |
| `billing_block_*`             | `quirks/oauth_system_rewrite.rs`       |
| `beta_list_*`                 | `quirks/anthropic_betas.rs`            |
| `cache_breakpoint_*`          | `quirks/cache_breakpoint.rs`           |
| `thinking_config_*`,
  `adaptive_thinking_*`         | `quirks/adaptive_thinking.rs`          |
| `fingerprint_*`               | `quirks/claude_fingerprint.rs`         |
| `user_agent_*`, `session_id_*`| `quirks/claude_user_agent.rs`          |
| `parse_stop_reason_*`         | `protocol/anthropic.rs`                |
| `strips_thinking_blocks_*`    | `protocol/anthropic.rs`                |

PR1 SHOULD bổ sung snapshot test byte-equal giữa request body do
runtime mới sinh ra và body của impl cũ cho 3 gateway hiện tại.
Snapshot được capture trước khi xóa code cũ.

### Success criteria

PR1 coi là xong khi:

- Toàn bộ test xanh, kể cả test di chuyển.
- `cargo clippy -- -D warnings` sạch.
- 3 module legacy đã xóa; `ProviderRuntime` là path duy nhất.
- Snapshot request-body khớp 100% với output trước refactor.
- Smoke test thủ công: một turn mỗi gateway (Claude OAuth, Codex,
  OpenAI direct) không regress.

PR2 (OpenCode Go) coi là xong khi:

- Thêm gateway entry + bindings; không sửa file nào trong
  `src/provider/*.rs`.
- `/connect opencode-go` paste API key hoạt động.
- Smoke test: `opencode-go/kimi-k2.5` (OpenAI Chat) và
  `opencode-go/minimax-m2.7` (Anthropic) đều hoàn thành 1 turn.

## Drawbacks

- Catalog phình: một model phổ biến có thể xuất hiện 4–5 lần. Dễ
  đọc nhưng khó sync khi vendor đổi tên.
- UX thêm khái niệm `display_id` vs `model_id`.
- Refactor đụng ~3500 dòng provider + auth + UI `/connect`.
- Migration không có cầu nối: nếu regress, rollback = revert commit,
  không có feature flag.
- Test quirks phải viết lại setup (dù assertion giữ nguyên) → có
  rủi ro typo tạo false-green.

## Rationale and alternatives

### Tại sao ba trục, không phải hai

Hai trục (Provider + Protocol) gộp quirks vào Provider, lặp lại
tình trạng hiện tại cho gateway tương lai. Quirks tách riêng cho
phép Bedrock = Anthropic protocol + zero quirks, OpenRouter =
OpenAI Chat + zero quirks, mà không copy code.

### Tại sao binding, không phải model

Cùng `model_id` có mặt ở nhiều gateway với protocol khác nhau (xem
bảng §Motivation). Nếu đơn vị đăng ký là `Model`, protocol phải
suy luận runtime từ gateway + heuristic → hacky. Binding làm mọi
thứ tường minh, data-driven.

### Alternative A: Map protocol theo tên model (trong provider code)

```rust
fn protocol_for(model_id: &str) -> &dyn Protocol {
    if model_id.starts_with("minimax") { ... } else { ... }
}
```

Loại. Heuristic tên model là fragile, không grep-able, vỡ khi
vendor đổi tên.

### Alternative B: Provider tự quản protocol qua lookup table

Đẩy cùng data vào code thay vì catalog. Mất tính grep-able và tách
concern. Chỉ hợp lý khi có logic runtime thật (fallback v1→v2 theo
response header). Không có nhu cầu đó hiện nay.

### Alternative C: Giữ nguyên, chỉ thêm `with_base_url` + source mới

Ship nhanh hơn (1–2 ngày) nhưng tích thêm debt. Lần thêm gateway
kế tiếp (OpenRouter, Bedrock) sẽ đắt hơn. Dự án đang beta → ưu
tiên nền tảng.

### Impact của không làm gì

Mỗi gateway mới = copy một provider file, gộp thêm quirks. Chu kỳ
này đã lặp 3 lần (claude, codex, openai). Lần thứ 4 (OpenCode Go)
là dấu hiệu kiến trúc sai.

## Prior art

- **LiteLLM** (Python): gateway normalizer, unified schema; gộp
  protocol vào gateway, quirks ẩn trong adapter. Inspiration nhưng
  không áp dụng vì Rust ưu tiên explicit hơn.
- **Vercel AI SDK** (`@ai-sdk/*`): package per provider
  (`@ai-sdk/openai`, `@ai-sdk/anthropic`) + concept "openai-compatible"
  provider. OpenCode Go docs trực tiếp tham chiếu naming này.
- **rust-lang/rfcs**: format template. RFC này theo convention đó
  rút gọn.
- **LangChain ChatModel hierarchy**: gộp quá nhiều concern vào class
  con — bài học ngược, nên tránh.

## Unresolved questions

1. **Claude fingerprint + session_id storage**: hiện là
   process-global `OnceLock` trong `claude.rs`. Đề xuất mặc định:
   chuyển vào `ProviderState` gắn với `ProviderRuntime`, quirks
   đọc/ghi qua `RequestCtx`.
2. **Codex session header lifecycle**: cần `account_id` từ
   credential và `session_id` per-turn. Đề xuất: `RequestCtx`
   expose cả hai; quirk `codex_session` tổng hợp header.
3. **`anthropic-version` header**: thuộc Protocol hay Gateway?
   Đề xuất: Protocol, vì phụ thuộc wire format.
4. **Multi-credential selection UX**: khi gateway có nhiều
   credential, chọn cái nào? Đề xuất: giữ behavior hiện tại (label
   active trong config) cho PR1; RFC riêng nếu cần nâng.
5. **Schema versioning cho catalog**: có cần `"version": 1` ở
   top-level không? Đề xuất: có, để migration tương lai an toàn.

## Future possibilities

- **Gateway mới**: OpenCode Zen (Responses API), OpenRouter, Groq,
  Moonshot, Bedrock — mỗi cái là catalog entry + optional
  `AuthScheme` variant.
- **Quirks plugin từ config**: chưa hỗ trợ, có thể thêm nếu cần
  user-defined middleware.
- **Per-binding retry policy**: hiện global; có thể gắn vào
  `ModelBinding`.
- **A/B fallback chain**: binding trỏ tới binding khác khi lỗi
  auth/quota.
- **Usage tracking per gateway**: OpenCode Go có quota tuần/tháng,
  cần surface cho user.

## Implementation status

PR1 complete. PR2a (OpenCode Go infrastructure) shipped. PR2b (OpenCode
Go user-facing: api-key auth, `/connect` UI, catalog entries) is the
only remaining work and is gated on smoke-testing with a real OpenCode
Go API key.

### Commits shipped

Quirks phase (session 1):

- `51b3471` docs(rfcs): standardize format, add provider-architecture RFC
- `5ea2e6c` refactor(provider): define StreamEvent and pull-based Protocol trait
- `5f52a6b` refactor(provider): extract build_request_body helpers
- `3c20817` refactor(provider): extract consume_chat_stream (openai)
- `7458be0` refactor(provider/quirks): extract cache_breakpoint
- `c0ef8a2` refactor(provider/quirks): extract claude_identity
- `b9c2892` refactor(provider/quirks): extract oauth_system_rewrite
- `f8c7e50` refactor(provider/quirks): extract adaptive_thinking

Structural cutover (session 2):

- `f797424` refactor(provider): introduce BindingRegistry scaffolding (9a)
- `dce29f5` refactor(provider): structural cutover — ProviderRuntime +
  protocol modules (9). Legacy `claude.rs`/`codex.rs`/`openai.rs` deleted;
  moved verbatim into `protocol/{anthropic,openai_chat,openai_responses}.rs`.
- `b75cf02` refactor(auth): rename AuthProvider → AuthVendor, introduce
  AuthKind (10). `AuthVendor` names the pool bucket; `AuthKind` describes
  the wire-level auth scheme.
- `5705b62` fix(auth): classify AuthKind by vendor, not `account_id`.
  Claude OAuth entries carry `profile.account_uuid` in `account_id`,
  so the initial `(is_oauth, account_id.is_some())` split misrouted
  Claude OAuth to `CodexSession` and dropped the Claude Code headers
  — 401 on every turn. `Credential` now carries `vendor`.

Cleanup of session-2 scaffolding (still session 2):

- `277220d` refactor(provider): drop ProviderRuntime enum +
  BindingRegistry scaffold. Replaced 6 match blocks of dispatch
  forwarding with `Box<dyn Provider>` returned directly from three
  flat free functions in `binding.rs`. -163 LOC.
- `13dae16` refactor(core/provider): drop dead Protocol trait
  scaffolding. The `StreamEvent` / `ProtocolId` / `Protocol` trait
  added in `5ea2e6c` carried `#[allow(dead_code)]` and no call sites;
  deleted to be re-added at the shape pull migration actually needs.
- `815d3c6` docs(provider): strip stale RFC narrative from module
  comments. Comments now describe current shape, not in-flight plan.

Pull migration (session 2):

- `044ade8` refactor(provider/anthropic): pull-based SSE decoder.
  `AnthropicDecoder` is pure (no I/O, no `tx.send`); exposes
  `BoxStream<Result<StreamEvent>>` via `futures::stream::unfold`.
  Consumer in `Provider::stream` translates `StreamEvent` → UI
  `Event::*` and assembles `StreamResponse`. Re-introduced
  `StreamEvent` in `core::provider` with the minimal nine-variant
  shape Anthropic needs. +4 decoder unit tests.
- `9261e2e` refactor(provider): pull migration for OpenAI Chat +
  Responses. Same pattern as Anthropic: `ChatDecoder` /
  `ResponsesDecoder` + `consume_*_stream` consumer. Codex's
  structured failure / incomplete-reason handling preserved via
  `decoder.finalize() -> Result`.

Quirks composition (session 2):

- `15f0178` refactor(provider/quirks): introduce QuirkSet bitflags.
  Hand-rolled `u32` wrapper (no bitflags crate); five flags
  (CACHE_BREAKPOINT, ADAPTIVE_THINKING, OAUTH_SYSTEM_REWRITE,
  ANTHROPIC_BETAS, CLAUDE_IDENTITY). `binding::quirks_for(gateway,
  auth_kind)` is the single policy site; `AnthropicRuntime` gates
  each quirk on `self.quirks.contains(FLAG)` rather than branching
  on `auth_kind`. Wire behaviour preserved.

PR2a infrastructure (session 2):

- `7ab50d5` refactor(provider): ProtocolId + per-binding base_url.
  Decouple protocol from gateway. New `ProtocolId` enum
  (AnthropicMessages / OpenAIChat / OpenAIResponses); `ModelBinding`
  gains `protocol` + `base_url`. `build_provider` dispatches by
  `binding.protocol`, not `binding.gateway`. `AnthropicRuntime` and
  `OpenAIChatRuntime` accept `base_url` at construction. Hardcoded
  `BASE_URL` consts removed. Codex Responses keeps its hardcoded
  endpoint (chatgpt.com session headers are not transferable).

PR2b OpenCode Go shipping (session 2):

- `e0ee004` feat(auth): OpenCode Go support + arrow-key login picker.
  New `AuthVendor::OpenCodeGo`, `GatewayId::OpenCodeGo`, seven
  builtin model bindings (glm-5/5.1, kimi-k2.5, mimo-v2-pro/omni,
  minimax-m2.5/2.7). `resolve(opencode-go, model_id)` consults a
  per-model table because the proxy serves both protocols on
  distinct endpoint paths. `auth::upsert_api_key` stores paste-key
  credentials with `is_oauth=false`, no refresh, no expiry. New
  `cli_login` module drives an arrow-key provider picker (termina
  raw mode) that dispatches to the PKCE flow for OAuth vendors and
  to an inline paste prompt for api-key vendors. `/login` and
  related TUI command surface removed; one login entry point.
- `4a60d7a` fix: picker render uses `\r\n` (raw mode has no
  LF→CRLF translation).
- `484e583` fix: opencode-go models missing from picker after
  `luma sync` (catalog JSON didn't include them, sync didn't scan
  them — proxy has no list-models endpoint); picker also didn't
  clear prior terminal content. Adds clear-screen on entry and
  includes the seven builtin OpenCode Go rows when sync falls back
  to the embedded catalog.
- `314e3f5` → `dc5f26f` → `5f48cad` a sequence of root-cause fixes
  for "exhausted auth retries" on OpenCode Go. See §Root causes
  below.

Architecture cleanup forced by PR2b surface (session 2):

- `17a97bf` refactor(binding): collapse five scattered
  match-by-GatewayId helpers (auth_vendor, default_protocol,
  base_url, auth_kind_for, quirks_for) into one static
  `GatewaySpec` table. Every per-gateway concern is one field on
  one row. First attempt at centralising.
- `1d155fb` refactor(provider): drop the spec table for a proper
  `Gateway` trait + one-file-per-provider under
  `provider/gateways/<name>.rs`. Each gateway owns its vendor,
  base_url, quirks, protocol resolution, thinking caps, and
  `build`. `binding.rs` is a 123-LOC dispatcher; adding a gateway
  is a new file + one row in `GATEWAYS` + one `GatewayId` variant.
  Registry-coverage test asserts every variant has a row.
- `948af1a` refactor(provider): typed `ProviderUnauthorized`.
  Replaces vendor-keyed keyword classifiers
  (`classify_auth_failure`, `AuthFailureKind`, `is_auth_error`)
  with a typed error raised by the HTTP layer on 401/403.
  `turn.rs` downcasts the typed error; no more grepping for
  "401" / "unauthorized" in message strings.
- `948af1a`/`dc5f26f` additionally drop `Credential::auth_kind`
  and `Gateway::auth_kind`. Wire header shape now lives at the
  protocol runtime (`AnthropicRuntime` derives it from an `is_oauth`
  bool). Root cause: header shape depends on **protocol endpoint**,
  not gateway or credential — OpenCode Go's `/v1/chat/completions`
  wants `Authorization: Bearer` while `/v1/messages` on the same
  host wants `x-api-key`. Any enum tied to gateway-or-credential
  alone could not express that.

Fixes forced by live smoke-test of OpenCode Go:

- `314e3f5` First misrouting: `Credential::auth_kind` derived
  `CodexSession` for OpenCode Go api keys because the
  `(is_oauth, account_id)` split shared with Codex misfired.
- `dc5f26f` Second misrouting: after dropping `auth_kind`, discovered
  via curl that OpenCode Go's two endpoints want different auth
  headers. Forced header shape per protocol in `OpenCodeGo::build`.
- `d8bd787` URL mismatch: `OpenAIChatRuntime` built
  `{base_url}/chat/completions` while `AnthropicRuntime` built
  `{base_url}/v1/messages`. OpenAI gateway was hiding the
  inconsistency by hard-coding `/v1` into its base_url; OpenCode
  Go didn't, so its OpenAI Chat path 404'd. Normalised: base_url
  is scheme+host only, runtime owns the `/v1/<endpoint>` path.
- `5f48cad` UX: 401 on an api key no longer tries to refresh or
  delete. Matches gh/aws/stripe CLIs — a single auth failure
  surfaces "`luma login` to replace it"; the pool is untouched.

### State after commit `5f48cad`

- 578 tests pass; `cargo clippy -- -D warnings` clean.
- Every per-gateway concern lives in `src/provider/gateways/<name>.rs`.
  Adding a gateway: new file + one row in `gateways::GATEWAYS` + one
  `GatewayId` variant. Compiler enforces exhaustiveness; a registry
  test asserts the mapping is total.
- `binding.rs` (123 LOC) is a thin dispatcher. No per-gateway policy
  survives there.
- Pull-based decoders in all three protocol modules with synthetic
  SSE unit tests.
- `QuirkSet` decouples vendor quirks from wire protocol.
- Typed error surface (`ProviderRateLimited`, `ProviderUnauthorized`,
  `StreamInterrupted`) — no more keyword matching on error strings.
- 0 `#[allow(dead_code)]`, 0 TODO/FIXME, 0 stale RFC narrative.
- Smoke-tested live:
  - Claude OAuth turn (twice, after `5705b62` and `15f0178`).
  - OpenCode Go: direct curl verified both endpoints reachable with
    the auth headers the code now sends. End-to-end TUI turn from
    a model selector pending user confirmation.
  - Codex + OpenAI direct not yet verified against live traffic.

### Remaining work

Out of scope for this RFC; tracked here for continuity:

- **Model-selection UX for `opencode-go/<model>` ids.** `AgentConfig.source`
  still carries a single vendor string; users select an OpenCode Go
  model by gateway + id through the model picker. Unifying to a
  single `display_id` format is a cosmetic follow-up.
- **On-disk `AccountEntry` migration to a typed auth_kind field.**
  Today the pool file still keys by `(provider, is_oauth,
  account_id)`. Bump `POOL_STORE_VERSION` to 3 only if a future
  schema change forces it.
- **`AnthropicRuntime` fingerprint/session storage** (unresolved Q1).

- **`AnthropicRuntime` fingerprint/session storage** (unresolved Q1).
  Currently process-global `OnceLock` in `quirks/claude_identity`.
  Move into per-runtime state when there's a concrete need (e.g. two
  Anthropic-protocol bindings with different fingerprint policies in
  the same process).

### Future possibilities (deferred indefinitely)

- **Catalog JSON loader** (`models.catalog.json`). The data-driven
  dispatch RFC originally proposed. Not needed today: four gateways
  fit comfortably as one-file-per-provider impls under
  `provider/gateways/`, and no user has asked for custom catalogs.
  Revisit when a fifth gateway lands or a user explicitly needs to
  bring their own.
- **Inventory-based plugin registration.** Replaces the static
  `GATEWAYS` array with runtime discovery via the `inventory`
  crate. Evaluated and rejected (commit `1d155fb` discussion):
  regression in compile-time exhaustiveness outweighs the "add a
  file, not a line" ergonomics for a project with < 10 gateways.
