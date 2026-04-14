# RFC 0005: Kiro Provider

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0005                                         |
| Title            | Kiro Provider (Amazon Q / CodeWhisperer)     |
| Status           | Implemented                                  |
| Author(s)        | nghyane                                      |
| Created          | 2026-04-13                                   |
| Updated          | 2026-04-13                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Thêm Kiro (Amazon Q / CodeWhisperer) làm provider thứ ba trong luma,
bên cạnh Anthropic và OpenAI. Auth dùng Social Login (Google/GitHub)
qua PKCE flow của Kiro, token lưu trong pool hiện có. Không cần
keychain — credentials đọc từ file JSON hoặc login trực tiếp qua luma.

## Motivation

- Kiro IDE dùng Amazon Q (CodeWhisperer) làm backend. User đã có
  account Kiro có thể tái sử dụng mà không cần thêm subscription.
- Kiro CLI lưu token trong macOS Keychain (`kirocli:social:token`),
  nhưng luma cần hoạt động cross-platform và không phụ thuộc keychain.
- Cần hỗ trợ cả hai path: import credentials từ Kiro CLI (zero-config)
  và login mới trực tiếp qua luma.

## Guide-level explanation

Sau RFC này, user có thể:

```
# Login mới
luma login  →  chọn "Kiro (Google / GitHub)"  →  browser mở  →  done

# Hoặc import từ Kiro CLI đang có sẵn (macOS)
luma login  →  chọn "Import from Kiro CLI"  →  đọc keychain tự động
```

Trong model picker, các model Kiro hiển thị với prefix `kiro/`:
- `kiro/auto` — model tự chọn tối ưu (mặc định)
- `kiro/claude-sonnet-4.5`
- `kiro/claude-sonnet-4`
- `kiro/claude-haiku-4.5`
- `kiro/deepseek-3.2`
- `kiro/qwen3-coder-next`

Tool use hoạt động đầy đủ — luma gửi tool definitions và nhận
`toolUseEvent` stream, xử lý tool loop như Anthropic/OpenAI.

## Reference-level explanation

### Endpoints (đã verified)

| Mục đích | URL |
|----------|-----|
| List models | `POST https://q.us-east-1.amazonaws.com/?origin=KIRO_CLI&profileArn=<arn>` body: `{"origin":"KIRO_CLI","profileArn":"..."}` |
| Chat | `POST https://q.us-east-1.amazonaws.com/generateAssistantResponse?origin=KIRO_CLI&profileArn=<arn>` |
| Refresh token | `POST https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken` |
| Token exchange | `POST https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token` |
| Authorize | `GET https://prod.us-east-1.auth.desktop.kiro.dev/signin` |

### Chat request body

```json
{
  "conversationState": {
    "chatTriggerType": "MANUAL",
    "conversationId": "<uuid>",
    "agentContinuationId": "<uuid>",
    "agentTaskType": "vibe",
    "history": [
      {
        "userInputMessage": {
          "content": "...",
          "origin": "KIRO_CLI",
          "modelId": "auto",
          "userInputMessageContext": {
            "envState": { "operatingSystem": "macos", "currentWorkingDirectory": "/..." },
            "tools": [{ "toolSpecification": { "name": "...", "description": "...", "inputSchema": { "json": { ...schema } } } }]
          }
        }
      },
      {
        "assistantResponseMessage": {
          "messageId": "<uuid>",
          "content": "...",
          "toolUses": [{ "toolUseId": "...", "name": "...", "input": { ... } }]
        }
      }
    ],
    "currentMessage": {
      "userInputMessage": {
        "content": "",
        "origin": "KIRO_CLI",
        "modelId": "auto",
        "userInputMessageContext": {
          "envState": { ... },
          "tools": [...],
          "toolResults": [{
            "toolUseId": "...",
            "content": [{ "text": "..." }],
            "status": "success"
          }]
        }
      }
    }
  },
  "profileArn": "arn:aws:codewhisperer:us-east-1:...:profile/..."
}
```

### Response event stream (AWS Event Stream binary)

Frame format: `[4B total_len][4B headers_len][4B prelude_crc][headers][JSON payload][4B msg_crc]`

Event types:
- `initial-response` → `{"conversationId": ""}` — chỉ turn đầu
- `assistantResponseEvent` → `{"content": "...", "modelId": "auto"}` — text streaming
- `toolUseEvent` → `{"name": "...", "toolUseId": "...", "input": "..."}` — streaming JSON input, kết thúc khi `"stop": true`
- `contextUsageEvent` → `{"contextUsagePercentage": 5.4}`
- `meteringEvent` → `{"unit": "credit", "usage": 0.107}`

### Tool use flow

```
1. Gửi request với tools[] trong userInputMessageContext
2. Nhận toolUseEvent stream → ghép input JSON
3. Khi stop=true → execute tool locally
4. Gửi request tiếp với toolResults[] trong currentMessage
5. Lặp đến khi chỉ còn assistantResponseEvent
```

