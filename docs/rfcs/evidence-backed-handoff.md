# RFC: Evidence-backed Handoff for Multi-Provider Context Planning

- Status: M1 shipped; M2 planner phase A shipped; handoff + provider switch deferred pending metrics
- Author: Nghia / Luma
- Updated: 2026-04-12

## 0. Implementation status

RFC này được ship theo milestone, không theo 6 phase tuần tự (§10):

- **M1 — Evidence store (shipped).** Commits `ec5cc73`, `6224251`, `880b31b`.
  Oversized tool results (≥ 8K) spill sang `sessions/{id}/evidence/{ev_id}.txt`,
  transcript giữ summary + `evidence_id`. Crash-safe write (tmp → fsync →
  rename → append record). Image path scoped sang `images/`. Provider
  adapters không đổi (destructure thủ công, `evidence_id` không lên wire).
- **M2 planner phase A — Evidence dedup injection (shipped).** Commits
  `6508100`, `3aae889`, `d3b62ea`, `1f86532`, `c61e8e8`, `a49bad9`,
  `2b6398c`, `235bfa5`. `core/context_plan.rs` chèn giữa
  `session.messages` và `provider.stream()`. Hai lane:
  file-based dedup latest-per-file (Read/Edit/Write/Grep) và
  no-file recency (Bash/GhFile/WebFetch). Shared budget 32K chars.
  Anchor bao phủ cả tool_result tail. Wrap `<system-reminder>` và
  smoosh vào `tool_result.content` cuối. Path normalize trước khi
  dedup. Partial Read summary tag `(partial: lines X-Y, …)`.
  Trigger và post-ship fix progression: ses_19d802b2734 (§2.2),
  ses_19d8049f736 (§2.3), ses dùng inject đầu (§2.4),
  ses dùng inject với Bash/partial-read (§2.5).
- **M2 handoff + provider switch (deferred).** `core/handoff.rs` và
  `ProviderCacheHint` không implement cho tới khi có signal thực từ
  §16. Planner hiện dùng turn_index recency thay cho `files_in_play`
  (chưa có handoff).

Các deviation so với RFC gốc — tất cả giảm complexity vì không tìm thấy
invariant cần encode:

- `Session` giữ flat. Không tách `SessionMeta` / `Transcript` / `SessionStats`
  / `ProviderThreads` ở Phase 1. Tách là cosmetic refactor; sẽ re-evaluate
  khi M2 thêm 2-3 field nữa (§6.1).
- `EvidenceStatus::Pending` bị bỏ. Write order đảm bảo record tồn tại
  ⇒ blob tồn tại, không có trạng thái half-written cần đánh dấu (§6.2).
- `Tool::classify_evidence` trait extension không làm ở M1. `classify` ở
  `core::evidence` đang đủ cho 5 tool (Read/Grep/GhSearch/Bash/exec_command);
  mở trait khi có tool thứ 2 cần override (§6.5).
- `EvidenceDraft.blob: Option<String>` rút thành `String` (non-optional).
  Caller đã quyết định promote trước khi gọi ingest — `None` branch chưa
  từng hit (§6.5).
- **Planner inject in-place, không tạo User message mới.** RFC gốc ngầm
  định evidence là Message riêng. Claude yêu cầu user/assistant alternation
  nghiêm; injection in-place (prepend `ContentBlock::Text` vào user turn
  cuối) là cách duy nhất không phá wire contract (§9.1).
- **Planner dedup dựa trên `related_files` thuần, không cần handoff.**
  RFC gốc §9 dùng `related_files ∩ handoff.files_in_play`. Phase A dùng
  `latest-per-file trong recent turn window` — vẫn deterministic, không
  cần handoff để khởi động, và đã chữa duplicate-read signal đo được.
- **Injection shape mirror Claude Code.** Evidence wrap trong
  `<system-reminder>...</system-reminder>` và smoosh vào
  `tool_result.content` (không làm sibling `ContentBlock::Text`).
  Đây không phải lựa chọn của dự án — là invariant từ Claude API
  training. Sibling Text sau tool_result (a) bị model hiểu là user
  input và trigger prompt-injection defense, (b) render trên wire
  thành `</function_results>\n\nHuman:<…>` → teach model emit
  `Human:` drift (Claude Code A/B 92% → 0%, §2.4). Pattern này có
  sẵn ở `src/utils/messages.ts` của Claude Code leak; RFC phase A
  đầu không biết và đã retrofit.
- **Planner tách two-lane cho records không có `related_files`.**
  Bash/GhFile/WebFetch evidence không có path dedup key. RFC gốc
  assume mọi record có `related_files`; Phase A đầu silently drop
  class này. Fix: no-file lane giữ theo recency, share window/
  budget với file-based lane (§2.5).

## 1. Tóm tắt

RFC này đề xuất tách 3 trách nhiệm hiện đang bị dồn vào `session.messages` thành 3 lớp riêng:

1. `transcript`: lịch sử hội thoại canonical
2. `evidence`: dữ liệu dài, log, excerpt, output cần truy hồi
3. `handoff`: working memory có cấu trúc để agent tiếp tục task hoặc đổi provider

Thay vì compact transcript liên tục hoặc nhét toàn bộ tool output vào prompt, agent sẽ:

- lưu raw tool outputs lớn vào evidence store
- giữ summary ngắn trong transcript
- cập nhật handoff snapshot theo facts đã xác minh
- build prepared context theo từng turn từ transcript + handoff + selected evidence

Mục tiêu là tăng chất lượng context, giảm hallucination, và hỗ trợ multi-provider tốt hơn mà không phụ thuộc vào provider-local state.

## 2. Động cơ

Hiện trạng:

- tool output dài làm phình `session.messages` (hard truncate ở `AGENT_RESULT_SAFETY_CAP = 32_000` trong `turn.rs`)
- hard truncate làm mất thông tin và khó debug
- transcript đang vừa là lịch sử, vừa là working memory, vừa là provider input
- đổi provider giữa đường khó ổn định
- nếu để model tự summarize quá sớm sẽ tăng nguy cơ hallucination

### 2.1. Bằng chứng từ local sessions

Feasibility scan (175 sessions, xem `core::session::tests::rfc_feasibility_scan`)
xác nhận các giả định:

- **Tool output thực sự phình transcript.** 512 tool_result blocks: p50 = 382 chars,
  p90 = 9.6K, p99 = 31.9K, max = 32_012 (đã hit cap hiện tại).
- **Hard-truncate đang mất info.** 26/512 tool_result (5.1%) đã mang marker
  `[truncated]` — debugging phải dựa vào suy đoán.
- **Ngưỡng 8K là sweet spot.** Với threshold = 8K: 70 tool_result (13.7%)
  thành evidence blob → transcript giảm 73% bytes tool_result. Threshold = 16K
  chỉ giảm 52%; threshold = 4K giảm 83% nhưng tạo quá nhiều blob nhỏ (101 blobs).
- **`EvidenceKind` tối thiểu đủ cover.** Distribution của tool_result ≥ 8K:
  Read 30, Bash 19, GhFile 7, exec_command 7, Grep 4, GhLs 3 → match đúng
  5 variants đề xuất (`ReadExcerpt`, `BashLog`, `GrepResult`, `BuildLog`, `Other`).
