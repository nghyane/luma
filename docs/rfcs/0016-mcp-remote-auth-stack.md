# RFC 0016: MCP Remote Auth Stack

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0016                                         |
| Title            | MCP Remote Auth Stack                        |
| Status           | Draft                                        |
| Author(s)        | Nghia Hoang                                  |
| Created          | 2026-04-18                                   |
| Updated          | 2026-04-18                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Bổ sung full auth stack cho remote MCP servers trong luma, tách khỏi RFC
0015 vốn chỉ cover MCP integration cơ bản. Stack mới bao gồm OAuth
discovery, browser-based authorization flow, callback handling,
secret/token persistence, refresh/revoke, status model `needs-auth`, và
retry/reconnect semantics cho remote transports (`http`, `sse`). Mục tiêu
là đạt mức behavior thực dụng gần Claude Code cho MCP auth mà vẫn giữ
kiến trúc phù hợp với codebase Rust hiện tại.

## Motivation

- Remote MCP support hiện đã có transport + config + token injection cơ
  bản, nhưng chưa có authorization flow hoàn chỉnh.
- Nhiều MCP servers public/private yêu cầu OAuth discovery qua
  `WWW-Authenticate`, Protected Resource Metadata, và Authorization
  Server Metadata thay vì chỉ static bearer token.
- Local evidence:
  - `src/mcp/config.rs` đã có `oauth.clientId` và
    `oauth.authServerMetadataUrl`.
  - `src/mcp/auth.rs` đã có secret store, refresh-token flow cơ bản, và
    auto-discovery `authServerMetadataUrl -> token_endpoint`.
  - `src/mcp/manager.rs` đã có `NeedsAuth` classification nhưng mới dựa
    trên message heuristics.
- Workaround hiện tại (`luma mcp set-secret --access-token ...`) không đủ
  cho servers yêu cầu auth flow chuẩn hoặc rotate token ngắn hạn.

## Guide-level explanation

Người dùng cấu hình remote MCP server giống Claude Code:

```json
{
  "mcpServers": {
    "figma": {
      "type": "http",
      "url": "https://mcp.figma.com/mcp",
      "oauth": {
        "clientId": "...",
        "authServerMetadataUrl": "https://issuer/.well-known/oauth-authorization-server"
      }
    }
  }
}
```

Khi luma khởi động và server yêu cầu auth:

1. Nếu đã có access token hợp lệ, luma dùng luôn.
2. Nếu token hết hạn, luma refresh bằng refresh token.
3. Nếu chưa có token hoặc refresh fail, luma:
   - đọc `WWW-Authenticate`
   - discover Protected Resource Metadata / Authorization Server Metadata
   - mở browser để user complete OAuth flow
   - nhận callback local
   - lưu tokens/secrets
   - retry connect/tool call

Trong UI/CLI, server được phân loại rõ:
- `connected`
- `needs auth`
- `failed`

`luma mcp get <name>` cho biết server đang thiếu client secret, thiếu
refresh token, hay chờ user auth.

## Reference-level explanation

### Scope

RFC này MUST cover remote transports do luma hiện support:
- `http`
- `sse`

RFC này MUST NOT change stdio auth behavior.

### Data model

`src/mcp/config.rs` MUST support and preserve:
- `oauth.clientId`
- `oauth.authServerMetadataUrl`
- `headersHelper`
- future-compatible unknown auth fields

`src/mcp/auth.rs` MUST persist, per `server_key`:
- `client_id`
- `client_secret`
- `access_token`
- `refresh_token`
- `auth_server_metadata_url`
- `resource_metadata_url`
- `token_endpoint`
- `authorization_endpoint`
- `revocation_endpoint`
- `scopes`
- `expires_at_unix_ms`

Storage key MUST remain identity-based:
- `{server_name}|{transport}|{url}`

### Discovery

luma MUST resolve OAuth endpoints in this order:

1. persisted endpoints in store
2. configured `oauth.authServerMetadataUrl`
3. `WWW-Authenticate` header with `resource_metadata=...`
4. Protected Resource Metadata -> Authorization Server Metadata

If discovery yields Authorization Server Metadata, luma MUST cache the
resolved endpoints in the MCP OAuth store.

### Interactive auth flow

luma MUST provide a browser-based authorization-code flow with PKCE for
remote MCP servers that require user auth.

Implementation MUST:
- allocate a local callback port
- generate PKCE verifier/challenge + state
- open browser to authorization endpoint
- validate callback state
- exchange code for tokens
- persist tokens and client metadata
- retry the original connection or tool call once auth succeeds

