# RFC 0011: Image Attachment Routing

| Field          | Value       |
| -------------- | ----------- |
| RFC            | 0011        |
| Title          | Image Attachment Routing |
| Status         | Draft       |
| Author(s)      | —           |
| Created        | 2026-04-15  |
| Updated        | 2026-06-15  |
| Tracking issue | N/A         |
| Supersedes     | N/A         |
| Superseded by  | N/A         |

## Summary

Chuẩn hóa một đường duy nhất cho ảnh xuyên provider: mọi ảnh mà model cần
"nhìn thấy" MUST được biểu diễn như một `ToolResultItem::Image { id, media_type }`
chuẩn, được lưu trong session image store, rồi runtime/provider adapter sẽ
serialize qua wire path phù hợp nhất cho provider hiện tại. `Read` khi đọc
file ảnh MUST tạo item này thay vì coi ảnh như text-only tool output hoặc
buộc mọi provider phải hỗ trợ image-in-tool-result.

Ngoài routing, RFC này cũng chuẩn hóa **image preprocessing pipeline**: ảnh
MUST được resize và nén về trong giới hạn kích thước trước khi lưu vào store,
thay vì bail khi ảnh quá lớn như hiện tại.

## Motivation

### Trạng thái hiện tại

Codebase đã có nền tảng:

- `ToolResultItem::Image { media_type, id }` tồn tại trong `core/types.rs`.
- `save_image` / `read_image_base64` / `image_resolver` trong `core/session.rs`
  lưu bytes vào `sessions/{id}/images/img_{ts_hex}.{ext}` và trả về base64
  qua `ImageResolver = dyn Fn(&str) -> String`.
- `StreamRequest` mang `resolve_image: &ImageResolver` để provider adapter
  dùng khi serialize.

Provider adapters đã xử lý `ToolResultItem::Image` theo wire format riêng:

- **Anthropic** (`protocol/anthropic.rs`): inline image block base64 trong
  tool-result content.
- **OpenAI Responses** (`protocol/openai_responses.rs`): `input_image` với
  `image_url = data:...`.
- **Kiro** (`protocol/kiro.rs`): `extract_tool_results` chỉ lấy
  `ToolResultItem::Text`, bỏ qua `Image` items hoàn toàn. Ảnh từ user turn
  đi qua `msg_images` → `userInputMessage.images[].{format, source.bytes}`.

### Vấn đề

1. `read_image` trong `tool/read.rs` bail khi ảnh > 5 MB thay vì resize.
   Comment trong code ghi rõ: *"Resize is deferred to a future RFC"*.

2. Kiro không có official image tool-result path (`ToolResultContentBlock =
   text | json`). `ToolResultItem::Image` từ `Read` bị silently drop ở
   `extract_tool_results`. Model Kiro không nhìn thấy ảnh từ `Read`.

3. Hành vi `Read("a.png")` lệch giữa provider:
   - Claude/OpenAI: model nhìn thấy ảnh qua tool-result image block.
   - Kiro: model không nhìn thấy ảnh.

4. Không có preprocessing pipeline: ảnh > 5 MB bị reject thay vì resize.
   Không có coordinate mapping metadata khi ảnh bị scale.

## Guide-level explanation

Sau RFC này:

**`Read` đọc ảnh:**
- Đọc bytes, chạy qua preprocessing pipeline (resize + nén nếu cần).
- Lưu bytes đã xử lý vào session image store.
- Trả về `ToolResultItem::Text` (metadata) + `ToolResultItem::Image { id, media_type }`.
- Không thay đổi gì về transcript — `tool_use → tool_result` contract giữ nguyên.

**Runtime routing (request assembly):**
- Provider hỗ trợ image tool-result (Claude, OpenAI): inline image block trong
  tool-result content như hiện tại.
- Provider không hỗ trợ image tool-result nhưng hỗ trợ user image attachment
  (Kiro): runtime promote `ToolResultItem::Image` sang `userInputMessage.images`
  ở bước request assembly, không thay đổi transcript.
- Provider không hỗ trợ bất kỳ image path nào: degrade về text-only metadata,
  log warning.

**Preprocessing pipeline:**
- Ảnh đã trong giới hạn → pass through.
- Ảnh vượt dimension limit (2000×2000) → resize `fit: inside`.
- Ảnh vượt payload limit (3.75 MB raw / 5 MB base64) → nén theo format.
- Nếu tất cả fail → bail với error rõ ràng.
- Khi resize xảy ra, metadata ghi lại `original_dims` để model có thể map
  tọa độ ngược lại.

## Reference-level explanation

### 1. Preprocessing pipeline

`tool/read.rs` MUST thay thế hard bail bằng pipeline sau, theo thứ tự:

1. **Empty check**: bail nếu 0 bytes.
2. **Dimension check** (PNG: parse IHDR; JPEG/WebP: parse header nếu có):
   nếu `width > 2000 || height > 2000`, resize về `fit: inside, 2000×2000,
   withoutEnlargement: true`.
