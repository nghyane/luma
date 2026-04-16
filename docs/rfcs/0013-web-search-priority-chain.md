# RFC 0013: Web Search Priority Chain — Provider-Aware Search Routing

| Field            | Value                                                        |
| ---------------- | ------------------------------------------------------------ |
| RFC              | 0013                                                         |
| Title            | Web Search Priority Chain — Provider-Aware Search Routing    |
| Status           | Accepted                                                     |
| Author(s)        | Nghia / Luma                                                 |
| Created          | 2026-07-14                                                   |
| Updated          | 2026-07-14                                                   |
| Tracking issue   | N/A                                                          |
| Supersedes       | N/A                                                          |
| Superseded by    | N/A                                                          |

## Summary

Hiện tại web search có hai đường chạy song song — **server-side**
(provider built-in: Anthropic `web_search_20250305`, OpenAI
`web_search`) và **client-side** (`WebSearchTool` qua Kiro
MCP/Exa/Tavily/SearXNG). Cả hai đường đều được khai báo đồng thời
trong `Registry`, khiến model thấy 2 search tool, không có cơ chế ưu
tiên hay fallback.

RFC này thay thế bằng **priority chain có provider preference**: mỗi
provider khai báo nó muốn dùng server hay client search, hệ thống chỉ
expose đúng 1 đường cho model, và fallback tự động khi đường ưu tiên
không khả dụng.

## Motivation

### 1. Model thấy 2 search tool — gây confusion

`build_registry()` hiện tại:

```rust
reg.add_server_capability("web_search");     // LUÔN khai báo
if let Some(backend) = search {
    reg.register(Box::new(WebSearchTool));   // CÓ THỂ register
}
```

Khi dùng Anthropic model + Kiro auth, model thấy cả:
- `web_search` (server tool, Anthropic built-in)
- `WebSearch` (client tool, Kiro MCP)

Model có thể gọi cả hai, hoặc chọn ngẫu nhiên. Kết quả không
deterministic.

### 2. Codex không dùng được Kiro search

`search_backend()` check `source == "kiro"` trước — Codex model
(source = `"codex"`) luôn trả `None`, rơi vào server-side search của
OpenAI. Ngay cả khi user đã login Kiro và Kiro search miễn phí, tốt
hơn.

```rust
pub(super) fn search_backend(source: &str) -> Option<SearchBackend> {
    if source == "kiro" { return Some(SearchBackend::Kiro); }  // ← chỉ kiro
    // ...env vars...
    None
}
```

### 3. Không có fallback

Nếu server-side search fail (rate limit, provider không hỗ trợ thực
sự), không có cơ chế tự động chuyển sang client-side.

### 4. Mỗi provider có thế mạnh search khác nhau

- **Anthropic**: `web_search_20250305` tích hợp sâu vào reasoning,
  chạy trong cùng inference pass, không thêm round-trip. Chất lượng
  cao.
- **Codex (OpenAI Responses)**: có `web_search` nhưng basic. Kiro
  search cho kết quả tốt hơn và miễn phí.
- **OpenAI Chat**: không có server search. Phải dùng client-side.
- **Kiro gateway**: proxy, không có built-in search. Phải dùng
  client-side.

Một chiến lược "client always wins" sẽ làm giảm chất lượng Claude.
Một chiến lược "server always wins" sẽ bỏ lỡ Kiro search cho Codex.
Cần **per-provider preference**.

## Guide-level explanation

Sau RFC này, mỗi provider khai báo `SearchPreference`:

- `PreferServer` — "tôi có server search tốt, dùng nó trừ khi không
  có". Claude chọn cái này.
- `PreferClient` — "tôi có server search nhưng client tốt hơn, dùng
  client nếu có, fallback server". Codex chọn cái này.
- `ClientOnly` — "tôi không có server search". OpenAI Chat, Kiro
  gateway chọn cái này.

`build_registry()` nhận thêm `SearchPreference` và quyết định:

```
PreferServer  → add_server_capability("web_search"), KHÔNG register WebSearchTool
PreferClient  → nếu có client backend → register WebSearchTool
                nếu không             → fallback add_server_capability
ClientOnly    → nếu có client backend → register WebSearchTool
                nếu không             → không có search
```

Model luôn chỉ thấy **đúng 1** search tool (hoặc 0 nếu không có gì).

`search_backend()` không còn check `source` — nó resolve client
backend thuần túy dựa trên credential/env availability:

```
Kiro auth cached? → Kiro
EXA_API_KEY?      → Exa
TAVILY_API_KEY?   → Tavily
SEARXNG_URL?      → SearXNG
None
```

### Ví dụ: Codex model + user đã login Kiro

```
search_backend() → Some(Kiro)       // Kiro auth available
provider.search_preference() → PreferClient
build_registry(PreferClient, Some(Kiro)):
  → register(WebSearchTool::new(Kiro))
  → KHÔNG add_server_capability
Model thấy: WebSearch (client, Kiro MCP)
```