### Auth endpoints

**Authorize URL:**
```
GET https://prod.us-east-1.auth.desktop.kiro.dev/signin
  ?state=<random>
  &code_challenge=<sha256_base64url>
  &code_challenge_method=S256
  &redirect_uri=http://localhost:<port>/oauth/callback
  &redirect_from=kirocli
```

**Token exchange** (`POST /oauth/token`, JSON):
```json
{ "code": "...", "redirectUri": "...", "codeVerifier": "..." }
```

**Refresh** (`POST /refreshToken`, JSON):
```json
{ "refreshToken": "..." }
```
Response: `{ "accessToken", "refreshToken", "expiresIn": 3600, "profileArn" }`

### AccountEntry + Credential extensions

`AccountEntry` (private, serialized) thêm:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
profile_arn: Option<String>,
```

`Credential` (public) thêm:
```rust
pub profile_arn: Option<String>,
```

`credential_from(&entry)` copy `profile_arn` sang `Credential`. Gateway
dùng `credential.profile_arn` để build query param và request body.

### ProtocolId

Thêm variant mới:
```rust
pub enum ProtocolId {
    AnthropicMessages,
    OpenAIChat,
    OpenAIResponses,
    KiroEventStream,   // AWS Event Stream binary, endpoint /generateAssistantResponse
}
```

### try_refresh camelCase mapping

Kiro refresh response dùng camelCase (`accessToken`, `refreshToken`,
`expiresIn`) khác Anthropic/OpenAI (`access_token`, `refresh_token`,
`expires_in`). `try_refresh` cần fallback:
```rust
let new_access = json.get("access_token")
    .or_else(|| json.get("accessToken"))
    ...
```

### scan_kiro + models sync

`sync()` trong `models.rs` thêm `scan_kiro()`:
```rust
POST https://q.us-east-1.amazonaws.com/?origin=KIRO_CLI&profileArn=<arn>
body: {"origin":"KIRO_CLI","profileArn":"..."}
→ { "models": [{ "modelId", "modelName", "tokenLimits": { "maxInputTokens", "maxOutputTokens" } }] }
```

Cần credential Kiro để gọi — `scan_kiro` chỉ chạy nếu có account Kiro
trong pool. Source string: `"kiro"`.

### agentContinuationId

Generate một UUID v4 khi bắt đầu conversation, giữ trong
`AgentConfig` (thêm field `continuation_id: Option<String>`). Gửi
trong mỗi request của cùng conversation. `agentTaskType` luôn là
`"vibe"`.

### File layout

```
src/config/auth/
  mod.rs          — thêm AuthVendor::Kiro, load_kiro_keychain() (macOS)
src/config/auth/pkce.rs
                  — thêm ProviderFlow::kiro(...)
src/config/auth/policy.rs
                  — thêm RefreshRequest cho Kiro (JSON body)
src/provider/gateways/
  kiro.rs         — GatewayId::Kiro
src/provider/protocol/
  kiro.rs         — AWS Event Stream decoder + request builder
src/provider/
  binding.rs      — map AuthVendor::Kiro → GatewayId::Kiro
src/config/
  models.rs       — thêm Kiro models vào catalog
```

### Test plan

- Unit: parse AWS Event Stream frames
- Unit: `toolUseEvent` input streaming + stop detection
- Unit: `load_kiro_keychain` parse JSON format
- Unit: refresh request JSON body
- Integration: full tool loop end-to-end (manual)

## Drawbacks

- AWS Event Stream decoder là code mới hoàn toàn (~100 LOC).
- `profile_arn` là field mới trong `AccountEntry` — backward-compatible vì optional.
- macOS-only cho zero-config import. Linux/Windows cần manual login.

## Rationale and alternatives

**Tại sao không dùng AWS SDK?**
Quá nặng. Gọi HTTP trực tiếp như Anthropic/OpenAI.

**Alternative: chỉ text streaming, không tool use**
Đơn giản hơn nhưng không tận dụng được full capability. Loại vì tool
use đã verified hoạt động và là core feature của luma.

## Prior art

- `src/config/auth/pkce.rs` — PKCE flow tái dùng trực tiếp.
- `src/provider/sse.rs` — SSE decoder làm mẫu cho Event Stream decoder.
- Kiro CLI binary reversed: `crates/fig_auth/src/social.rs`.

## Unresolved questions

1. **GitHub login**: authorize URL có cần param `provider=github` không? Chưa probe.
2. **`agentContinuationId`**: có cần generate mới mỗi conversation không, hay giữ nguyên?

## Future possibilities

- AWS Identity Center (IdC/SSO) — Kiro CLI đã có flow này.
- Multi-account Kiro — pool hiện tại đã hỗ trợ.

## Implementation status

Chưa implement.
