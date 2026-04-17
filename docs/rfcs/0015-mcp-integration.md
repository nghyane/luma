# RFC 0015: MCP (Model Context Protocol) Integration

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0015                                         |
| Title            | MCP (Model Context Protocol) Integration     |
| Status           | Draft                                        |
| Author(s)        | Nghia Hoang                                  |
| Created          | 2025-07-15                                   |
| Updated          | 2025-07-15                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Thêm MCP client vào luma để agent có thể gọi tools từ MCP servers bên
ngoài (stdio subprocess, streamable HTTP). MCP tools xuất hiện trong
`Registry` cùng với built-in tools — model thấy chúng như tool bình
thường, agent loop gọi qua MCP client thay vì local `Tool::execute`.

## Motivation

- luma hiện chỉ có ~15 built-in tools. Người dùng không thể mở rộng
  tool set mà không fork code.
- Hệ sinh thái MCP đã có hàng nghìn server (Sentry, GitHub, Postgres,
  Slack, …). Tích hợp MCP cho phép luma dùng tất cả mà không cần
  implement từng cái.
- Claude Code, Cursor, Windsurf, Kiro đều hỗ trợ MCP — đây là
  expectation cơ bản của coding agent 2025.
- Không có MCP, luma bị giới hạn ở local file/shell tools. Không thể
  tương tác với external services (DB, API, monitoring) trong agent loop.

## Guide-level explanation

### Config

Người dùng thêm MCP servers vào `~/.config/luma/mcp.json` hoặc
`.luma/mcp.json` (project-level):

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_..." }
    },
    "sentry": {
      "type": "http",
      "url": "https://mcp.sentry.dev/mcp"
    }
  }
}
```

### Startup

Khi luma khởi động, nó đọc config, spawn MCP clients, gọi
`initialize` + `tools/list` trên mỗi server. Tools được đăng ký vào
`Registry` với prefix `mcp__{server}__{tool}`.

### Agent loop

Model thấy MCP tools trong tool list. Khi model gọi
`mcp__github__search_repositories`, agent loop nhận ra prefix `mcp__`,
route call đến MCP client tương ứng thay vì local `Tool::execute`.

### TUI

MCP server status hiển thị trong status bar. `/mcp` command liệt kê
servers và trạng thái (connected / failed / pending).

### CLI

```bash
# Thêm stdio server
luma mcp add github -- npx -y @modelcontextprotocol/server-github

# Thêm HTTP server
luma mcp add sentry --transport http https://mcp.sentry.dev/mcp

# Liệt kê
luma mcp list

# Xóa
luma mcp remove github
```

## Reference-level explanation

### Dependencies

```toml
rmcp = { version = "1.5", features = [
  "client",
  "transport-child-process",
  "transport-streamable-http-client",
  "transport-streamable-http-client-reqwest",
] }
```

`rmcp` là official Rust SDK từ `modelcontextprotocol/rust-sdk`. Nó
dùng tokio, serde_json, futures — tất cả đã có trong luma.

### Data model

```rust
// src/config/mcp.rs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    #[serde(alias = "stdio", untagged)]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}
```

Untagged `Stdio` variant cho backward compat — config không cần
`"type": "stdio"` (giống Claude Code).

### Config resolution

Config MUST được merge theo thứ tự ưu tiên (sau ghi đè trước):

1. `.luma/mcp.json` (project-local)
2. `~/.config/luma/mcp.json` (user-global)

Project config ghi đè user config khi cùng server name.

### MCP service module

```
src/
  mcp/
    mod.rs          — public API: McpManager
    config.rs       — config loading + merge
    client.rs       — connection lifecycle per server
    bridge.rs       — McpTool adapter (impl Tool)
```

#### McpManager

```rust
// src/mcp/mod.rs

pub struct McpManager {
    clients: HashMap<String, McpClient>,
}

pub struct McpClient {
    name: String,
    // rmcp RunningService handle
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tools: Vec<rmcp::model::Tool>,
    status: McpStatus,
}

pub enum McpStatus {
    Connected,
    Failed(String),
    Pending,
}

impl McpManager {
    /// Spawn all configured servers. Non-blocking — failures are
    /// captured per-server, not propagated.
    pub async fn start(config: &McpConfig) -> Self { ... }

    /// Register MCP tools into an existing Registry.
    pub fn register_tools(&self, registry: &mut Registry) { ... }