- **Tool đã tự aware về size.** `Edit`: 103 calls, tổng 8.5K chars, max 88 → xác
  nhận design "tool owns its summary" (§6.5).
- **Session tail dài.** p50 = 10.8KB / 3 msgs, p99 = 767KB / 245 msgs, max = 1.14MB.
  Lợi ích evidence tập trung ở tail; head sessions gần như không đổi.
- **107/175 session legacy** (role `"tool"`, system prompt raw string) hiện đã
  silent-fail trong `Session::load()`. Dự án đang ở beta — RFC không migrate
  và cũng không cam kết tương thích với session pre-RFC.

### 2.2. Bằng chứng từ M1 production (ses_19d802b2734)

Session dev đầu tiên sau khi M1 ship, ~2 phút, 63 messages:

- **M1 correctness đã verified.** 13/13 tool_result ≥ 8K được promote;
  13/13 record có blob trên disk (`sessions/{id}/evidence/{ev_id}.txt`);
  0 orphan, 0 broken ref, 0 `.tmp` leftover. Tmp → fsync → rename
  (§7.3) hoạt động đúng.
- **Summary footprint nhỏ đúng expectation.** Min 82, max 87 chars inline
  per promoted result, so với avg ~15K raw — giảm ~99.4% bytes cho blob.
  13 × 15K = ~195K raw → ~1.1K in-transcript.
- **Duplicate read confirm signal M2 cần chữa.** `types.rs` được `Read`
  lại ở turn 22, 30, 32 (3 lần trong ~10 turn); `agent/turn.rs` ở turn
  24, 44, 48 (3 lần, gap 20 turn). Agent stateless giữa các turn —
  summary 85 chars không mang content, khi cần code buộc phải call
  Read lại. Signal #4 của §16 quan sát được ngay từ session đầu →
  trigger M2 planner phase A sớm hơn dự định.
- **`related_files` populated đúng từ args.** Mọi record có path thực
  — sẵn sàng cho planner dedup rule (§9.1). *Nhưng giá trị thô
  không normalize* — xem §2.3 cho hệ quả.
- **`EvidenceKind` distribution:** 13/13 là `read_excerpt` cho session
  dev read-heavy này; 0 BuildLog. Phase A ưu tiên case read-heavy —
  case verification cần session chạy test để validate.

### 2.3. Bằng chứng từ Phase A post-ship (ses_19d8049f736)

Session dev đầu tiên sau khi phase A ship, ~5.5 phút, 71 messages, 27
tool call (trong một assistant turn duy nhất 26 tool + 1 user). Đo hai
bug design trong phase A bản đầu:

- **Anchor rule fire 0 lần.** `find_last_user_text_index` yêu cầu tail
  là user+text. Trong session có 1 assistant turn chứa 26 tool call;
  mọi stream iter giữa loop có tail = `tool_result` → guard skip →
  store có record nhưng không inject lần nào. Phase A về mặt code
  path **không chạy**.
- **Duplicate cluster trong cùng turn, không cross-turn.** Giả định
  gốc (duplicate xuất hiện giữa các user turn) sai — pattern thực là
  agent loop `Read → reason → Read → reason …` trong **cùng một
  assistant turn**. Injection point duy nhất thấy được bởi
  `stream()` tiếp theo là message cuối = `tool_result` user message.
- **Path spelling mismatch sẽ bypass dedup.** `extract_related_files`
  lưu raw path từ args; agent gọi `Read src/x.rs` rồi `Read
  /Users/.../src/x.rs` → 2 record, 2 key dedup, 2 injection cho cùng
  file. Confirm bằng code inspect — chưa hit trong session này
  nhưng risk rõ ràng.

Ba fix shipped ngay sau session (§0):

- `d3b62ea` `fix(context_plan): anchor on any user tail and dedup
  injected evidence` — nới anchor để fire ở tool_result tail; thêm
  dedup idempotent qua header `# Retrieved evidence: {id}` để không
  double-inject khi cùng record đã có trong transcript.
- `1f86532` `fix(evidence): normalize related_files for dedup across
  path spellings` — `normalize_path` (strip `./`, rewrite cwd-prefix
  sang relative) ở classify time, không đụng disk.

### 2.4. Bằng chứng từ session đầu dùng Phase A (inject fired lần đầu)

Hai bug mới quan sát được khi phase A thật sự chạy và agent thấy
injection output:

- **400 Bad Request: tool_use without tool_result immediately after.**
  Phase A bản `d3b62ea` prepend evidence làm `ContentBlock::Text`
  block đứng **trước** `tool_result` trong user tail. Anthropic API
  yêu cầu user message sau assistant tool_use bắt đầu bằng
  tool_result → reject toàn turn. Fix ban đầu ở `c61e8e8`: dịch
  evidence xuống sau tool_result cluster (vẫn sibling Text). Wire
  hợp lệ nhưng vẫn không đúng shape Claude expect (xem điểm 2).
- **Agent treat evidence như prompt-injection attack.** Khi fix 400
  xong, session tiếp theo cho thấy agent đọc file với `limit=302`
  (defensive chunking) thay vì trust content đã có sẵn trong
  context. Model thấy Text block lạ sau tool_result, behavior là
  "user chèn gì đó đáng ngờ, phòng thủ". Agent vẫn `Read` lại file
  bình thường → inject không có tác dụng giảm duplicate.

**Research từ Claude Code leak giải quyết cả hai:**

`src/utils/messages.ts` ở `yasasbanukaofficial/claude-code` có 2
primitive được dùng mọi chỗ inject metadata vào user message:

- `wrapInSystemReminder(text)` → `<system-reminder>\n{text}\n</system-reminder>`.
  Trained signal: model hiểu content là system-authored metadata,
  không phải user input. Prompt-injection reflex tắt.
- `smooshIntoToolResult(tr, blocks)` → concat text chunks vào
  `tool_result.content` cuối, **không** tạo sibling Text. Comment
  trong source: sibling sau tool_result render trên wire thành
  `</function_results>\n\nHuman:<…>` → repeated mid-conversation
  teach model emit `Human:` drift. A/B test (`sai-20260310-161901`)
  của Claude Code: 92% → 0% khi chuyển sang smoosh.

Fix shipped ở `a49bad9`:

- `wrap_system_reminder` + smoosh vào `tool_result.content` khi
  anchor có tool_result.
- Plain user-text anchor (không có tool_result, không có assistant
  tool_use liền trước) vẫn prepend Text block — không cần smoosh.
- `collect_injected_ids` scan cả `tool_result.content` để idempotent.

**Lesson quan trọng:** injection shape không phải design choice —
là invariant cứng từ Claude training. Phase B và các layer sau phải
reuse hai primitive này, không được "prototype" shape khác.

### 2.5. Bằng chứng từ session dùng inject (sau wrap-smoosh fix)

Khi `<system-reminder>` + smoosh đã ship, agent thấy evidence đúng
shape nhưng hai class tool vẫn bị mù:

- **Bash/GhFile/WebFetch không bao giờ re-inject.** `select_evidence`
  filter `!r.related_files.is_empty()` → mọi record có
  `related_files = []` bị drop khỏi candidate set vĩnh viễn.
  `extract_related_files` trả empty cho tool không phải
  Read/Edit/Write/Grep. Trong session agent chạy `git status &&
  git diff`, blob được promote thành evidence đúng quy trình, nhưng
  iter kế tiếp planner vẫn không thấy → mù với diff mình vừa sinh ra.
- **Partial Read không phân biệt với full Read.** `Read limit=300
  offset=1` trả 302 dòng (300 thực + 2 metadata header). Summary
  `({path} (302 lines, stored as evidence)` không có dấu hiệu
  partial → model dễ tưởng 300 lines = entire file. File thật 831
  dòng.

Fix shipped:

- `2b6398c` `fix(context_plan): inject evidence without related_files
  via recency lane` — tách `select_evidence` thành hai lane cùng
  share recent-window + idempotency + budget. File-based lane dedup
  latest-per-file (Read/Edit/Write/Grep). No-file lane giữ theo
  recency (Bash/GhFile/WebFetch, mỗi invocation là artifact riêng).
- `235bfa5` `fix(evidence): tag partial Read summary with line range` —
  khi args có `offset`/`limit`, summary tag `(partial: lines X-Y, …)`
  hoặc `(partial: from line X, …)`. Full read không tag — giữ
  format cũ.

Non-issue (để ý rồi bỏ): chain `echo --- && cmd1 && cmd2` → blob
single → không tách được. Design limit "promote whole blob"; agent
phải tự chia thành nhiều Bash call nếu muốn tách artifact.

Bug cấp phối thứ hai (chưa fix): iter N gọi Read, cùng iter chỉ
thấy summary; blob đầy đủ phải chờ iter N+1 khi planner chạy
pre-stream. Đây là hệ quả cấu trúc của planner-trước-stream, không
phải bug. Cân nhắc Phase B: preview inline đoạn đầu blob khi promote,
hoặc eager-load blob ngay trong tool_result của iter N — cả hai
đều phá idempotency hiện tại nên defer.

Dự án này cần:

- multi-provider
- có thể chuyển provider giữa đường để backup
- chất lượng context tốt
- cache effective hơn
- giảm cost mà không hy sinh correctness

## 3. Mục tiêu

### 3.1. Goals

- Giữ transcript canonical, provider-neutral
- Lưu tool outputs lớn ngoài transcript
- Tạo handoff snapshot deterministic, facts-only
- Build prompt theo turn thay vì gửi raw history nguyên xi
- Hỗ trợ provider switch mà không mất tiến độ
- Giảm prompt noise và cải thiện cache stability

### 3.2. Non-goals

- Không triển khai semantic memory/vector DB ở giai đoạn đầu
- Không để model viết handoff mặc định
- Không tối ưu token cực đoan bằng heuristic khó debug ngay từ đầu

Ghi chú: RFC kế thừa các pattern phổ biến trong ngành (working memory + addressable evidence). Không đặt mục tiêu "phải khác Claude Code", mà đặt mục tiêu đúng trách nhiệm và deterministic.

## 4. Đề xuất

### 4.1. Các khái niệm mới

#### Canonical transcript
Lưu trong `Session.transcript.messages`:
- user messages
- assistant messages
- tool_use
- tool_result: `content` giữ summary ngắn, `evidence_id` ref blob (xem §6.4)

#### Evidence store
Lưu:
- read excerpts
- grep results
- bash/build/test/clippy logs
- các tool outputs dài khác

Mỗi evidence có `id`, `summary`, `blob_path`, metadata liên quan. Blob lưu tại `sessions/{session_id}/evidence/{evidence_id}.txt` (xem §6.3).

#### Handoff snapshot
Working memory có cấu trúc, deterministic, do core dựng (không do model):
- task hiện tại
- current_state
- files_in_play
- unresolved
- recent_evidence_ids (bounded)
- next_step

#### Prepared context
Context thực tế gửi provider ở turn hiện tại, được build từ:
- system/rules
- handoff
- recent transcript
- selected evidence excerpts

### 4.2. Quy tắc cốt lõi

Không compact transcript làm source of truth.

Thay vào đó:
- transcript giữ lịch sử thật
- evidence giữ dữ liệu dài
- handoff giữ trạng thái công việc
- planner chọn những gì cần gửi model ở turn này

## 5. Kiến trúc đề xuất

Thêm 3 module core mới:

- `src/core/evidence.rs`
- `src/core/handoff.rs`
- `src/core/context_plan.rs`

Đồng thời tách `Session` thành sub-structs (§6.1) để tránh "God struct".

### `core/evidence.rs`
Phụ trách:
- ingest raw tool result
- quyết định summary vs persisted blob
- load excerpt theo budget khi planner cần
- quản lý crash-safe write order (§7.3)

Summary template: **không** centralize trong `evidence.rs`. Mỗi tool tự khai báo
summary của nó qua extension trên `Tool` trait (§6.5). `evidence.rs` chỉ cầm
ingest generic.

### `core/handoff.rs`
Phụ trách:
- build/update handoff từ facts đã xác minh
- refresh sau tool batch / verification / provider switch
- giữ `recent_evidence_ids` bounded (`HANDOFF_RECENT_EVIDENCE_MAX = 16`)

### `core/context_plan.rs` (Phase A shipped)
Phụ trách:
- build prepared messages trước mỗi `provider.stream()`
- chọn evidence nào cần load
- enforce context budget ở mức planner (char-based)
- log quyết định planner để debug

Phase A đã ship rule duy nhất (§9.1): dedup latest-per-file trong recent
window 15 turn, budget 32K chars, inject in-place vào user turn cuối.
Chưa dùng handoff — handoff vẫn là input optional của Phase B.

Rule mới chỉ được thêm kèm test regression (RULES §14).

## 6. Data model

### 6.1. Tách `Session`

`Session` hiện đã cohesive với `messages + usage + turn_durations`. Khi thêm
evidence/handoff/providers, flat struct sẽ có 6 field không cùng lifecycle. Tách
thành sub-structs:

```rust
pub struct Session {
    pub meta: SessionMeta,
    pub transcript: Transcript,
    #[serde(default)]
    pub evidence: EvidenceStore,
    #[serde(default)]
    pub handoff: Option<HandoffSnapshot>,
    #[serde(default)]
    pub providers: ProviderThreads,
    #[serde(default)]
    pub stats: SessionStats,
}

pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct Transcript {
    pub messages: Vec<Message>,
}

pub struct SessionStats {
    pub usage: SessionUsage,
    pub turn_durations: Vec<f64>,
}
```

Planner nhận `&Transcript`, `&EvidenceStore`, `&HandoffSnapshot` — không cầm
nguyên `&Session`.

Dự án đang ở beta — schema thay đổi là **breaking change**. Session pre-RFC
không đảm bảo đọc được sau M1. Không thực hiện migration; session cũ
sẽ silent-fail ở `Session::load()` như hôm nay (đã là hành vi hiện tại với
107/175 session legacy, xem §2.1).

### 6.2. Types mới

