# RFC 0004: Capability-Aware Read — Native Image Support Across Providers

| Field            | Value                                             |
| ---------------- | ------------------------------------------------- |
| RFC              | 0004                                              |
| Title            | Capability-Aware Read — Native Image Support      |
| Status           | Draft                                             |
| Author(s)        | Nghia / Luma                                      |
| Created          | 2026-04-13                                        |
| Updated          | 2026-04-13                                        |
| Tracking issue   | N/A                                               |
| Supersedes       | N/A                                               |
| Superseded by    | N/A                                               |

## Summary

Cho phép agent đọc file ảnh qua chính `Read` tool thay vì block với
`Cannot read binary file`. Tool trở thành **capability-aware**: nếu
model hiện tại hỗ trợ vision, `Read("logo.png")` MUST trả về image
block để model "nhìn" được; nếu không, trả text metadata mô tả ảnh.
Agent không cần học tool mới, schema không đổi, workflow tự nhiên
như đọc text file.

Thay đổi gồm 3 phần: (1) `ContentBlock::ToolResult` thêm field
`images: Vec<ToolResultImage>` để carry image attachments; (2)
catalog model thêm `capabilities: Vec<String>` (e.g. `["vision"]`);
(3) `Tool::execute` nhận thêm `ModelCaps` để branch theo vision
support. Anthropic và OpenAI Responses serialize natively; OpenAI
Chat Completions không hỗ trợ tool-result image — trả metadata text
với note hướng dẫn chuyển gateway.

## Motivation

### Vấn đề hiện tại

`src/tool/read.rs:17-21` khai báo `BINARY_EXTENSIONS` gồm
`png, jpg, jpeg, gif, bmp, ico, webp, avif` cùng audio/archive.
Khi agent gọi `Read("screenshot.png")`, tool bail với
`Cannot read binary file (png). Use appropriate tools for binary
analysis.` (line 121-124). Không có "appropriate tool" nào khác —
agent bó tay với mọi image task:

- "Check màu nền của logo này" → fail
- "Có bug gì trên UI screenshot không" → fail
- "OCR file scan.png" → fail (phải dùng Bash tesseract workaround)

User paste image qua Cmd+V đã hoạt động (RFC 0003), nhưng agent
không thể tự đọc image từ filesystem trong turn sau đó.

### Evidence

- `ContentBlock::ToolResult.content` (core/types.rs:36-43) là
  `String` — không carry được image block.
- Anthropic Messages API hỗ trợ `tool_result.content: Array<
  {type:"text"|"image",...}>` (per-block) nhưng Luma adapter
  `provider/protocol/anthropic.rs:664-679` luôn gửi string.
- OpenAI Responses API hỗ trợ `function_call_output.output:
  Array<{type:"input_text"|"input_image",...}>` (codex
  `protocol/src/models.rs:1116-1128`) nhưng Luma adapter chỉ gửi
  text.
- OpenAI Chat Completions KHÔNG hỗ trợ image trong
  `ChatCompletionToolMessageParam` — giới hạn upstream, không thể
  workaround sạch.

### Case study: codex + claude-code

**codex-rs** (`core/src/tools/handlers/view_image.rs`) cung cấp
tool dedicated `view_image`:
- Check `turn.model_info.input_modalities.contains(Image)` trước
  khi chạy, bail với message "view_image is not allowed because
  you do not support image inputs".
- Return `FunctionCallOutputBody::ContentItems(vec![InputImage {
  image_url, detail }])` — native Responses API.

**claude-code** (`src/tools/FileReadTool/FileReadTool.ts:866-891`)
inline image handling trong `Read`:
- Detect extension → `readImageWithTokenBudget()` → resize theo
  token budget qua sharp.
- `mapToolResultToToolResultBlockParam` trả `tool_result.content:
  [{type:'image', source:{type:'base64',...}}]` — native Anthropic.
- Kèm `newMessages` metadata (dimensions) qua user meta message.

Hai pattern khác nhau: dedicated tool (codex) vs inline (claude-code).