### Ví dụ: Claude model + user đã login Kiro

```
search_backend() → Some(Kiro)       // Kiro auth available
provider.search_preference() → PreferServer
build_registry(PreferServer, Some(Kiro)):
  → add_server_capability("web_search")
  → KHÔNG register WebSearchTool
Model thấy: web_search (server, Anthropic built-in)
```

### Ví dụ: Codex model + không có client backend

```
search_backend() → None
provider.search_preference() → PreferClient
build_registry(PreferClient, None):
  → fallback: add_server_capability("web_search")
Model thấy: web_search (server, OpenAI built-in)
```

## Reference-level explanation

### 1. New enum: `SearchPreference`

Đặt tại `src/core/provider.rs`:

```rust
/// Provider's preference for web search routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPreference {
    /// Provider has high-quality built-in search (e.g. Anthropic).
    /// MUST use server-side. Client search MUST NOT be registered.
    PreferServer,
    /// Provider has server search but client is preferred (e.g. Codex).
    /// MUST use client if available; MUST fallback to server otherwise.
    PreferClient,
    /// Provider has no server search (e.g. OpenAI Chat, Kiro gateway).
    /// MUST use client if available; no search otherwise.
    ClientOnly,
}
```

### 2. New method on `Provider` trait

```rust
trait Provider {
    /// Search routing preference. Default: ClientOnly.
    fn search_preference(&self) -> SearchPreference {
        SearchPreference::ClientOnly
    }
    // ...existing methods...
}
```

Implementations:

| Provider               | Return value   |
| ---------------------- | -------------- |
| `AnthropicRuntime`     | `PreferServer` |
| `OpenAIResponsesRuntime` (Codex) | `PreferClient` |
| `OpenAIChatRuntime`    | `ClientOnly`   |
| `KiroRuntime`          | `ClientOnly`   |

### 3. `search_backend()` — decouple from source

```rust
fn search_backend() -> Option<SearchBackend> {
    if has_kiro_credential() {
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
```

`has_kiro_credential()` MUST be a **sync, non-blocking** check against
the in-memory credential cache. It MUST NOT trigger a network call.
If the cache is empty (user never logged in to Kiro), it returns
`false`.

### 4. `build_registry()` — mutual exclusion

```rust
pub fn build_registry(
    style: ToolStyle,
    client_search: Option<SearchBackend>,
    search_pref: SearchPreference,
) -> Registry {
    let mut reg = Registry::new();
    // ...file/shell tools (unchanged)...

    match search_pref {
        SearchPreference::PreferServer => {
            reg.add_server_capability("web_search");
        }
        SearchPreference::PreferClient => {
            if let Some(backend) = client_search {
                reg.register(Box::new(WebSearchTool::new(backend)));
            } else {
                reg.add_server_capability("web_search");
            }
        }
        SearchPreference::ClientOnly => {
            if let Some(backend) = client_search {
                reg.register(Box::new(WebSearchTool::new(backend)));
            }
        }
    }

    reg.register(Box::new(web_fetch::WebFetchTool));
    reg
}
```

Invariant: sau `build_registry()`, registry chứa **tối đa 1** trong:
- `server_capabilities` chứa `"web_search"`, HOẶC
- `tools` chứa `"WebSearch"`

KHÔNG BAO GIỜ cả hai.

### 5. Caller changes

`ensure_agent_loop()` (tui) và `resolve_search()` (acp bridge) MUST
pass `SearchPreference` vào `build_registry`. Preference được lấy từ
provider binding — cùng chỗ resolve gateway/protocol.

Vì `Provider` trait instance chưa tồn tại tại thời điểm
`build_registry` (provider được tạo sau trong `agent_loop`), preference
MUST được resolve từ `source: &str` qua một pure function:

```rust
pub fn search_preference_for(source: &str) -> SearchPreference {
    match source {
        "anthropic" => SearchPreference::PreferServer,
        "codex"     => SearchPreference::PreferClient,
        _           => SearchPreference::ClientOnly,
    }
}
```

Hàm này đặt cạnh `build_registry` trong `src/tool/mod.rs` hoặc trong
`src/provider/binding.rs`.

### 6. Result matrix

| Provider | Client available | Search used | Tool exposed |
| --- | --- | --- | --- |
| Anthropic | ✅ Kiro | Server (Anthropic) | `web_search` server |
| Anthropic | ❌ | Server (Anthropic) | `web_search` server |
| Codex | ✅ Kiro | Client (Kiro) | `WebSearch` client |
| Codex | ✅ Exa | Client (Exa) | `WebSearch` client |
| Codex | ❌ | Server (OpenAI) | `web_search` server |
| OpenAI Chat | ✅ Kiro | Client (Kiro) | `WebSearch` client |
| OpenAI Chat | ❌ | ❌ None | — |
| Kiro gateway | ✅ Kiro | Client (Kiro) | `WebSearch` client |
| Kiro gateway | ❌ | ❌ None | — |