```rust
pub struct EvidenceRecord {
    pub id: String,
    pub kind: EvidenceKind,
    pub tool_use_id: Option<String>,
    pub summary: String,
    pub blob_path: Option<String>,
    pub chars: usize,
    pub turn_index: usize,
    pub related_files: Vec<String>,
    /// Crash-safety marker. Planner bỏ qua Pending.
    #[serde(default)]
    pub status: EvidenceStatus,
}

pub enum EvidenceStatus {
    Pending,
    Persisted,
}

pub struct EvidenceStore {
    pub records: Vec<EvidenceRecord>,
}

pub struct HandoffSnapshot {
    pub version: u32,
    pub task: String,
    pub current_state: Vec<String>,
    pub files_in_play: Vec<String>,
    pub unresolved: Vec<String>,
    /// Bounded by `HANDOFF_RECENT_EVIDENCE_MAX`.
    pub recent_evidence_ids: Vec<String>,
    pub next_step: Option<String>,
    pub source_turn: usize,
}

pub struct ProviderThreads {
    pub entries: Vec<ProviderCacheHint>,
}

/// Performance hint only — không phải source of truth. Planner không phụ
/// thuộc vào nó; provider có thể dùng để resume prompt cache.
pub struct ProviderCacheHint {
    pub provider: String,
    pub model: String,
    pub thread_id: Option<String>,
    pub updated_at: String,
}
```

### 6.3. Session asset layout

Session dir trước RFC có image flat: `sessions/{id}/img_xxx.png`. RFC chuẩn
hóa taxonomy trước khi evidence vào:

```
sessions/
  {id}.json
  {id}/
    images/{image_id}           # new layout
    evidence/{evidence_id}.txt  # new — text blobs only
```

- **Path:** evidence blob ở `sessions/{id}/evidence/{evidence_id}.txt`.
- **Image không quản lý bởi evidence store.** Image đã có channel riêng
  (`save_image`, `image_resolver`) với lifecycle khác: immutable, resolved
  to base64 at send time. Evidence có `status` và được planner load on-demand.
  Gộp làm một sẽ loãng cả hai.
- **Image path:** `sessions/{id}/images/{image_id}` (đổi từ flat). `save_image`
  ghi thẳng `images/`; `read_image_base64` chỉ đọc `images/`. Session cũ
  tham chiếu flat path sẽ không resolve — beta stance.
- **Evidence id convention:** `ev_{timestamp:x}` tương tự `ses_{ts:x}` và
  `img_{ts:x}` hiện tại. Generator dùng monotonic counter nếu collision
  trong cùng turn.
- **Tạo:** M1 viết evidence blob write-first + fsync + rename (§7.3).
- **Xóa session:** xóa cả thư mục `sessions/{id}/` — cover tất cả assets.
- **Fork session (nếu có):** clone thư mục; id của asset không đổi.
- **Quota:** M1 không áp; M2 đo `chars` tổng và cảnh báo nếu
  vượt ngưỡng (tuning sau).
- **GC:** evidence và image chỉ bị xóa khi session bị xóa. Không auto-prune
  vì mục đích debug là giữ raw.

### 6.4. `ContentBlock::ToolResult` structured evidence ref

Hiện `ToolResult { tool_use_id, content, is_error }` chỉ có 1 field text.
RFC mở rộng:

```rust
ToolResult {
    tool_use_id: String,
    content: String,           // summary ngắn, hoặc full nếu không promote
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
},
```

- **Structured, không string-encode.** `evidence_id` là invariant, type
  phải diễn đạt (RULES §12). Không parse `"[evidence:ev_xxx]"` từ `content`.
- **Wire format.** Provider adapter chịu trách nhiệm ignore `evidence_id`
  khi serialize lên API (nó không có nghĩa với model); planner dùng nó
  để load blob khi cần.
- **`content` vẫn bắt buộc.** Khi không có blob (tool_result nhỏ), chỉ
  set `content`, `evidence_id = None`. Khi có blob, `content` là summary
  (~200 chars), `evidence_id` trỏ blob.
- **Beta breaking.** Thêm field mới với `serde(default)` tương thích read
  cho session post-M1 format; không cam kết cho session pre-RFC.

### 6.5. Tool summary extension (deferred to Phase B)

**M1 chọn centralize-first** thay vì trait extension: `classify` ở
`core::evidence` nhận `(tool_name, args, result)` và cover 5 tool hiện
có (Read/Grep/GhSearch/Bash/exec_command). Lý do:

- Chỉ 5 tool cần classify đặc thù; trait per-tool là boilerplate.
- Summary/classify ít drift hơn nếu sống cùng một chỗ với
  `EvidenceKind` và `EVIDENCE_PROMOTION_THRESHOLD`.
- `Tool` trait stay cohesive ở responsibility "execute", không kiêm
  presentation/classification.

Chuyển sang trait (đề xuất gốc ở dưới) khi có ít nhất một tool cần
override format theo cách không vừa `classify` centralized:

```rust
pub trait Tool {
    // ... existing methods ...

    /// Summary ngắn cho UI và transcript. Default fallback nếu tool không
    /// override.
    fn summarize_call(&self, args: &serde_json::Value) -> String { /* default */ }

    /// Classify + shape evidence record từ result. Default: lưu blob nếu
    /// vượt ngưỡng, summary = first line / short preview.
    fn classify_evidence(&self, args: &serde_json::Value, result: &str)
        -> EvidenceDraft { /* default */ }
}
```

Dependency vẫn theo chiều `agent → tool` (RULES §4), không đảo.

### 6.6. Path normalization cho dedup (shipped)

`extract_related_files` (`core::evidence`) normalize path trước khi
gắn vào `EvidenceRecord.related_files` để planner dedup hoạt động
đúng khi agent dùng nhiều spelling cho cùng file.

**Rule (`normalize_path`):**

1. Strip leading `./`.
2. Absolute path có prefix là cwd → rewrite sang relative từ cwd.
3. Absolute path ngoài cwd → giữ nguyên (identity riêng).

Không đụng disk — `Write` có thể classify path chưa tồn tại. Dùng
`Path::strip_prefix` của std, không dùng `canonicalize()`.

**Không xử lý (để sau):**

- Symlink resolution — cần I/O, và path spelling thường không liên
  quan tới symlink trong dev flow.