### Vì sao workaround nhỏ không đủ

- **Thêm `ViewImage` tool riêng**: agent phải học 2 tool, schema
  description phải giải thích khi nào dùng cái nào. Khi mở rộng
  sang PDF, notebook, audio → tiếp tục đẻ tool mới.
- **Chỉ giảm `BINARY_EXTENSIONS`**: Read vẫn không biết trả image
  block vì data model `ToolResult.content: String` không cho phép.
- **Synthesize user message với image sau tool_result**: hack, đẩy
  message count cao, confuse model vì thấy "user" message xen giữa
  assistant turns, session replay phức tạp.

Cần fix ở data model + tool contract, không patch.

## Guide-level explanation

Sau RFC, agent workflow với image tự nhiên như text:

```
Agent → Read(path="assets/logo.png")

Vision model (Claude Sonnet / GPT-5):
  → Text: "Image: image/png 512×512 45 KB (attached)"
  → + Image block với bytes → model "nhìn" được
  → Agent có thể mô tả màu, layout, OCR text trong ảnh

Non-vision model (Kimi / DeepSeek):
  → Text: "Image: image/png 512×512 45 KB. This model does not
     support image input — describe to user or use Bash/OCR for
     text extraction."
  → Agent biết phải báo lại user hoặc thử workaround
```

User không thay đổi gì. Agent không đổi schema. Chỉ có
behavior: Read không bail trên image nữa.

Catalog `models.catalog.json` thêm capability flag:

```json
{
  "id": "claude-sonnet-4-6",
  "source": "anthropic",
  "capabilities": ["vision"]
}
```

Model nào không có flag → text-only, fallback metadata tự động.

Khi gateway là **OpenAI Chat Completions** (OpenRouter, một số
proxy), image vẫn không gửi được do giới hạn protocol upstream.
Luma trả metadata + note:

```
Image: image/png 512×512 45 KB. Image attachment omitted — this
gateway uses OpenAI Chat Completions protocol which does not
support images in tool results. Switch to an Anthropic or
Responses-compatible gateway to enable image reading.
```

Reader rời section này hiểu: Read "just works", capability detection
tự động, gateway limitations transparent.

## Reference-level explanation

### Data model

`ContentBlock::ToolResult` (`src/core/types.rs:36-43`) gains one
field:

```rust
ToolResult {
    tool_use_id: String,
    content: String,
    /// Image attachments produced by the tool. Empty for most tools.
    /// Each entry references an image saved in the session image
    /// store; the provider `ImageResolver` resolves id → base64
    /// at send time (same mechanism as user-attached images).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ToolResultImage>,
    is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_id: Option<String>,
},

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultImage {
    pub media_type: String,
    /// Session-scoped id, resolved via ImageResolver at send time.
    /// Same format as ContentBlock::Image.id.
    pub id: String,
}
```

Old session files: `images` MUST default to empty via
`#[serde(default)]`. No migration required.

### Tool contract

`ToolExecution` (`src/core/tool.rs:17-21`) gains matching field:

```rust
pub struct ToolExecution {
    pub result: String,
    pub artifact: Option<FileChangeArtifact>,
    /// Images produced by the tool (e.g. Read of an image file).
    /// Empty for tools that produce text only.
    pub images: Vec<ToolResultImage>,
}
```

`Tool::execute` signature extends with `ModelCaps`:

```rust
pub struct ModelCaps {
    pub vision: bool,
    // Future: pub pdf: bool, pub audio: bool
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn execute(
        &self,
        args: serde_json::Value,
        output_tx: mpsc::Sender<String>,
        cancel: CancellationToken,
        caps: ModelCaps,
    ) -> Pin<Box<dyn Future<Output = Result<ToolExecution>> + Send + '_>>;
}
```

Existing tools that don't care about caps MUST accept and ignore
the parameter — no behavior change.

### Model catalog

`ModelEntry` (`src/config/models.rs:12-22`) gains:

```rust
pub struct ModelEntry {
    pub id: String,
    pub source: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub capabilities: Vec<String>,  // NEW — e.g. ["vision"]
}
```

`models.catalog.json` — initial annotations:

| Model | capabilities |
|-------|-------------|
| `claude-haiku-4-5*`, `claude-sonnet-4*`, `claude-opus-4*` | `["vision"]` |
| `claude-sonnet-4-5-20250929` | `["vision"]` |
| `gpt-5*`, `gpt-4o*` | `["vision"]` |
| `o1*`, `o3*` | `["vision"]` |
| Kimi, DeepSeek, MiniMax text-only | `[]` (default) |

`ModelEntry::caps()` convenience method:

```rust
impl ModelEntry {
    pub fn caps(&self) -> ModelCaps {
        ModelCaps {
            vision: self.capabilities.iter().any(|c| c == "vision"),
        }
    }
}
```

### Read tool branching

`src/tool/read.rs`:

1. Remove image extensions from `BINARY_EXTENSIONS` (png, jpg,
   jpeg, gif, bmp, ico, webp, avif). Audio/video/archive stay.
2. Add branching before the binary-extension bail:

```rust
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

if let Some(ext) = path.extension().and_then(|e| e.to_str())
    && IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
{
    return read_image(&path, ext, &caps).await;
}
```

```rust
async fn read_image(
    path: &Path,
    ext: &str,
    caps: &ModelCaps,
) -> Result<ToolExecution> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_IMAGE_BYTES {
        bail!(
            "Image too large ({:.1} MB, max {} MB). \
             Resize or crop before reading.",
            meta.len() as f64 / 1_048_576.0,
            MAX_IMAGE_BYTES / 1_048_576,
        );
    }
    let data = fs::read(path)?;
    let (media_type, _) = detect_image_format(&data);
    let (w, h) = parse_png_dimensions(&data).unwrap_or((0, 0));
    let size_kb = data.len().div_ceil(1024);
    let dim = if w > 0 { format!("{w}×{h} ") } else { String::new() };

    if !caps.vision {
        return Ok(ToolExecution {
            result: format!(
                "{media_type} image: {dim}{size_kb} KB. \
                 This model does not support image input — describe \
                 the contents to the user or use Bash/OCR tools for \
                 text extraction.",
            ),
            artifact: None,
            images: vec![],
        });
    }

    let session_id = crate::core::session::current_session_id()
        .ok_or_else(|| anyhow!("no active session"))?;
    let id = crate::core::session::save_image(&session_id, &data, ext);

    Ok(ToolExecution {
        result: format!("{media_type}: {dim}{size_kb} KB (attached)"),
        artifact: None,
        images: vec![ToolResultImage {
            media_type: media_type.to_owned(),
            id,
        }],
    })
}
```

`parse_png_dimensions` = existing PNG-header-offset reader
(deferred — OK to return `None` for non-PNG in phase 1).

### Agent turn propagation

`src/core/agent/turn.rs` where tool execution result is pushed
into `ToolResult` block: propagate `exec.images`:

```rust
result_blocks.push(ContentBlock::ToolResult {
    tool_use_id: id,
    content: text,
    images: exec.images,  // NEW
    is_error: exec.is_err,
    evidence_id,
});
```

### Provider serialization

#### Anthropic (native)

`src/provider/protocol/anthropic.rs:664-679`:

```rust
ContentBlock::ToolResult {
    tool_use_id, content, images, is_error, ..
} => {
    let content_json = if images.is_empty() {
        // Backward: plain string content
        serde_json::json!(content)
    } else {
        // Multi-block content with text + images
        let mut blocks: Vec<serde_json::Value> = Vec::new();
        if !content.is_empty() {
            blocks.push(serde_json::json!({
                "type": "text",
                "text": content
            }));
        }
        for img in images {
            let data = resolve(&img.id);
            if data.is_empty() { continue; }
            blocks.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": data,
                }
            }));
        }
        serde_json::json!(blocks)
    };
    let mut v = serde_json::json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content_json,
    });
    if *is_error {
        v["is_error"] = serde_json::json!(true);
    }
    Some(v)
}
```