### 7. Test plan

- Unit test `build_registry` với mỗi combination (3 prefs × 2
  client states = 6 cases). Assert invariant: không bao giờ cả
  `has_capability("web_search")` lẫn `get("WebSearch").is_some()`.
- Unit test `search_preference_for()` cho mỗi known source.
- Integration: chạy Codex model với Kiro auth, verify `WebSearch`
  tool call (không phải `web_search` server event).

## Drawbacks

1. **Anthropic mất client fallback**: nếu Anthropic server search
   fail (rate limit, outage), không có fallback sang Kiro. Chấp nhận
   được vì Anthropic search rất ổn định và retry logic đã có.

2. **Thêm 1 parameter vào `build_registry`**: minor API churn. Nhưng
   function này chỉ có 2 call sites (tui + acp bridge).

3. **`has_kiro_credential()` sync check**: cần expose credential
   cache state. Nếu cache chưa warm (app vừa start), có thể miss
   Kiro backend ở turn đầu tiên. Mitigation: credential cache được
   warm tại login time, trước khi agent loop start.

## Rationale and alternatives

### Alternative 1: Client always wins

```rust
if let Some(backend) = client_search {
    reg.register(WebSearchTool);
} else {
    reg.add_server_capability("web_search");
}
```

Đơn giản nhưng **giảm chất lượng Claude**. Anthropic
`web_search_20250305` chạy trong cùng inference pass — zero
round-trip. Client search thêm 1 turn (model → client → search →
client → model). Với Claude, server search tốt hơn cả về latency lẫn
quality.

**Loại vì**: sacrifice quality cho simplicity.

### Alternative 2: Server always wins

```rust
reg.add_server_capability("web_search");
// never register WebSearchTool
```

Đơn giản nhất nhưng **Codex bỏ lỡ Kiro search** (miễn phí, chất
lượng tốt hơn OpenAI built-in). OpenAI Chat không có server search →
mất search hoàn toàn.

**Loại vì**: không tận dụng được Kiro search.

### Alternative 3: Không làm gì

Model tiếp tục thấy 2 search tool khi cả hai đường đều available.
Kết quả không deterministic, đôi khi model gọi cả hai gây duplicate
search.

**Loại vì**: UX kém, lãng phí resource.

### Tại sao chọn per-provider preference

- Tôn trọng thế mạnh từng provider (Claude search tốt → dùng, Codex
  search kém → thay thế)
- Mutual exclusion đảm bảo model chỉ thấy 1 tool
- Fallback tự nhiên (PreferClient không có client → dùng server)
- Extensible: thêm provider mới = thêm 1 dòng trong match

## Prior art

- **Claude Code**: dùng Anthropic server `web_search` exclusively,
  không có client fallback. Đơn giản vì chỉ support 1 provider.
- **Cursor**: routing search qua proxy server riêng, không dùng
  provider built-in. Tương đương "client always wins".
- **Codex CLI (upstream)**: dùng OpenAI Responses `web_search`
  server tool. Không có client-side search.

## Unresolved questions

1. **Nên cho user override preference qua config?** Ví dụ
   `SEARCH_PREFER=server` để force server search cho Codex. Đề xuất:
   không cần ở v1, thêm sau nếu có demand.

2. **Anthropic có nên có client fallback?** Nếu server search fail,
   có nên retry với client? Đề xuất: không ở v1. Anthropic search
   rất ổn định, retry logic hiện tại đủ.

3. **`has_kiro_credential()` implementation**: dùng
   `auth::pool` in-memory check hay đọc file cache? Đề xuất:
   in-memory check từ auth pool — đã warm tại app start.

## Future possibilities

- **Runtime fallback**: nếu server search fail mid-turn, agent loop
  có thể swap sang client search và retry turn. Cần thay đổi
  `run_turn()` — phức tạp hơn, để sau.
- **Per-model preference**: thay vì per-provider, cho phép mỗi model
  khai báo preference trong catalog. Ví dụ: `gpt-4o-search-preview`
  có thể prefer server, `gpt-5.4` prefer client.
- **Search quality metrics**: log search source + result quality để
  data-driven tuning preference defaults.
- **User config**: `~/.config/luma/search.toml` cho phép user
  override preference, backend priority, max results.

## Implementation status

Implemented in commit following this RFC. Changes:

- `src/core/provider.rs`: added `SearchPreference` enum
- `src/tool/mod.rs`: added `search_preference_for()`, updated
  `build_registry()` with mutual-exclusion routing
- `src/config/auth/mod.rs`: added `has_kiro_credential()` sync check
- `src/tui/app/agent.rs`: decoupled `search_backend()` from source
- `src/tui/app/commands.rs`: updated `build_registry` call
- `src/acp/bridge.rs`: updated `resolve_search()` and `build_registry` call