The callback listener MUST timeout and MUST surface a user-actionable
error when cancelled or expired.

### Client registration strategy

Phase 1 SHOULD prefer pre-configured `clientId` from config.

If `clientId` is missing and the discovered authorization server exposes
Dynamic Client Registration, luma MAY register a public client and store:
- `client_id`
- `client_secret` if returned

If neither pre-configured `clientId` nor dynamic registration is
available, luma MUST surface `needs-auth` with an actionable message.

### Refresh/revoke

luma MUST refresh access tokens automatically when:
- access token is expired, or
- remote connect returns auth-required and a refresh token is available

luma SHOULD revoke refresh/access tokens when the user clears auth.

If revoke fails, luma MUST still clear local stored tokens.

### Status model

`src/mcp/manager.rs` MUST expose distinct statuses:
- `Connected`
- `NeedsAuth`
- `Failed`

`NeedsAuth` MUST carry enough message/context for CLI/TUI to explain the
next action.

### CLI

CLI MUST support:
- inspect auth state
- trigger auth flow explicitly
- clear auth
- set auth hints/secrets manually

Minimum subcommands:
- `luma mcp get <name>`
- `luma mcp auth <name>`
- `luma mcp clear-secret <name>`
- existing `set-secret` for manual bootstrap

### File layout

Expected modules:

- `src/mcp/config.rs` — config schema
- `src/mcp/auth.rs` — storage, discovery, token refresh/revoke
- `src/mcp/oauth.rs` — browser flow + callback listener
- `src/mcp/manager.rs` — connect/retry/status lifecycle
- `src/mcp/cli.rs` — auth-related commands

### Migration plan

Existing `mcp_oauth.db` entries MUST continue to load.
New fields MUST be optional and backfilled lazily as discovery/auth runs.

### Test plan

Implementation MUST add tests for:
- config parsing of auth fields
- server-key stability
- token endpoint discovery from metadata URL
- parsing `WWW-Authenticate` with `resource_metadata`
- callback state validation
- refresh token success/failure
- `NeedsAuth` status mapping

### Rollout / rollback

Rollout SHOULD be phase-based:
1. discovery + explicit auth state
2. browser auth flow
3. revoke + registration improvements

Rollback MAY disable interactive auth while preserving manual token mode.

## Drawbacks

- Tăng đáng kể độ phức tạp của module MCP.
- Browser auth + callback listener làm CLI behavior phức tạp hơn.
- SQLite secret store chưa mạnh bằng native OS keychain.
- Cần nhiều edge-case handling cho OAuth servers không chuẩn.

## Rationale and alternatives

- Chọn RFC mới thay vì nhồi vào RFC 0015 vì auth stack đã là một mảng
  đủ lớn, có lifecycle và tradeoff riêng.
- Chọn giữ SQLite store hiện tại thay vì nhảy ngay sang OS keychain vì
  codebase đã có precedent dùng SQLite cho persistence và chưa có secure
  storage abstraction chung.
- Alternative 1: chỉ support manual bearer token. Loại vì không đáp ứng
  servers yêu cầu OAuth chuẩn.
- Alternative 2: clone hoàn toàn secure storage/native auth model của
  Claude Code. Loại ở giai đoạn này vì chi phí cao, cross-platform churn
  lớn.

## Prior art

- Claude Code MCP auth stack: remote transports, OAuth discovery,
  secure storage, refresh/revoke, `needs-auth` semantics.
- MCP authorization documentation:
  `https://apps.extensions.modelcontextprotocol.io/api/documents/authorization.html`
- RFC 9728 Protected Resource Metadata.
- `rmcp` auth/discovery machinery trong `transport/auth.rs`.

## Unresolved questions

- Có nên chuyển secret store từ SQLite sang native keychain trong phase
  sau? Mặc định: chưa.
- Có nên support Dynamic Client Registration ngay phase đầu? Mặc định:
  chưa, chỉ nếu server không có pre-configured `clientId`.
- Có nên expose auth flow trực tiếp trong TUI ngoài CLI? Mặc định: sau
  khi CLI flow ổn định.

## Future possibilities

- Native OS secure storage abstraction.
- Dynamic client registration support đầy đủ.
- Scope upgrade flow.
- Revoke UI.
- Shared OAuth primitives cho các provider khác ngoài MCP.

## Implementation status

Chưa implement theo RFC này. Local groundwork đã có:
- remote `http`/`sse` transports
- MCP OAuth SQLite store
- token injection
- refresh token flow cơ bản
- `NeedsAuth` status cơ bản
