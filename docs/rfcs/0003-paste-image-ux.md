# RFC 0003: Paste Image UX — Unified, Feedback-First

| Field            | Value                                                |
| ---------------- | ---------------------------------------------------- |
| RFC              | 0003                                                 |
| Title            | Paste Image UX — Unified, Feedback-First             |
| Status           | Draft                                                |
| Author(s)        | Nghia / Luma                                         |
| Created          | 2026-04-13                                           |
| Updated          | 2026-04-13                                           |
| Tracking issue   | N/A                                                  |
| Supersedes       | N/A                                                  |
| Superseded by    | N/A                                                  |

## Summary

Cải thiện UX paste image với 2 thay đổi chính: (1) thêm fallback
empty-bracketed-paste → trigger clipboard image read để giải quyết
case screenshot trên các terminal forward Cmd+V dưới dạng paste rỗng;
(2) thêm loading placeholder chip `[loading image…]` để user có
feedback visual trong ~200-400ms chờ clipboard read. Giữ nguyên raw
`Ctrl+V` / `Cmd+V` handler vì terminal behavior không nhất quán —
Ghostty không gửi paste event khi clipboard không có text, iTerm2
gửi empty paste, Warp xử lý riêng. Raw key + empty-paste fallback
phủ được tất cả case.

Mục tiêu: paste screenshot vào chat MUST hoạt động trên các terminal
phổ biến (iTerm2, Terminal.app, Warp, Ghostty, Windows Terminal,
Alacritty, Kitty), user MUST có visual feedback trong toàn bộ flow,
race giữa raw-key trigger và bracketed-paste MUST không gây
double-attach.

## Motivation

### Vấn đề hiện tại

Code paste image hiện sống ở 3 chỗ, không coordinate với nhau:

- `src/tui/app/dispatch.rs:339-345` — bắt raw `Ctrl+V` keystroke
  (`Modifiers::CONTROL`), gọi `paste_clipboard_image()` async.
- `src/tui/app/dispatch.rs:358-371` — `on_paste()` xử lý bracketed
  paste, branch giữa image path / text.
- `src/tui/prompt/keys.rs:20-37` — `handle_paste()` lo path text vào
  buffer.

### Pain points đã xác định

| # | Tình huống | Behavior hiện tại | Vấn đề |
|---|------------|------------------|--------|
| 1 | Screenshot (Cmd+Shift+4 trên macOS) → Cmd+V | Bracketed paste text rỗng → app drop event, không có gì xảy ra | **Im lặng hoàn toàn**, user tưởng app hỏng |
| 2 | Cmd+V trên macOS có image trong clipboard | Như trên — không có cách nào trigger image read | Không có shortcut |
| 3 | Ctrl+V trên Linux trong terminal có bracketed paste | App bắt key → spawn clipboard read, **rồi** terminal cũng gửi bracketed paste | Race condition, có thể double-attach hoặc nhiễu |
| 4 | Clipboard read mất 200-500ms | Prompt im lặng | User nhấn lại nhiều lần, spawn nhiều task |
| 5 | Drag image file vào terminal | Image attached, không có toast | OK nhưng không confirm |

### Evidence

- macOS Terminal/iTerm2 intercept Cmd+V trước khi đến app, **chỉ**
  gửi bracketed paste — app không bao giờ nhận `KeyEvent(V, SUPER)`
  cho thao tác paste thông thường. Code bắt `Modifiers::CONTROL` ở
  `dispatch.rs:340` chưa bao giờ chạy trên macOS qua Cmd+V.
- `read_clipboard_image()` trên macOS dùng `osascript` + temp file
  (`src/tui/app/agent.rs:262-280`), latency ~200-400ms. Trong khoảng
  này UI không có signal nào.
- Khi clipboard chứa raw image (PNG bytes, không phải text), terminal
  vẫn emit bracketed paste với content rỗng hoặc binary garbage —
  `on_paste()` hiện tại return `Action::Continue` và bỏ qua.

### Prior art: codex-rs

`openai/codex` (`codex-rs/tui/src/chatwidget.rs:4970`) cũng giữ raw
key handler `Ctrl|Alt + 'v'` gọi `paste_image_to_temp_png()`. Empty
bracketed paste không được dùng làm trigger. Pattern này được
validated trong production, RFC này theo cùng hướng + thêm empty
paste làm fallback bù cho terminal không forward Ctrl+V.

