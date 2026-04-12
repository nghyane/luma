# RFC: Evidence-backed Handoff for Multi-Provider Context Planning

- Status: M1 shipped — M2 (handoff, planner, provider switch) deferred pending metrics
- Author: Nghia / Luma
- Updated: 2026-04-12

## 0. Implementation status

RFC này được ship theo 2 milestone thay vì 6 phase tuần tự (§10):

- **M1 — Evidence store (shipped).** Commits `ec5cc73`, `6224251`, `880b31b`.
  Oversized tool results (≥ 8K) spill sang `sessions/{id}/evidence/{ev_id}.txt`,
  transcript giữ summary + `evidence_id`. Crash-safe write (tmp → fsync →
  rename → append record). Image path scoped sang `images/`. Provider
  adapters không đổi (destructure thủ công, `evidence_id` không lên wire).
- **M2 — Handoff + planner + provider switch (deferred).** Không implement
  cho tới khi M1 deploy và có metrics thực (§16) cho thấy cần. Section §4.1
  handoff, §5 `core/handoff.rs` + `core/context_plan.rs`, §7.2 provider
  switch, §9 planner rules đều thuộc M2.

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

### `core/context_plan.rs`
Phụ trách (**M2**):
- build prepared messages trước mỗi `provider.stream()`
- chọn evidence nào cần load
- enforce context budget ở mức planner (char-based)
- log quyết định planner để debug

Planner giữ **một rule duy nhất** (§9). Rule mới chỉ được thêm kèm
test regression.

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

### 6.5. Tool summary extension (deferred to M2)

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

## 7. Cách hoạt động

### 7.1. Normal turn

1. User message được push vào `transcript.messages`
2. `context_plan::build_prepared_messages(&transcript, &evidence, &handoff, budget)`
3. provider stream trả về assistant text / tool_use
4. tools được execute
5. `evidence::ingest_tool_result(...)` — blob write-first, sau đó mới update record
6. transcript lưu summary ngắn + evidence ref (thay thế hard truncate hiện tại)
7. `handoff::refresh_*`
8. session được save (record ở status `Persisted` sau khi blob fsync)

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

### Evidence loading (M2)

M2 **chỉ** implement một rule:

> Load N evidence gần nhất (theo `turn_index`) mà `related_files` giao với
> `handoff.files_in_play`. Budget char cứng; vượt budget thì skip evidence cũ
> nhất trước.

Mọi rule thêm vào sau phải đi kèm:
- test regression (input handoff + evidence → output selection)
- log quyết định (which loaded, which dropped, why) vào session debug log

Rule-set chỉ mở rộng khi có evidence thực từ §16 metrics.

### Budget

- M2 bắt đầu: char-based (deterministic, không provider-specific).
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

### M2 — Handoff + planner + provider switch (deferred)

Các phần sau không triển khai cho tới khi có dữ liệu thực từ M1:

- `core/handoff.rs` — handoff snapshot deterministic (§4.1, §8).
- `core/context_plan.rs` — một rule duy nhất (§9), char-based budget.
- Provider switch rebuild prepared context (§7.2).
- `ProviderCacheHint` optimization.
- `Session` sub-struct split (§6.1) — re-evaluate khi struct thêm 2-3
  field để biết shape đúng.

Gate để bật M2: xem §16.

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
  theo `turn_index`. **M2 scope** — chưa ship.
- **Budget:** char-based ở M2; token-estimate chỉ cân nhắc sau metrics.
- **Image vs evidence:** giữ tách riêng (`images/` vs `evidence/`). Khác
  lifecycle, khác resolver. Ship ở M1.
- **Migration:** không. Beta → breaking change. Session cũ tham chiếu image
  flat không fallback.
- **Provider-specific planner tuning:** không ở M1-M2. Nếu metrics
  cho thấy cần, thêm riêng mà không đổi API planner.
- **`Session` sub-struct split:** không ở M1. Flat thêm 1 field không gây
  đau; tách khi M2 đưa handoff/providers vào sẽ biết shape đúng.

## 14. Rủi ro chính cần canh

- **Planner heuristic drift.** Một khi chạy được, rất dễ thêm rule mà không
  test. Ép nguyên tắc "mỗi rule mới = 1 test regression". (M2)
- **I/O overhead.** Mid-turn save hiện đã ghi session.json mỗi batch. M1
  thêm blob fsync + rename mỗi tool_result ≥ 8K. Chưa đo ở production; nếu
  metrics cho thấy latency tăng đáng kể, cân nhắc batch hoặc async write.
- **Evidence orphan.** Crash giữa tmp write và rename → blob orphan (vô
  hại, chỉ tốn disk). Không có record → planner không thấy. Không cần GC
  chủ động cho tới khi disk pressure thành vấn đề.

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

## 16. M2 gate — tiêu chí bật handoff/planner

M2 (handoff, context planner, provider switch) chỉ triển khai khi có ít
nhất một signal cụ thể dưới đây từ M1 production:

- **Context overflow thực tế.** Session chạm context window (> 90%) với
  evidence blobs đã full-size nhưng transcript vẫn vượt budget →
  planner cần select evidence chứ không phải load hết.
- **Provider switch fail rate đo được.** Nếu user switch provider giữa
  turn và provider mới loss context → cần prepared context rebuild (§7.2).
- **Cache hit rate giảm sau evidence.** Nếu prefix cache bị invalidate
  thường xuyên vì summary format drift → cần handoff để ổn định prefix.
- **Hallucination do summary không đủ.** Nếu model citing sai vì summary
  200 chars ghi thiếu → cần planner reload evidence blob.

Không signal nào kích hoạt → M1 đã đủ. M2 chỉ là complexity thừa.

Theo dõi signal này như thế nào là open question — có thể manual
inspection tuần đầu, sau đó bổ sung log cần thiết (không add metric
infrastructure sớm, RULES §27).