Backward: empty `images` preserves exact current wire format.

#### OpenAI Responses (native)

`src/provider/protocol/openai_responses.rs` at `ContentBlock::
ToolResult` site:

```rust
if images.is_empty() {
    // Plain text output — unchanged
    serde_json::json!({
        "type": "function_call_output",
        "call_id": tool_use_id,
        "output": content,
    })
} else {
    let mut items: Vec<serde_json::Value> = Vec::new();
    if !content.is_empty() {
        items.push(serde_json::json!({
            "type": "input_text",
            "text": content
        }));
    }
    for img in images {
        let data = resolve(&img.id);
        if data.is_empty() { continue; }
        items.push(serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", img.media_type, data),
        }));
    }
    serde_json::json!({
        "type": "function_call_output",
        "call_id": tool_use_id,
        "output": items,
    })
}
```

#### OpenAI Chat Completions (degraded)

`src/provider/protocol/openai_chat.rs:367-380`: protocol does not
support image in tool message. Append a note to the text content
so the model knows content was stripped:

```rust
ContentBlock::ToolResult {
    tool_use_id, content, images, ..
} => {
    let effective_content = if images.is_empty() {
        content.clone()
    } else {
        format!(
            "{content}\n\n[{n} image attachment(s) omitted — this \
             gateway uses OpenAI Chat Completions which does not \
             support images in tool results; switch to Anthropic or \
             OpenAI Responses gateway to view images.]",
            n = images.len()
        )
    };
    out.push(serde_json::json!({
        "role": "tool",
        "content": effective_content,
        "tool_call_id": tool_use_id,
    }));
}
```

MUST NOT synthesize a follow-up user message carrying the image —
that pattern inflates context, confuses session replay, and is
rejected here.

### Capability plumbing

Agent turn loop (`core/agent/turn.rs`) reads `ModelCaps` from the
current binding and passes into each `tool.execute()`:

```rust
let caps = self.binding.model.caps();
let exec = tool.execute(args, output_tx, cancel, caps).await?;
```

Tools that don't care ignore the parameter.

### Session & transcript

Session files (`session.json`) serialize `ToolResult.images: []`
empty by default — existing sessions load unchanged. New sessions
with images serialize the extra field; older Luma version reading
new session file drops unknown `images` field (serde default =
empty) and renders text only. Acceptable under beta stance.

### TUI render

`src/tui/block/*` currently renders `ToolResult.content` as text.
Phase 1: append `[{n} image(s) attached]` suffix when
`!images.is_empty()`. Full image chip render deferred — Kitty/
iTerm2 inline image protocol is future work.

### Test plan

| Layer | Test |
|-------|------|
| Types | `ToolResult` serde roundtrip with empty + non-empty images |
| Types | Old session JSON (no images field) deserializes |
| Read | `read_image` with vision caps returns `images.len() == 1` |
| Read | `read_image` without vision caps returns text only, clear message |
| Read | Image > 5MB bails with size message |
| Read | Non-image extension unchanged behavior |
| Read | Removed image extensions no longer hit BINARY_EXTENSIONS bail |
| Anthropic | ToolResult with 0 images → string content (wire compat) |
| Anthropic | ToolResult with image → array content with image block |
| Responses | ToolResult with 0 images → string output |
| Responses | ToolResult with image → array with `input_image` |
| Chat | ToolResult with image → text + omitted note |
| Catalog | Claude/GPT models load with `capabilities: ["vision"]` |
| Catalog | Missing `capabilities` field defaults to empty |
| Turn | Exec images propagate into ToolResult block |

### Migration plan

**PR1** (data model + Anthropic):
1. Add `ToolResultImage` struct and `images` field to `ToolResult`.
2. Add `images` to `ToolExecution`.
3. Add `ModelCaps` + extend `Tool::execute` signature.
4. Update all existing tools to accept & ignore `caps` parameter
   and return `images: vec![]`.