### Vì sao workaround nhỏ không đủ

- Bỏ Ctrl+V dùng empty-paste duy nhất: Ghostty và một số terminal
  không emit event khi clipboard chỉ có image, user mất hoàn toàn
  cách paste.
- Slash command `/paste`: giải quyết được pain point #1 nhưng user
  phải học shortcut mới, không khớp muscle memory.
- State machine + timeout chống double: id-based placeholder match
  đơn giản hơn, scale tốt hơn cho concurrent paste.

## Guide-level explanation

Sau RFC, user paste image với cùng một thao tác như paste text:

```
1. Cmd+Shift+4 → screenshot (clipboard có raw PNG)
2. Trong luma TUI: Cmd+V (macOS) hoặc Ctrl+Shift+V (Linux)
3. Prompt hiển thị ngay: > before [loading image…] cursor
4. ~300ms sau, placeholder thay bằng: > before [png 1920×1080 245KB] cursor
5. Toast trên doc: "attached: image (245 KB)"
6. Enter để gửi
```

Nếu clipboard không có image:

```
1. Cmd+V với clipboard rỗng / non-image
2. Placeholder [loading image…] xuất hiện 300ms
3. Placeholder biến mất, doc info: "no image in clipboard"
```

Paste text/path vẫn hoạt động không thay đổi — vì terminal gửi
bracketed paste với content khác rỗng:

```
- Paste "hello world"  → insert text như cũ
- Paste "/path/img.png" → load file → image chip
- Paste 5MB code       → paste block chip như cũ
```

Drag image file vào terminal → đường dẫn được paste (bracketed) →
detect là image path → load → chip. Thêm toast confirm.

Reader đọc code mới sẽ thấy: chỉ còn 1 entry point `on_paste()` xử
lý mọi paste case, phân nhánh dựa trên content (rỗng / image path /
text). Không còn raw key handler riêng.

## Reference-level explanation

### Data model

Thêm variant `LoadingImage` vào `Seg`:

```rust
// src/tui/prompt/buffer.rs
pub enum Seg {
    Text(String),
    Image { media_type: String, data: Vec<u8> },
    Paste(String),
    LoadingImage(u32),  // id để match khi clipboard read xong
}
```

`LoadingImage` MUST render như chip `[loading image…]` với spinner
animation (reuse existing tick mechanism). Khi user nhấn Backspace
sát chip, MUST xóa được như Image chip — đồng thời SHOULD signal
agent layer để cancel pending clipboard read nếu task chưa xong (qua
`CancellationToken` riêng cho mỗi paste).

### App state

Thêm vào `AgentHandle` (`src/tui/app/state.rs`):

```rust
pub struct AgentHandle {
    // ... existing fields
    /// Monotonic counter để gán id cho mỗi clipboard read in-flight.
    /// Wrapping_add — collision sau 2^32 paste là acceptable.
    pub next_paste_id: u32,
}
```

KHÔNG thêm `PasteState` enum hay timeout — vì id-based matching đã
giải quyết race tự nhiên: mỗi paste có placeholder riêng, mỗi result
event resolve đúng placeholder của nó.

### Event

Sửa variant trong `src/event.rs`:

```rust
pub enum Event {
    // ... existing
    /// Async clipboard image result. `id` matches the LoadingImage
    /// placeholder inserted by paste_clipboard_image().
    ClipboardImage {
        id: u32,
        result: Option<(String, Vec<u8>)>,
    },
}
```

Breaking change từ `ClipboardImage(Option<(String, Vec<u8>)>)`. Migration
chỉ cần update 1 emit site và 1 handle site.

### Dispatch

Giữ raw key handler hiện tại — chỉ thêm empty-paste fallback và
dedupe-by-id để chống double trigger:

```rust
// dispatch.rs — on_key() giữ nguyên:
if key.code == KeyCode::Char('v')
    && key.modifiers.contains(Modifiers::CONTROL)
    && !self.ui.picker.is_active
{
    self.paste_clipboard_image();
    return Action::Render;
}
```

Sửa `on_paste()` thêm fallback empty paste:

```rust
pub(super) fn on_paste(&mut self, text: String) -> Action {
    crate::dbg_log!("paste: {}B", text.len());

    // Empty bracketed paste = clipboard had non-text content
    // (typically a raw image from a screenshot). Trigger the same
    // async clipboard read as Ctrl+V. Dedupe via paste id — if
    // raw Ctrl+V already spawned a read, the placeholder for that
    // id is already in flight; calling paste_clipboard_image()
    // again creates a second placeholder + read, which is fine
    // (one will resolve, the other returns None and is removed).
    if text.is_empty() {
        self.paste_clipboard_image();
        return Action::Render;
    }

    if let Some(path) = extract_image_path(&text) {
        self.paste_image_file(&path);
    } else if self.ui.prompt.handle_paste(text).is_none() {
        self.doc
            .warn("paste too large (>1 MB) — use a file reference instead");
    }
    Action::Render
}
```

Trường hợp double trigger (Ctrl+V + empty paste cùng phát sinh):
2 placeholder, 2 clipboard read. Cả 2 sẽ trả về cùng image data
(clipboard không đổi giữa 2 read sát nhau). User thấy 2 image chip
liên tiếp — acceptable trade-off cho code đơn giản. Nếu cần tối
ưu sau này, có thể debounce bằng instant-based throttle (skip
`paste_clipboard_image()` nếu đã gọi trong <100ms).

### Agent layer

```rust
// src/tui/app/agent.rs
pub(super) fn paste_clipboard_image(&mut self) {
    if is_ssh_session() {
        self.doc
            .info("image paste not supported over SSH — use a file path instead");
        return;
    }
    let Some(tx) = self.tx.clone() else { return };

    let id = self.agent.next_paste_id;
    self.agent.next_paste_id = self.agent.next_paste_id.wrapping_add(1);
    self.ui.prompt.attach_loading_image(id);

    tokio::task::spawn_blocking(move || {
        let result = read_clipboard_image().map(|data| {
            let (media_type, _) = detect_image_format(&data);
            (media_type.to_owned(), data)
        });
        let _ = tx.blocking_send(Event::ClipboardImage { id, result });
    });
}

pub(super) fn on_clipboard_image(
    &mut self,
    id: u32,
    result: Option<(String, Vec<u8>)>,
) {
    match result {
        Some((media_type, data)) => {
            let size_kb = data.len() / 1024;
            let resolved = self.ui.prompt.resolve_loading_image(id, media_type, data);
            if resolved {
                self.doc.info(&format!("attached: image ({size_kb} KB)"));
            }
            // Nếu placeholder không còn (user đã backspace) → drop data.
        }
        None => {
            let removed = self.ui.prompt.remove_loading_image(id);
            if removed {
                self.doc.info("no image in clipboard");
            }
        }
    }
}

pub(super) fn paste_image_file(&mut self, path: &str) {
    let Ok(data) = std::fs::read(path) else {
        self.doc.info("cannot read image file");
        return;
    };
    let size_kb = data.len() / 1024;
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let (media_type, _) = detect_image_format(&data);
    self.ui.prompt.attach_image(media_type.to_owned(), data);
    self.doc.info(&format!("attached: {name} ({size_kb} KB)"));
}
```

### Prompt API

Thêm vào `PromptState` (`src/tui/prompt/mod.rs`):

```rust
/// Insert a loading-image placeholder at cursor position.
pub fn attach_loading_image(&mut self, id: u32) {
    self.buf.attach_loading_image(id);
}

/// Replace placeholder with id by a real image. Returns true if
/// placeholder existed (false = user already removed it).
pub fn resolve_loading_image(
    &mut self,
    id: u32,
    media_type: String,
    data: Vec<u8>,
) -> bool {
    self.buf.resolve_loading_image(id, media_type, data)
}

/// Remove placeholder with id. Returns true if it existed.
pub fn remove_loading_image(&mut self, id: u32) -> bool {
    self.buf.remove_loading_image(id)
}
```

Tương ứng trong `PromptBuffer` — thao tác trên `Vec<Seg>` linear
search theo id. O(n) acceptable vì n nhỏ (số segment trong 1 prompt
hiếm khi >10).

### Render