- Windows drive letter case (`C:\` vs `c:\`) — OS-dependent handling
  chưa cần ở scope hiện tại.
- `../` collapse trong relative path — hiếm gặp trong agent arg.
  Thêm khi có session reproduce.

Test matrix trong `core::evidence::tests`: leading-dot-slash,
cwd-prefix absolute, outside-cwd absolute, dedup-key equality qua
`classify()` cho 3 spelling.

## 7. Cách hoạt động

### 7.1. Normal turn

Actual flow (M1 + Phase A shipped):

1. User message được push vào `session.messages`
2. `context_plan::build_prepared_messages(PlanInput { transcript, evidence, assets_dir })`
   build prepared messages — inject evidence in-place vào user turn cuối
   nếu điều kiện thỏa (§9.1)
3. `provider.stream(&prepared)` trả về assistant text / tool_use
4. tools được execute
5. `maybe_promote_to_evidence(...)` — blob write-first, sau đó mới append record
6. transcript lưu summary ngắn + `evidence_id` (thay hard truncate)
7. (Phase B) `handoff::refresh_*` — chưa ship
8. session được save sau mỗi assistant message và mỗi tool result batch

### 7.2. Provider switch

Khi đổi provider:
- không rewrite transcript
- không giữ provider-local state làm source of truth
- refresh handoff
- provider mới nhận prepared context từ transcript + handoff + selected evidence
- `ProviderCacheHint` cho provider đích có thể dùng làm optimization, không
  phải ràng buộc

Tức là bàn giao công việc, không bàn giao raw hidden state.

### 7.3. Crash recovery

Write order (shipped ở M1):

1. Ghi blob ra `evidence/{id}.txt.tmp`, `fsync`, rename sang `.txt`.
2. Chỉ sau khi rename thành công, append record vào `EvidenceStore.records`.
3. Crash giữa (1) và (2) → không có record → blob orphan (vô hại, không
   gây lỗi khi load). Scan GC chưa cần.

`EvidenceStatus::Pending` đã bị bỏ: write order đảm bảo record ⇒ blob
exists, không có half-written state. Crash an toàn bằng thứ tự disk op,
không bằng flag persist riêng.

`fix_orphaned_tool_uses` (`agent.rs`) không cần đổi ở M1: placeholder
`[aborted]` vẫn là `ContentBlock::ToolResult` hợp lệ với `evidence_id =
None`. M2 có thể mở rộng nếu cần preserve evidence ref qua abort.

### 7.4. Tương thích với mid-turn save

`turn.rs` hiện save sau mỗi assistant message và sau mỗi tool result batch.
Thứ tự mới:

1. assistant message → push vào transcript → save session
2. execute tools
3. với mỗi result lớn: blob write-first (§7.3) → append evidence record
4. push tool_result blocks (chứa evidence ref) vào transcript
5. save session

I/O overhead chưa đo ở production; nếu metrics cho thấy fsync per-result
tăng latency đáng kể, cân nhắc batch hoặc async write (RULES §27).

## 8. Quy tắc chống hallucination

### Facts-only handoff
Handoff chỉ được chứa thông tin truy được về:
- transcript
- evidence
- verification status

### Verification là source of truth cao nhất
Nếu `cargo check`/`clippy`/`test` fail, handoff phải phản ánh fail.

### Unknown vẫn là unknown
Nếu chưa xác minh, phải ghi rõ là chưa xác minh.

### Model không phải source of truth
Ở phase đầu, model không tạo handoff. Handoff do core dựng deterministically.

## 9. Chiến lược context planning

Planner dùng bucket theo độ ưu tiên.

### Mandatory
- system prompt
- project rules
- current user request
- current handoff

### High priority
- latest verification result
- files in play
- evidence liên quan unresolved item

### Lower priority
- assistant prose cũ
- stale logs
- evidence không liên quan next step

### Evidence loading (Phase A shipped)

Phase A ship rule dedup độc lập handoff — đã chữa signal duplicate read
từ ses_19d802b2734 (§2.2) và ba bug design phát hiện post-ship ở
ses_19d8049f736 (§2.3):

> Với trailing user message (bất kể chỉ chứa tool_result hay text), xét
> evidence có `turn_index ≥ current_turn - RECENT_TURN_WINDOW`, chưa
> injected trước đó. Dedup latest-per-file theo path đã normalize.
> Sort recent-first, greedy-fit dưới `EVIDENCE_INJECTION_BUDGET_CHARS`.
> Inject chronological order — prepend content blocks vào user turn đó.

Constants shipped:

- `RECENT_TURN_WINDOW = 15` — từ duplicate-read cluster (~10-20 turn).
- `EVIDENCE_INJECTION_BUDGET_CHARS = 32_000` — ~3x p90 blob size.

Phase B (pending handoff) sẽ nâng rule thành:

> Ranking: `unresolved hit > user-mentioned file > recency`. Dedup và
> budget giữ nguyên.

Mọi rule thêm vào sau phải đi kèm:
- test regression (input transcript + evidence → output selection)
- log quyết định (which loaded, which dropped, why) vào session debug log

Rule-set chỉ mở rộng khi có evidence thực từ §16 metrics.

### 9.1. Rule hiện tại (Phase A)

**Anchor discovery.** `find_injection_anchor` accept tail là bất kỳ
`Role::User` message — text, paste, hoặc tool_result only. Assistant
tail và system-only → passthrough. Lý do nới guard: duplicate read
thực tế cluster trong cùng assistant turn (ses_19d8049f736 §2.3);
tail trong tool-loop iter là `tool_result` user message — không
anchor vào đó thì phase A không fire. Chỉ anchor message **cuối
cùng**, không scan ngược — user turn cũ giữ nguyên để cache prefix
ổn định.

**Idempotency qua marker scan.** `collect_injected_ids` scan cả Text
block và `tool_result.content` string tìm marker
`# Retrieved evidence: {id}`; record đã có trong transcript bị
filter khỏi selection set. Mỗi iter của cùng tool loop vẫn chạy
planner nhưng không re-inject — tail message tăng monotonic theo số
evidence unique, không phình theo iter count.

**Path normalization cho dedup.** `related_files` đã normalize ở
classify time (§6.6). Dedup key dùng path sau normalize → 3 spelling
cùng file dedup đúng về 1 record.

**Selection algorithm (two-lane).**

```text
1. window_start = current_turn - RECENT_TURN_WINDOW
2. in_window    = records có blob_path.is_some()
                  ∧ turn_index ≥ window_start
                  ∧ id ∉ already_injected

3a. file_lane (có related_files):
      latest[f] = max(turn_index) cho mỗi file f
      keep record nếu nó là latest[f] cho ≥ 1 file
      dedup by id (khi hai record tie turn_index cho khác file)

3b. no_file_lane (related_files rỗng):
      giữ tất cả — mỗi record là artifact riêng (Bash run
      khác nhau = output khác nhau, không có dedup key)

4. merged      = file_lane ∪ no_file_lane
5. sort merged DESC by turn_index
6. greedy fit dưới EVIDENCE_INJECTION_BUDGET_CHARS (skip over-budget,
   không partial load) — shared budget cross-lane
7. sort ASC by turn_index để inject chronological
```

**Injection shape (invariant cứng, không tự chọn).**

Mỗi evidence được render thành chunk:

```text
<system-reminder>
# Retrieved evidence: {id} ({summary})

{body}
</system-reminder>
```

Sau đó chèn tuỳ shape của anchor:

- **Anchor có tool_result (mid tool-loop).** Smoosh từng chunk vào
  `tool_result.content` **cuối cùng** — concat với `\n\n` separator,
  không tạo block mới. Mirror `smooshIntoToolResult` của Claude
  Code. Block shape của message không đổi.
- **Anchor plain user text (không tool_result).** Prepend mỗi chunk
  làm `ContentBlock::Text` trước user text gốc. Không có assistant
  tool_use liền trước → không có ràng buộc "tool_result must come
  first" và không có `Human:` drift risk.

**Lý do bắt buộc shape này** (§2.4):
- `<system-reminder>` tag là trained signal — nếu bỏ, model treat
  content như user input → prompt-injection defense triggered →
  agent dùng `limit=302` thay vì trust content.
- Sibling Text sau tool_result render trên wire thành
  `</function_results>\n\nHuman:<…>` → teach model emit `Human:`
  (A/B của Claude Code 92% → 0%).

**Fallback.** Blob missing on disk → skip record, log qua `dbg_log!`.
Không fail turn.

### Budget

- Phase A shipped: char-based (`EVIDENCE_INJECTION_BUDGET_CHARS = 32_000`).
  Deterministic, không provider-specific.
- Nâng cấp: bổ sung token-estimate per-provider khi đã có metrics.

Lý do: tokenizer khác nhau giữa Anthropic/OpenAI; pick sớm sẽ bias planner.

## 10. Kế hoạch rollout

Beta stance: project đang ở beta. Schema và path là breaking change chấp nhận
được. **Không migration, không backward-compat layer.** Session pre-RFC có
thể không load được — đó là trade-off đã chọn.

### M1 — Evidence store (shipped)

Ba commit tuần tự, mỗi commit compile + test + clippy clean:

1. **`refactor(session): scope image assets under images/ subdir`** —
   `sessions/{id}/images/{image_id}`. Session cũ tham chiếu flat image
   không resolve (beta stance).
2. **`feat(evidence): scaffold evidence store data model`** —
   `core/evidence.rs` với `EvidenceKind`, `EvidenceRecord`, `EvidenceStore`;
   `Session.evidence` serde-default; `ContentBlock::ToolResult.evidence_id`
   opt-in. Provider adapter (Claude/OpenAI/Codex) destructure thủ công
   và drop field không có nghĩa trên wire, nên `evidence_id` không leak.
3. **`feat(evidence): promote oversized tool results to evidence store`** —
   `classify` + `ingest` với tmp → fsync → rename, wire vào
   `turn::run_turn` qua `maybe_promote_to_evidence`. Threshold 8K.
   `AGENT_RESULT_SAFETY_CAP` được rename thành `SAFETY_FALLBACK_CAP` và
   chỉ còn là fallback khi ingest I/O fail. 5 tool được classify:
   Read / Grep / GhSearch / Bash / exec_command. `cargo` / `npm` /
   `pytest` / etc. được tag `BuildLog`.

Ba item gốc thuộc "Phase 2" bị drop khỏi M1:

- `Tool::classify_evidence` trait extension — classifier centralized ở
  `evidence.rs` đủ cho 5 tool hiện tại (§6.5). Mở trait khi có tool thứ 2
  cần override summary format.
- `fix_orphaned_tool_uses` evidence-aware — placeholder `[aborted]` vẫn
  là `ToolResult` hợp lệ với `evidence_id = None`. Không đổi hành vi.
- `EvidenceStatus::Pending` — bỏ luôn, xem §7.3.

### M2 planner phase A — Evidence dedup (shipped)

Hai commit ship ban đầu, trigger sau khi quan sát duplicate read trong
ses_19d802b2734 (§2.2):

1. **`feat(context_plan): scaffold planner with passthrough`** (`6508100`) —
   `core/context_plan.rs` với `build_prepared_messages` passthrough;
   wire vào `turn::run_turn` thay cho `&session.messages` trực tiếp.
   3 tests preserve order/count/tool_result+evidence_id.
2. **`feat(context_plan): inject deduped evidence into pending user turn`**
   (`3aae889`) — select + inject rule sơ khai. 10 tests: passthrough
   paths, dedup latest-per-file, budget skip, window filter, assistant
   tail passthrough.

Hai commit fix sau ses_19d8049f736 (§2.3) cho thấy 2 bug design:

3. **`fix(context_plan): anchor on any user tail and dedup injected evidence`**
   (`d3b62ea`) — nới anchor để fire ở tool_result tail (duplicate thực
   sự cluster intra-turn, không cross-turn); `collect_injected_ids`
   scan header `# Retrieved evidence: {id}` để skip record đã inject →
   idempotent qua mỗi iter của cùng tool loop. Test pivot: cũ
   `does_not_inject_mid_tool_loop` (semantic sai) thay bằng
   `injects_into_tool_result_user_message` + `idempotent_across_tool_loop_iters`.
4. **`fix(evidence): normalize related_files for dedup across path spellings`**
   (`1f86532`) — `normalize_path` ở classify time: strip `./`,
   rewrite cwd-prefix absolute → relative. Ba spelling cùng file dedup
   đúng về 1 key. 4 tests thêm trong `core::evidence`.

Hai commit fix sau session đầu thật sự fire phase A (§2.4):

5. **`fix(context_plan): insert evidence after tool_results, not before`**
   (`c61e8e8`) — Anthropic API reject 400 vì user message sau assistant
   tool_use phải bắt đầu bằng tool_result. Fix ban đầu: dịch evidence
   Text block xuống sau tool_result cluster (vẫn sibling, chưa đúng
   shape Claude expect).
6. **`fix(context_plan): smoosh evidence into tool_result and wrap in system-reminder`**
   (`a49bad9`) — retrofit 2 primitive của Claude Code:
   `wrap_system_reminder` quanh chunk và smoosh vào
   `tool_result.content` cuối. Giải quyết cả prompt-injection
   defense trigger (agent dùng `limit=302` defensive) và `Human:`
   drift (A/B 92% → 0% trong Claude Code A/B). Tests pivot:
   `evidence_wrapped_in_system_reminder` pin wrapper shape;
   `injects_into_last_tool_result_with_mixed_tail` pin last-of-many
   rule; idempotency test scan `tool_result.content` thay vì Text
   block.

Hai commit fix sau session với Bash/partial-read (§2.5):

7. **`fix(context_plan): inject evidence without related_files via recency lane`**
   (`2b6398c`) — `select_evidence` tách thành hai lane share recent
   window/idempotency/budget. File-based lane dedup latest-per-file;
   no-file lane giữ theo recency. Fix class bug Bash/GhFile/WebFetch
   evidence silently dropped. 3 tests thêm:
   `injects_bash_evidence_without_related_files`,
   `no_file_lane_keeps_every_record_by_recency`,
   `merged_lanes_share_budget`.
8. **`fix(evidence): tag partial Read summary with line range`**
   (`235bfa5`) — `Read { offset, limit }` tag summary
   `(partial: lines X-Y, …)` hoặc `(partial: from line X, …)`.
   Model không còn nhầm 300-line slice với entire file. 2 tests thêm
   trong `core::evidence`.

Deviation so với §9 gốc:

- Dedup dựa `related_files` thuần (đã normalize), không cần
  `handoff.files_in_play`. Handoff chưa ship; phase A đủ chữa
  signal #4 một mình.
- Inject **in-place** vào user turn cuối, không tạo Message mới.
  Wire-safe với Claude alternation requirement.
- Anchor accept tool_result-only user message. Cache prefix vẫn ổn
  vì planner chỉ chạm message cuối; user turn cũ giữ nguyên.
- Chỉ anchor **message cuối cùng**, không scan ngược — rewrite user
  turn mid-transcript sẽ phá cache và mis-align evidence với
  tool_use đã xử lý.
- **Injection shape mirror Claude Code** (§2.4): wrap
  `<system-reminder>` + smoosh vào `tool_result.content`. Không
  tạo sibling Text block sau tool_result — không phải lựa chọn,
  là invariant cứng từ Claude training.
- **Two-lane split** (§2.5): RFC gốc ngầm định mọi evidence có
  `related_files`. Bash/GhFile/WebFetch thì không — tách lane riêng,
  recency-only, share budget.

### M2 phase B — Handoff + provider switch (deferred)

Các phần sau không triển khai cho tới khi có dữ liệu thực:

- `core/handoff.rs` — handoff snapshot deterministic (§4.1, §8).
- Planner ranking nâng cấp: `unresolved hit > user-mentioned > recency`
  (phase A đã có dedup + recency, phase B bổ sung 2 tín hiệu đầu).
- Provider switch rebuild prepared context (§7.2).
- `ProviderCacheHint` optimization.
- `Session` sub-struct split (§6.1) — re-evaluate khi struct thêm 2-3
  field để biết shape đúng.

Gate để bật phase B: xem §16.

## 11. Lợi ích

- Context gọn hơn nhưng vẫn traceable
- Giảm prompt noise
- Giảm nguy cơ hallucination do summary tự do
- Hỗ trợ multi-provider tốt hơn
- Failover giữa providers ổn định hơn
- Cache prefix ổn định hơn, cải thiện cost
- Debug dễ hơn vì raw evidence vẫn còn

## 12. Trade-offs

- Thêm complexity ở core
- Cần quản lý evidence blobs trong session dir (lifecycle, crash safety)
- Planner chọn sai evidence có thể làm giảm quality
- Thêm I/O (blob writes) — đo ở production sau M1
- Cần test kỹ heuristic trước khi mở rộng thêm

Tuy vậy, trade-off này chấp nhận được vì:
- source of truth rõ hơn
- multi-provider bền hơn
- correctness tốt hơn hard truncate / transcript compaction

## 13. Quyết định đã chốt

Các câu hỏi mở trước đây được chốt như sau (có bằng chứng từ scan ở §2.1
hoặc từ kinh nghiệm M1):

- **`EvidenceKind` tối thiểu M1:** `ReadExcerpt`, `GrepResult`, `BashLog`,
  `BuildLog` (build/test/clippy), `Other`. Khớp với tool distribution thực tế.
  Ship y nguyên ở `core::evidence::EvidenceKind`.
- **Evidence threshold:** 8_000 chars. Dựa trên scan: giảm 73% tool_result bytes
  trong transcript với chỉ 13.7% tool_result (70/512) được promote thành blob.
  Ship ở `EVIDENCE_PROMOTION_THRESHOLD`.
- **Summary template:** centralize ở `core::evidence::classify` cho M1.
  Chuyển sang `Tool::classify_evidence` trait method khi có tool thứ 2 cần
  override — chưa thấy case. RFC gốc chọn trait-first; M1 chọn centralize-first
  vì ít abstraction hơn và scan confirm 5 tool cover hết (§6.5).
- **`EvidenceDraft.blob`:** non-optional `String` thay vì `Option<String>`.
  Caller quyết định promote trước khi ingest — `None` branch không có user.
- **`EvidenceStatus`:** bỏ. Write order đảm bảo invariant (§7.3).
- **`recent_evidence_ids` bounded:** `HANDOFF_RECENT_EVIDENCE_MAX = 16`. FIFO
  theo `turn_index`. **Phase B scope** — chưa ship.
- **Budget:** char-based; token-estimate chỉ cân nhắc sau metrics.
  Shipped: `EVIDENCE_INJECTION_BUDGET_CHARS = 32_000`.
- **Image vs evidence:** giữ tách riêng (`images/` vs `evidence/`). Khác
  lifecycle, khác resolver. Ship ở M1.
- **Migration:** không. Beta → breaking change. Session cũ tham chiếu image
  flat không fallback.
- **Provider-specific planner tuning:** không ở phase A-B. Nếu metrics
  cho thấy cần, thêm riêng mà không đổi API planner.
- **`Session` sub-struct split:** không ở M1 hay Phase A. Flat thêm
  2 field (`evidence`, future `handoff`) không gây đau; tách khi
  `ProviderThreads` vào sẽ biết shape đúng.
- **Planner anchor:** accept bất kỳ user message ở tail (text, paste,
  hoặc tool_result). Không scan ngược — chỉ message cuối. Rewrite
  user turn cũ giữa tool-loop sẽ phá cache prefix và mis-align
  evidence với tool_use đã xử lý (§9.1). *Lưu ý:* bản phase A đầu
  chỉ anchor user-text; fix ở `d3b62ea` sau khi quan sát 0 fire
  trong ses_19d8049f736.
- **Planner idempotency:** scan cả Text block và `tool_result.content`
  trong transcript tìm marker `# Retrieved evidence: {id}`; record đã
  có thì không re-inject. Giữ tail message stable qua mỗi iter của
  cùng tool loop (§9.1).
- **Planner injection shape:** mirror Claude Code primitives
  (`src/utils/messages.ts`) — wrap `<system-reminder>...</system-reminder>`
  và smoosh vào `tool_result.content` cuối khi anchor có tool_result;
  prepend `ContentBlock::Text` khi anchor plain user text. Không bao
  giờ tạo sibling Text sau tool_result. Lý do: (a) tag là trained
  signal, bỏ thì prompt-injection defense trigger; (b) sibling sau
  tool_result teach `Human:` drift (§2.4).
- **Planner recency window:** `RECENT_TURN_WINDOW = 15`. Từ duplicate-read
  cluster (~10-20 turn) trong ses_19d802b2734. Tune khi có session workload
  khác.
- **Path normalization cho dedup:** `normalize_path` strip `./`,
  rewrite cwd-prefix absolute → relative. Không dùng `canonicalize()`
  (đụng disk, Write có path chưa tồn tại). Symlink, `../` collapse,
  Windows drive letter case để sau khi reproduce (§6.6).
- **Two-lane selection:** file-based (Read/Edit/Write/Grep) dedup
  latest-per-file; no-file (Bash/GhFile/WebFetch) giữ theo recency.
  Share recent window, idempotency check, char budget. Mỗi Bash
  invocation là artifact riêng nên không có dedup key hợp lý (§9.1,
  §2.5).
- **Partial Read summary tag.** `Read` với `offset`/`limit` → summary
  tag `(partial: lines X-Y, …)` hoặc `(partial: from line X, …)`;
  full read giữ format cũ. Ngăn model nhầm slice với entire file
  (§2.5).

## 14. Rủi ro chính cần canh

- **Planner heuristic drift.** Một khi chạy được, rất dễ thêm rule mà không
  test. Ép nguyên tắc "mỗi rule mới = 1 test regression". Phase A đã ship
  với 19 context_plan test + 6 evidence test cover passthrough, dedup,
  budget, anchor, idempotency, path spelling, wrapper shape, smoosh
  target, two-lane split, partial Read — rule mới của Phase B phải giữ
  gate này.
- **I/O overhead.** Mid-turn save hiện đã ghi session.json mỗi batch. M1
  thêm blob fsync + rename mỗi tool_result ≥ 8K. Phase A thêm đọc blob
  mỗi turn khi có evidence match. Chưa đo ở production; nếu metrics
  cho thấy latency tăng đáng kể, cân nhắc cache blob content theo
  session hoặc async read.
- **Evidence orphan.** Crash giữa tmp write và rename → blob orphan (vô
  hại, chỉ tốn disk). Không có record → planner không thấy. Không cần GC
  chủ động cho tới khi disk pressure thành vấn đề.
- **Planner chọn sai file.** Phase A dedup thuần theo recency; nếu user
  chuyển focus sang file ngoài window, evidence cũ không được load.
  Signal để cân nhắc Phase B ranking (§16.2).
- **Evidence stale sau Edit.** Dedup theo record id: nếu `ev_100` đọc
  file X, sau đó `Edit` X ở turn kế, blob `ev_100` vẫn ở disk và
  planner vẫn inject nó (vì đã match dedup). Model thấy code cũ, có
  thể commit nhầm. Phase B có thể invalidate khi Edit cùng file;
  hiện tại agent vẫn có thể `Read` lại để force refresh và tạo
  record mới.
- **Wire shape drift qua providers khác.** Phase A smoosh vào
  `tool_result.content` chạy qua Claude provider adapter OK vì nội
  dung được pass-through. OpenAI/Codex adapter chưa test với smooshed
  content; nếu gặp provider reject hoặc parse lỗi, phải split theo
  provider (rule Claude ≠ rule khác) thay vì unify. Low-risk cho tới
  khi tương tác thực với OpenAI/Codex có evidence inject.
- **One-iter blind window.** Tool output > 8K ở iter N chỉ có summary
  inline; blob đầy đủ xuất hiện qua planner ở iter N+1. Iter N agent
  không reason được ngay trên content. Không phải bug (hệ quả
  planner-trước-stream), nhưng có thể lãng phí 1 iter nếu agent
  cần content ngay. Phase B cân nhắc: inline preview đầu blob ở
  promote time, hoặc eager-load blob ngay trong tool_result của iter
  N — cả hai phá idempotency hiện tại nên defer (§2.5).

## 15. Khuyến nghị

Triển khai theo hướng:
- deterministic first
- evidence-backed handoff
- minimal planner (một rule)
- provider-neutral orchestration
- tool owns its summary/evidence shape

Không nên:
- model-generated handoff sớm
- transcript compaction làm source of truth
- provider-owned memory policy
- centralize tri thức tool trong `evidence.rs`/`agent`

## 16. Gate — tiêu chí bật từng phase

### 16.1. Phase A (planner dedup) — đã trigger và đã fix post-ship

**Signal đã quan sát (trigger ship):**
- **Duplicate read loop** (signal #4 mở rộng). Ses_19d802b2734 cho thấy
  agent `Read` lại cùng file 3 lần trong cluster ~10-20 turn vì
  summary 85 chars không mang content. Agent stateless giữa các turn.
  → Phase A shipped (`6508100`, `3aae889`).

**Bug design quan sát ngay session sau (ses_19d8049f736, §2.3) → fix:**
- **Anchor quá hẹp fire 0 lần.** Duplicate thực tế cluster intra-turn,
  không cross-turn. → `d3b62ea` nới anchor + thêm dedup idempotent.
- **Path spelling không normalize.** 3 spelling cùng file → 3 key
  dedup → double-inject. → `1f86532` normalize_path.

**Bug design khi phase A thật sự fire (§2.4) → fix:**
- **Wire format reject 400.** User message sau assistant tool_use phải
  bắt đầu bằng tool_result, không được bắt đầu bằng Text. → `c61e8e8`
  dịch Text xuống sau tool_result cluster (vẫn sibling).
- **Prompt-injection defense + `Human:` drift.** Sibling Text sau
  tool_result bị model treat là user input (defensive
  `limit=302` reads) và render trên wire thành `Human:` → teach
  model emit `Human:` drift. → `a49bad9` mirror Claude Code:
  wrap `<system-reminder>` + smoosh vào `tool_result.content`. Pattern
  là invariant cứng từ training, không phải design choice.

**Bug design với Bash/partial-read (§2.5) → fix:**
- **Bash evidence silently dropped.** Filter
  `!r.related_files.is_empty()` loại sạch Bash/GhFile/WebFetch
  records vĩnh viễn. → `2b6398c` two-lane split.
- **Partial Read nhầm với entire file.** Summary `(302 lines)` cho
  slice 300 dòng của file 831 dòng không có dấu hiệu partial. →
  `235bfa5` tag `(partial: lines X-Y, …)`.

Bài học:
- Gate M2 phase A ship được trên scan data (§2.1) + cross-turn
  hypothesis, nhưng intra-turn pattern chỉ lộ ra khi observe session
  thực (§2.3).
- Wire-format contract và prompt-injection defense là 2 invariant ẩn
  Claude không document nhưng Claude Code source đã giải quyết —
  research source leak trước khi prototype injection shape mới
  (§2.4).
- Filter silently-drop rất dễ bỏ sót khi RFC ngầm định mọi record
  đồng dạng. Scan ngược design để tìm "class bị drop" khi thêm
  filter mới (§2.5).
- RFC không dự đoán được những thứ trên; rollout qua commit nhỏ +
  observe + reference upstream là đúng approach.

### 16.2. Phase B (handoff + provider switch) — pending

Phase B chỉ triển khai khi có ít nhất một signal cụ thể từ production:

- **Context overflow sau Phase A.** Session chạm context window (> 90%)
  sau khi dedup + inject đã chạy → cần handoff facts-only chèn đầu
  context để giảm thêm, hoặc cần ranking tốt hơn (unresolved > recency).
- **Provider switch fail rate đo được.** Nếu user switch provider giữa
  turn và provider mới loss context → cần handoff + prepared context
  rebuild (§7.2).
- **Cache hit rate giảm sau evidence.** Nếu prefix cache bị invalidate
  thường xuyên vì summary format drift → cần handoff để ổn định prefix
  (Phase A đã giữ evidence block format ổn định; handoff là layer kế).
- **Hallucination về verification status.** Nếu model claim "test pass"
  sau khi BuildLog fail → cần `handoff.verifications` chèn đầu context
  để model không miss.
- **Planner chọn sai evidence.** Phase A dedup theo recency; nếu user
  quay lại file cũ (ngoài window 15) thường xuyên → cần
  `unresolved hit > user-mentioned file > recency` ranking.
- **Stale evidence sau Edit.** Model commit nhầm dựa trên blob cũ vì
  planner inject blob của record trước Edit → cần invalidate record
  khi Edit cùng file. Có thể làm standalone không cần handoff, nhưng
  gộp vào Phase B để batch scope.

Không signal nào kích hoạt → Phase A đã đủ. Phase B chỉ là complexity
thừa.

Theo dõi signal này như thế nào là open question — có thể manual
inspection trong các session dev, sau đó bổ sung log cần thiết (không
add metric infrastructure sớm, RULES §27).