    /// Call a tool on the appropriate server.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<String> { ... }

    /// Shutdown all clients gracefully.
    pub async fn shutdown(&self) { ... }
}
```

#### McpTool bridge

MCP tools MUST implement `core::tool::Tool` so they appear in
`Registry` identically to built-in tools:

```rust
// src/mcp/bridge.rs

pub struct McpTool {
    pub server_name: String,
    pub tool_name: String,
    pub prefixed_name: String,   // mcp__{server}__{tool}
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl Tool for McpTool {
    fn name(&self) -> &str { &self.prefixed_name }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.prefixed_name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
            streamable_arg: None,
        }
    }

    fn execute(&self, args, output_tx, cancel, _caps) -> ... {
        // Delegate to McpManager::call_tool via a global handle
        // or captured Arc<McpManager>.
    }
}
```

#### Naming convention

Tool name: `mcp__{normalize(server_name)}__{tool_name}`

`normalize`: replace non-alphanumeric with `_`, collapse runs, trim.
Giống Claude Code — đảm bảo model không nhầm lẫn giữa built-in và
MCP tools.

### Integration vào agent loop

Thay đổi tối thiểu — MCP tools đã implement `Tool` trait nên
`execute_one` trong `turn.rs` gọi chúng qua `registry.get()` như
bình thường. Không cần sửa turn loop.

Thay đổi cần thiết:

1. `build_registry` trong `src/tool/mod.rs` nhận thêm `&McpManager`
   và gọi `manager.register_tools(&mut reg)`.

2. `AgentConfig` hoặc `spawn()` trong `src/core/agent.rs` nhận
   `McpManager` (wrapped trong `Arc`) để lifetime outlive agent task.

3. `main.rs` khởi tạo `McpManager::start()` trước khi spawn agent.

### Connection lifecycle

1. **Startup**: `McpManager::start()` spawns mỗi server config thành
   một tokio task. Stdio servers dùng `TokioChildProcess`, HTTP dùng
   `StreamableHttpClientTransport`. Mỗi client gọi MCP `initialize`
   rồi `tools/list`.

2. **Timeout**: Connection MUST timeout sau 30s. Server không respond
   → `McpStatus::Failed`.

3. **Failure isolation**: Một server fail MUST NOT block startup hoặc
   ảnh hưởng server khác. `McpManager` log warning và tiếp tục.

4. **Shutdown**: Khi luma exit, `McpManager::shutdown()` gọi
   `cancel()` trên mỗi `RunningService`. Stdio servers nhận SIGTERM
   qua child process drop.

### Hot-reload

Khi user chạy `/mcp add` hoặc `/mcp remove` trong TUI:

1. Update config file.
2. `McpManager` disconnect server cũ (nếu remove) hoặc spawn server
   mới (nếu add).
3. Rebuild `Registry` và gửi `AgentCommand::SetContext` để agent loop
   nhận tool set mới.

### Event integration

Thêm events cho MCP lifecycle:

```rust
// Trong Event enum
McpServerConnected { name: String },
McpServerFailed { name: String, error: String },
```

TUI hiển thị trạng thái MCP servers trong status bar hoặc khi user
gọi `/mcp`.

### Compatibility với Claude Code config

luma SHOULD đọc được `.claude/settings.json` → `mcpServers` nếu
không tìm thấy `.luma/mcp.json`. Cho phép người dùng chuyển từ Claude
Code sang luma mà không cần reconfigure MCP servers.

Thứ tự fallback:
1. `.luma/mcp.json`
2. `~/.config/luma/mcp.json`
3. `.claude/settings.json` → extract `mcpServers`
4. `~/.claude.json` → extract `mcpServers`

## Drawbacks

- **Binary size**: `rmcp` + `reqwest` (cho HTTP transport) thêm ~1-2MB
  vào release binary. luma đã dùng `reqwest` nên impact thực tế nhỏ
  hơn.

- **Startup latency**: Mỗi MCP server cần ~0.5-2s để spawn + initialize.
  Mitigated bằng parallel spawn + không block TUI render.

- **Complexity**: Thêm một layer abstraction (McpTool bridge). Nhưng
  nó isolated trong `src/mcp/` và không chạm vào core agent logic.

- **Child process management**: Stdio servers là subprocess — cần
  handle crash, restart, zombie process. `rmcp` xử lý phần lớn qua
  `TokioChildProcess` nhưng edge cases vẫn có.

## Rationale and alternatives

### Tại sao `rmcp` (official SDK)

- Maintained bởi `modelcontextprotocol` org.
- Hỗ trợ cả stdio và streamable HTTP.
- Dùng tokio — match runtime của luma.
- Alternatives (`mcp-sdk-rs`, `mcp_client_rs`) ít maintained, thiếu
  transport options.

### Alternative 1: Custom MCP client

Implement JSON-RPC over stdio/HTTP từ scratch. Ưu điểm: zero
dependency. Nhược điểm: phải maintain protocol compliance, miss spec
updates, tốn 2-3 tuần thay vì 3-5 ngày.

→ Loại vì cost/benefit không hợp lý.

### Alternative 2: Chỉ hỗ trợ stdio

Bỏ HTTP transport, chỉ spawn subprocess. Đơn giản hơn nhưng loại bỏ
remote MCP servers (Sentry, Cloudflare, etc.) — đây là use case phổ
biến nhất.

→ Loại vì giảm giá trị quá nhiều.

### Không làm gì

luma tiếp tục chỉ có built-in tools. Người dùng cần external
integration phải dùng Claude Code hoặc Cursor. Đây là competitive
disadvantage ngày càng lớn.

## Prior art

- **Claude Code** (`codeaashu/claude-code`): TypeScript, hỗ trợ 6
  transport types (stdio, sse, http, ws, sdk, claudeai-proxy). Config
  scopes: local/user/project/enterprise. Tool naming:
  `mcp__{server}__{tool}`. Tham khảo chính cho naming convention và
  config format.

- **Cursor**: MCP support qua `~/.cursor/mcp.json`, format giống
  Claude Desktop.

- **Zed**: Rust editor, tích hợp MCP cho context providers. Dùng
  custom client, không dùng `rmcp`.

- **MCP Spec 2025-03-26**: Deprecated SSE, recommend streamable HTTP
  cho remote. Stdio vẫn là standard cho local servers.

## Unresolved questions

1. **Reconnect policy**: Khi MCP server crash mid-session, tự động
   reconnect hay báo user? Đề xuất: auto-reconnect 3 lần với backoff,
   sau đó mark failed và báo user.

2. **Tool approval**: MCP tools có cần user approval trước khi execute
   không? Claude Code có permission system phức tạp. Đề xuất: phase 1
   trust all configured servers, phase 2 thêm approval cho
   destructive tools.

3. **Resource/Prompt support**: MCP spec có Resources và Prompts ngoài
   Tools. Đề xuất: phase 1 chỉ Tools, phase 2 thêm Resources (inject
   vào context), phase 3 Prompts.

4. **Max concurrent servers**: Giới hạn bao nhiêu MCP server đồng
   thời? Đề xuất: soft limit 20, configurable.

5. **Env var interpolation**: Config có nên hỗ trợ `${ENV_VAR}` trong
   values không? Đề xuất: có, cho `env` field và `url` field.

## Future possibilities

- **MCP server mode**: luma expose built-in tools qua MCP protocol
  (giống Claude Code's `src/entrypoints/mcp.ts`). Cho phép IDE hoặc
  agent khác gọi luma tools.

- **OAuth support**: HTTP MCP servers yêu cầu OAuth (spec 2025-03-26
  có authorization flow). `rmcp` đã có `auth` feature.

- **Dynamic tool discovery**: Server thêm/bớt tools runtime qua
  `notifications/tools/list_changed`. Client re-fetch và update
  Registry.

- **Sampling**: MCP spec cho phép server request LLM completion từ
  client. Cho phép MCP server dùng luma's model access.

- **`luma mcp serve`**: Chạy luma như MCP server, expose tools cho
  Claude Desktop hoặc IDE khác.

## Implementation status

Chưa bắt đầu. Phân pha đề xuất:

| Phase | Scope | Estimate |
|-------|-------|----------|
| 1 | Config loading + stdio client + McpTool bridge + Registry integration | 3 ngày |
| 2 | HTTP transport + `/mcp` TUI command + status bar | 2 ngày |
| 3 | CLI commands (`luma mcp add/remove/list`) + Claude Code config compat | 1 ngày |
| 4 | Hot-reload + reconnect + error UX | 2 ngày |