`render.rs` của prompt MUST render `Seg::LoadingImage` như chip với
text `loading image…` + tick-driven spinner (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`).
Spinner state SHOULD lưu trên `PromptState` (1 `u8` counter, tăng
mỗi tick), không cần per-segment state.

Khi `to_content()` / `take_content()` chạy với buffer còn
`LoadingImage`, MUST drop placeholder (không emit ContentBlock cho
nó). Trường hợp này hiếm — chỉ xảy ra nếu user nhấn Enter trong khi
clipboard read chưa xong. Alternative: block submit cho đến khi tất
cả LoadingImage resolve. Default đề xuất: drop, vì non-blocking UX
quan trọng hơn.

### Image metadata

Cải thiện chip rendering — thêm dimensions:

```rust
fn parse_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: width/height tại offset 16-23 (big-endian u32 mỗi cái)
    if data.starts_with(&[0x89, b'P', b'N', b'G']) && data.len() >= 24 {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return Some((w, h));
    }
    // JPEG: scan SOF marker. Skip — quá phức tạp cho lợi ích nhỏ.
    None
}
```

Chip format: `[png 1920×1080 245KB]` nếu có dimensions, fallback
`[png 245KB]` nếu không parse được.

### Test plan

| Scenario | Test |
|----------|------|
| `on_paste("")` triggers clipboard read | Unit test trong `dispatch.rs` (cần mock `tx`) |
| `on_paste("hello")` insert text | Existing test continues to pass |
| `on_paste("/path/img.png")` load file | Existing test |
| Resolve placeholder by id replaces seg | Unit test trong `buffer.rs` |
| Resolve unknown id returns false | Unit test |
| Remove placeholder by id | Unit test |
| `take_content()` skip LoadingImage | Unit test |
| Backspace removes LoadingImage chip | Unit test |
| Multiple pending paste có id riêng | Unit test |

### Migration

1. Thêm `Seg::LoadingImage(u32)` + buffer methods (3 file match
   exhaustive cần update: `buffer.rs`, `render.rs`, `mod.rs`).
2. Thêm `next_paste_id` vào `AgentHandle::new()`.
3. Sửa `Event::ClipboardImage` thành struct variant — update emit
   (`agent.rs`) và handle (`dispatch.rs`) sites.
4. Sửa `on_paste()` thêm empty-paste fallback (giữ nguyên Ctrl+V).
5. Update `paste_clipboard_image()` tạo placeholder trước khi spawn.
6. Update `on_clipboard_image()` resolve theo id.
7. Thêm spinner render trong prompt.
8. Thêm dimension parsing cho image chip.
9. Toast `attached: ...` cho clipboard và file paste.
10. Update existing tests + thêm tests mới.

Có thể chia thành 2 PR:
- **PR1** (smallest viable): Empty-paste fallback + dedupe. Giải
  quyết pain point #1 (screenshot không paste được trên một số
  terminal). 1-2 file change.
- **PR2** (UX polish): Loading placeholder + spinner + dimension
  + toast feedback. Churn lớn hơn (5-6 file).

### Rollback

Mỗi PR commit độc lập. Revert được riêng. Không có persistent state
hay format change ngoài enum biến thể — không cần migration script.

## Drawbacks

- **Empty bracketed paste là heuristic**, không phải signal chính
  thức. Một vài terminal có thể emit empty paste cho lý do khác (ví
  dụ paste khi clipboard đang được modify), trigger false-positive
  clipboard read. Mitigation: read trả về None → "no image in
  clipboard" — không có hậu quả khác ngoài 1 toast nhẹ.
- **Double-trigger trên terminal forward cả Ctrl+V và empty paste**
  (như iTerm2): app spawn 2 clipboard read, có thể attach 2 chip
  image trùng. Mitigation đơn giản: throttle `paste_clipboard_image()`
  bằng `Instant` last-call (skip nếu <100ms từ lần trước). Có thể
  defer sang follow-up nếu user report.
- **Spinner animation** thêm tick load lên prompt render. Hiện tại
  tick chỉ chạy khi streaming, sẽ phải chạy cả khi có
  `LoadingImage`. Acceptable: render path đã batched, thêm 1 chip
  width không đáng kể.
- **Breaking event variant**: code ngoài crate dùng `Event` sẽ phải
  update. Không có consumer ngoài binary chính → no-op.
- **Drop submitted-with-pending-paste**: nếu user nhấn Enter trong
  lúc clipboard read pending, image bị drop khỏi message. Edge case
  hiếm; user thấy chip thì không submit cho đến khi resolved là
  hành vi tự nhiên.

## Rationale and alternatives

### Vì sao chọn empty-paste detection

- **Tự nhiên**: user vẫn nhấn Cmd+V như thói quen, không cần học
  shortcut mới.
- **Cross-platform free**: macOS, Windows, Linux đều dùng bracketed
  paste cho paste shortcut → 1 code path.
- **Không state machine**: id-based placeholder match thay vì global
  pending flag với timeout.

### Alternative 1: Bỏ raw Ctrl+V, dùng empty-paste duy nhất

- Pro: code path đơn giản nhất, không có double trigger.
- Con: Ghostty và một số terminal không emit event khi clipboard
  chỉ có image (đang là issue mở của Ghostty về Cmd+V image).
- Con: User mất hoàn toàn cách paste image trên terminal đó.
- **Loại** sau khi test khả thi cho thấy terminal behavior không
  đảm bảo.

### Alternative 2: Slash command `/paste`

```
> /paste
[image attached]
```

- Pro: explicit, không heuristic.
- Con: user phải biết command tồn tại, phải gõ thay vì shortcut quen.
- Con: vẫn cần fix empty-paste case (user expect Cmd+V hoạt động).
- **Loại** vì giải quyết symptom phụ, không thay thế được flow chính.

### Alternative 3: PasteState machine + timeout

Set `ClipboardImagePending` khi spawn read, suppress bracketed paste
trong khoảng đó, timeout 2s reset.

- Pro: explicit chống double-trigger.
- Con: thêm state, thêm tick counter, thêm timeout tuning, vẫn
  không giải quyết pain point #1 (screenshot).
- Con: race khi 2 paste consecutive trong <2s.
- **Loại** vì id-based design đơn giản hơn và scale tốt hơn.

### Alternative 4: Bắt thêm `Modifiers::SUPER` cho Cmd+V

- Con: macOS terminal không forward Cmd+V — chỉ hoạt động nếu user
  bật Kitty keyboard protocol và tắt terminal-level paste handling.
- Con: tăng surface area key handling, không giảm.
- **Loại** vì giả định không đúng với terminal mặc định.

### Alternative 5: Không làm gì

- Pain point #1 (screenshot không paste được) vẫn là blocker cho UX
  cốt lõi của TUI hỗ trợ image input.
- Code path 3 nhánh tiếp tục bug-prone khi thêm platform / terminal
  mới.
- **Loại**.

## Prior art

- **Claude Desktop / Cursor / Zed**: paste image qua clipboard với
  loading placeholder. Pattern "placeholder rồi resolve" là chuẩn
  industry cho async attach.
- **Discord / Slack**: paste image hiển thị thumbnail ngay khi user
  paste, kể cả khi upload chưa xong. Lesson: visual feedback quan
  trọng hơn correctness của bytes tại render time.
- **Helix editor** (`helix-term/src/ui/prompt.rs`): xử lý bracketed
  paste với content-based dispatch (text vs path), không bắt raw key
  — model mà RFC này áp dụng.
- **Crossterm bracketed paste docs**: confirm pattern empty paste có
  thể signal non-text clipboard, nhưng không phải standard guarantee.

## Unresolved questions

- **Q1**: Submit khi có `LoadingImage` pending → drop hay block?
  - Default đề xuất: drop với warn. Lý do: blocking gây impression
    app freeze.
- **Q2**: Cancel clipboard read khi placeholder bị xóa?
  - Default: không cancel (task ngắn ~300ms, complexity của
    `CancellationToken` không đáng). Result handler đã check
    `resolved` → drop data nếu placeholder không còn.
- **Q3**: SSH detection có nên thử clipboard anyway (một số setup
  có clipboard forwarding qua xclip/wl-clipboard)?
  - Default: vẫn skip — false-positive (clipboard rỗng vì SSH) gây
    confuse hơn không thử.
- **Q4**: Spinner frame rate? 250ms (existing tick) hay nhanh hơn?
  - Default: 250ms — match tick, không cần thêm timer.

## Future possibilities

- Drag-and-drop binary image (không qua bracketed paste path) — cần
  terminal hỗ trợ OSC 52 / Kitty image protocol.
- Multiple image paste trong 1 thao tác (clipboard có album).
- Image preview hover trên chip — render dùng Kitty/iTerm2 inline
  image protocol.
- Auto-resize/compress image trước khi gửi provider để tiết kiệm
  token.
- Paste video / audio attachment — generalize `Seg::LoadingImage`
  thành `Seg::LoadingMedia { kind, id }`.

## Implementation status

Chưa implement. Sẽ cập nhật commit/PR sau khi merge.