5. Anthropic adapter: serialize array content when images present.
6. Tests for types + Anthropic wire.

**PR2** (Read branching + catalog):
1. Remove image extensions from `BINARY_EXTENSIONS`.
2. Add `read_image` branch in `ReadTool::execute`.
3. Add `capabilities` to `ModelEntry` + `ModelEntry::caps()`.
4. Annotate `models.catalog.json` with vision flags.
5. Turn loop passes `caps` into tool execution.
6. Tests: Read with/without vision, size limit, extension removal.

**PR3** (OpenAI Responses + Chat):
1. Responses adapter: array output when images present.
2. Chat adapter: text note when images present.
3. Tests for both protocols.

Each PR independent, mergeable, shippable.

### Rollback

Each PR revertable individually. No persistent state migration —
old Luma version reads new session files with `images` dropped
(text content only), new Luma reads old files with empty images
default. No user-facing breakage on downgrade.

## Drawbacks

- **`Tool::execute` signature breaking**: every tool gets a new
  parameter. ~10 tools affected. Simple mechanical change but
  unavoidable churn.
- **`ToolExecution` struct growth**: adds `images: Vec<_>` — small
  memory cost per result, acceptable.
- **OpenAI Chat degradation**: vision-capable model behind Chat
  gateway silently drops image bytes. User sees note but may not
  know why. Mitigation: doc and metadata note clear.
- **Session file format drift**: adds optional field. Old Luma
  reading new session → field dropped, model loses image context
  on replay. Accepted under beta stance.
- **Catalog annotation burden**: every new model must be tagged
  with capabilities or defaults to text-only (conservative default
  — may miss vision support silently).
- **No image resize**: large images eat tokens. 5MB limit is
  generous (Anthropic rejects >5MB anyway). Future RFC may add
  resize when `image` crate justified.

## Rationale and alternatives

### Vì sao chọn capability-aware inline Read

- **1 tool, 1 mental model**: agent biết `Read(path)` hoạt động
  cho mọi file. Không học tool mới.
- **Capability detection tự động**: text-only model không hallucinate
  image analysis — rõ ràng rằng image không được gửi.
- **Extensible**: pattern tương tự cho PDF (`pdf` capability),
  notebook (ipynb đã có riêng), audio tương lai.
- **Match user intent**: "read the file" là universal primitive.

### Alternative 1: Dedicated `ViewImage` tool (codex-style)

- Pro: isolation, không động đến Read.
- Con: agent phải học 2 tool + khi nào dùng cái nào.
- Con: mở rộng PDF/notebook → thêm tool nữa → tool explosion.
- Con: non-vision model vẫn thấy `view_image` trong schema → có
  thể gọi nhầm rồi bị bail.
- **Loại**: mental overhead cho agent không bù được isolation.

### Alternative 2: Inline trong Read nhưng luôn attach image, không check caps

- Pro: đơn giản nhất, caller tự xử lý.
- Con: text-only provider nhận image block → error 400.
- Con: token waste nếu model không hiểu.
- **Loại**: capability check là correctness, không phải polish.

### Alternative 3: Synthesize follow-up user message cho OpenAI Chat

- Pro: image bytes vẫn tới model dù gateway không hỗ trợ tool
  result image.
- Con: context inflate (2x message count per image call).
- Con: session replay phức tạp — re-synthesize? Lưu sẵn?
- Con: model có thể confuse role của user message xen giữa.
- Con: codex KHÔNG dùng pattern này — bằng chứng không cần thiết.
- **Loại**: giá trị marginal, complexity lớn.

### Alternative 4: Không làm gì

- Agent tiếp tục fail trên image task.
- User phải describe image bằng text hoặc OCR manual.
- **Loại**: vision là capability cốt lõi của model hiện đại,
  Luma block nó là regression so với claude-code/codex.

### Alternative 5: Chỉ fix Read branching, không đụng data model

- Return text "image attached" nhưng không có image block.
- Pro: minimal change.
- Con: model không thực sự "nhìn" được ảnh — câu trả lời chỉ là
  echo của metadata text.