3. **Payload check**: nếu `bytes.len() > IMAGE_TARGET_RAW_BYTES` (3.75 MB):
   - Thử nén theo format gốc (PNG compression level 9, JPEG/WebP quality 80).
   - Nếu vẫn quá lớn, thử resize xuống 75% → 50% → 25% dimensions.
   - Nếu vẫn quá lớn, convert sang JPEG quality 80 (last resort).
   - Nếu tất cả fail → bail.
4. Lưu bytes đã xử lý vào store.

Hai giới hạn cần định nghĩa trong `core/types.rs` hoặc một constants module:

```rust
pub const IMAGE_MAX_DIMENSION: u32 = 2000;
pub const IMAGE_TARGET_RAW_BYTES: usize = 3 * 1024 * 1024 + 768 * 1024; // 3.75 MB
pub const IMAGE_MAX_BASE64_BYTES: usize = 5 * 1024 * 1024; // 5 MB
```

Khi resize xảy ra, `ToolResultItem::Text` SHOULD bao gồm:

```
image/png: 3840×2160 → 2000×1125 (scale 1.92×), 1.2 MB (attached)
```

Để model biết cần nhân tọa độ khi dùng ảnh cho computer-use hoặc coordinate
reference.

### 2. `ToolResultItem::Image` — không thay đổi struct

Struct hiện tại đã đủ:

```rust
pub enum ToolResultItem {
    Text { text: String },
    Image { media_type: String, id: String },
}
```

`id` là filename trong session image store (`img_{ts_hex}.{ext}`), được
`ImageResolver` dùng để đọc lại bytes. Không cần thêm field.

### 3. Capability model

`core/provider.rs` MUST thêm hai capability flags:

```rust
pub struct ProviderCaps {
    pub vision: bool,                  // đã có ở ModelCaps
    pub tool_result_image: bool,       // provider nhận image trong tool-result
    pub user_image_attachment: bool,   // provider nhận image ở user turn
}
```

Mapping mặc định:

| Provider       | `tool_result_image` | `user_image_attachment` |
| -------------- | ------------------- | ----------------------- |
| Anthropic      | true                | true                    |
| OpenAI Responses | true              | true                    |
| Kiro           | false               | true                    |
| OpenAI Chat    | false               | true                    |

### 4. Runtime routing — request assembly

Khi provider có `tool_result_image = false` và `user_image_attachment = true`,
request assembly MUST:

1. Scan `ToolResultBody::Items` trong user turn hiện tại tìm `ToolResultItem::Image`.
2. Resolve bytes qua `ImageResolver`.
3. Append vào `userInputMessage.images` (Kiro) hoặc tương đương.
4. Giữ nguyên `ToolResultItem::Text` trong tool-result content.
5. Không xóa `ToolResultItem::Image` khỏi transcript — chỉ bỏ qua khi
   serialize wire format.

Logic này xảy ra ở bước `build_request_body` / `build_current_message` của
từng provider adapter, không ở transcript layer.

Kiro cụ thể: `extract_tool_results` hiện tại đã bỏ qua `Image` items (đúng).
Cần thêm bước collect images từ tool results và merge vào `userInputMessage.images`
cùng với images từ user content blocks.

### 5. Provider adapters

**Anthropic** (`protocol/anthropic.rs`): không thay đổi — đã xử lý
`ToolResultItem::Image` đúng.

**OpenAI Responses** (`protocol/openai_responses.rs`): không thay đổi — đã
xử lý `ToolResultItem::Image` đúng.

**Kiro** (`protocol/kiro.rs`): `build_current_message` và `build_history`
MUST collect `ToolResultItem::Image` từ tool-result turns và append vào
`userInputMessage.images` cùng với `msg_images`. Thứ tự: user content images
trước, tool-result images sau.

**OpenAI Chat** (`protocol/openai_chat.rs`): nếu không hỗ trợ image tool-result,
áp dụng cùng pattern như Kiro.

### 6. Session image store — không thay đổi layout

Layout hiện tại giữ nguyên:

```
sessions/{session_id}/images/img_{ts_hex}.{ext}
```

`save_image` trả về filename (= `id`). `image_resolver` wrap thành closure
`Fn(&str) -> String`. Không cần thay đổi.

Cleanup: session image dir bị xóa khi session kết thúc hoặc khi app khởi
động lại (cleanup sessions cũ). Không cần thêm GC logic.

### 7. Degradation khi provider không hỗ trợ vision

Nếu `ProviderCaps { vision: false, tool_result_image: false, user_image_attachment: false }`:

- `read_image` MUST trả về text-only metadata (đã implement).
- Runtime MUST NOT gọi `ImageResolver` cho provider này.
- Runtime SHOULD log warning một lần per session khi gặp `ToolResultItem::Image`
  bị drop.