- **Loại**: fix nửa vời, không giải quyết pain point gốc.

## Prior art

- **openai/codex** (`codex-rs/core/src/tools/handlers/view_image.rs`):
  dedicated tool, Responses API native, capability gate via
  `input_modalities.contains(Image)`, data URL encoding.
- **yasasbanukaofficial/claude-code** (`src/tools/FileReadTool/
  FileReadTool.ts`): inline trong FileRead, Anthropic only, resize
  aggressive via sharp.
- **Anthropic Messages API docs**: `tool_result.content` accepts
  array of blocks including `image` — used byclaude-code and
  directly here.
- **OpenAI Responses API docs**: `function_call_output.output`
  accepts array with `input_image` — used by codex and directly
  here.
- **Cursor / Zed**: image attachment qua UI, không qua agent-
  initiated read. Different use case.

## Unresolved questions

- **Q1**: Image resize trước khi gửi?
  - Default: không resize trong phase 1. Giới hạn file size 5MB.
  - Follow-up RFC nếu cần khi có evidence token waste.
- **Q2**: PDF capability gộp vào RFC này hay tách?
  - Default: tách. RFC này scope là image only. Pattern
    capability-aware đặt nền cho PDF RFC sau.
- **Q3**: Non-PNG dimension parsing (JPEG SOF markers)?
  - Default: bỏ qua dimension cho non-PNG trong phase 1. Text
    metadata vẫn có size, model không lose nhiều.
- **Q4**: `Read` có cache image đã đọc trong 1 session không?
  - Default: không cache ở tool level. Nếu model gọi lại cùng path,
    save lại image mới. Session store cleanup là concern riêng.
- **Q5**: Tool schema description có mention image support?
  - Default: thêm 1 dòng "Images are returned as visual attachments
    to the model when vision is available." Agent biết capability
    mà không cần probe.

## Future possibilities

- **PDF support**: extend `ModelCaps { pdf }`, Read branches cho
  `.pdf` → Anthropic PDF block / Responses file upload.
- **Notebook first-class**: Luma chưa có `.ipynb` branch; Read có
  thể extract cells theo claude-code pattern.
- **Image resize theo token budget**: thêm `image` crate khi có
  bottleneck evidence.
- **Kitty/iTerm2 inline image render**: TUI hiện preview text
  metadata; tương lai render ảnh thật trong terminal hỗ trợ.
- **Image generation tool**: `GenerateImage` tool output quay lại
  qua cùng `ToolResultImage` path.
- **Code-mode / MCP tools** tự định nghĩa image output → dùng
  chung `ToolResultImage` shape.

## Implementation status

Implemented in 3 PRs merged on 2026-04-13:

- **PR1 — data model + Anthropic**: `ToolResultBody { Text, Items }`,
  `ToolResultItem { Text, Image }` in `core/types.rs`. Anthropic adapter
  serializes native per-block array. OpenAI Chat + Responses degraded
  (flatten + note) pending PR3. Tests: 12 new, 599 total pass.

- **PR2 — ModelCaps + Read branching + catalog**: `ModelCaps { vision }`
  threaded through `Tool::execute`. `AgentConfig.capabilities` /
  `ModelEntry.capabilities` / catalog annotations (Claude 3/4/4.5/4.6,
  GPT-5.x, codex-mini, MiMo-V2-Omni = vision). `Read` branches by
  extension + vision caps: image → attach via session image store;
  non-vision → metadata text. `MAX_IMAGE_BYTES = 5 MB` guard. PNG
  dimension parsing. Tests: 5 new, 604 total pass.

- **PR3 — Responses native**: OpenAI Responses serializes `ToolResultBody`
  as `input_text` + `input_image` (data URL) natively. Chat retains
  degraded note. Tests: 3 new, 607 total pass.

All three PRs pass `cargo build`, `cargo clippy -- -D warnings`, and
`cargo test`. No clippy allows, no session migration required.