### 8. Migration plan

1. Thêm `IMAGE_MAX_DIMENSION`, `IMAGE_TARGET_RAW_BYTES`, `IMAGE_MAX_BASE64_BYTES`
   constants.
2. Implement preprocessing pipeline trong `tool/read.rs` (thay thế hard bail).
3. Thêm `ProviderCaps` với `tool_result_image` và `user_image_attachment`.
4. Update Kiro adapter: collect tool-result images → `userInputMessage.images`.
5. Update OpenAI Chat adapter nếu cần.
6. Xóa `MAX_IMAGE_BYTES` constant cũ và comment "deferred to future RFC".

### 9. Test plan

- `Read` trên PNG/JPEG nhỏ hơn giới hạn → pass through, trả về metadata + Image item.
- `Read` trên PNG > 2000px → resize, metadata ghi scale factor.
- `Read` trên ảnh > 3.75 MB → nén, metadata ghi kích thước sau nén.
- `Read` trên ảnh không thể nén về dưới giới hạn → bail với error rõ ràng.
- `Read` trên ảnh với model không có vision → text-only metadata, không có Image item.
- Kiro request assembly: `ToolResultItem::Image` từ tool-result xuất hiện trong
  `userInputMessage.images`, không trong `toolResults[].content`.
- Anthropic/OpenAI: `ToolResultItem::Image` vẫn inline trong tool-result content.
- Ảnh corrupt (0 bytes, invalid header) → bail với error rõ ràng.
- Ảnh animated GIF → xử lý như static image (lấy frame đầu hoặc pass through
  nếu trong giới hạn).

## Drawbacks

- Preprocessing pipeline cần image decoding library (hiện tại `read.rs` chỉ
  parse PNG IHDR thủ công). Cần quyết định dùng crate nào (`image`, `fast_image_resize`,
  hay gọi `convert` qua Bash).
- Kiro routing logic phức tạp hơn: phải merge images từ hai nguồn (user content
  và tool results) vào một `images[]` array.
- Trong giai đoạn chuyển tiếp, test coverage cần cover cả hai code path (cũ
  và mới) cho đến khi migration hoàn tất.

## Rationale and alternatives

### Alternative 1: Giữ hard bail, yêu cầu user resize thủ công

Loại vì trái với kỳ vọng sản phẩm. Comment trong code đã ghi rõ đây là
temporary workaround.

### Alternative 2: Promote tool-result thành synthetic user turn cho Kiro

Loại vì vi phạm `tool_use → tool_result` ordering contract và làm transcript
khó debug. Thiết kế được chọn giữ transcript nguyên vẹn, chỉ thay đổi ở
request assembly layer.

### Alternative 3: Thêm `ImageAttachmentRef` struct mới tách khỏi `ToolResultItem`

Không cần thiết. `ToolResultItem::Image { id, media_type }` đã đủ. Thêm struct
mới chỉ tăng complexity mà không giải quyết vấn đề gì thêm.

## Prior art

- `yasasbanukaofficial/claude-code`: `imageResizer.ts` implement pipeline
  tương tự (dimension check → quality reduction → progressive resize → JPEG
  fallback). Constants: `IMAGE_MAX_WIDTH/HEIGHT = 2000`, `IMAGE_TARGET_RAW_SIZE
  = 3.75 MB`, `API_IMAGE_MAX_BASE64_SIZE = 5 MB`. `imageStore.ts` dùng
  integer id với LRU eviction cap 200 entries, cleanup theo session.
- Anthropic Messages API: user/tool-result image blocks base64.
- OpenAI Responses API: `input_image` entries.
- Amazon Q Developer official model: `UserInputMessage.images`,
  `ToolResultContentBlock = text | json` (không có image variant).

## Unresolved questions

- **Image decoding crate**: `image` crate (~2 MB binary size increase) vs
  gọi `convert`/`ffmpeg` qua Bash vs chỉ xử lý PNG/JPEG thủ công. Cần
  quyết định trước khi implement step 2.
- **Animated GIF**: lấy frame đầu hay pass through? Anthropic hỗ trợ GIF
  nhưng Kiro format mapping chỉ có `gif | jpeg | png | webp`.
- **Coordinate mapping**: `ToolResultItem::Text` có nên include scale factor
  dưới dạng structured metadata (JSON) hay plain text? Plain text đơn giản
  hơn nhưng khó parse nếu sau này cần dùng programmatically.

## Future possibilities

- Screenshot, render, chart tools có thể reuse cùng preprocessing pipeline.
- Routing có thể mở rộng cho audio/video nếu project thêm multimodal input.
- `ProviderCaps` có thể mở rộng thêm `max_image_dimension`, `supported_formats`
  để adapter tự điều chỉnh target format khi nén.

## Implementation status

Chưa implement. Blocking item: quyết định image decoding crate (Unresolved
questions #1).
