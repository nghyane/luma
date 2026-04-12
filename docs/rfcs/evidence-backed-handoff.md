# RFC: Evidence-backed Handoff for Multi-Provider Context Planning

- Status: Draft
- Author: Nghia / Luma
- Updated: 2026-04-12

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
Phụ trách:
- build prepared messages trước mỗi `provider.stream()`
- chọn evidence nào cần load
- enforce context budget ở mức planner (char-based ở phase 4)
- log quyết định planner để debug

Ở phase 4 planner giữ **một rule duy nhất** (§9). Rule mới chỉ được thêm kèm
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
không đảm bảo đọc được sau Phase 1. Không thực hiện migration; session cũ
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
- **Tạo:** Phase 2 viết evidence blob write-first + fsync + rename (§7.3).
- **Xóa session:** xóa cả thư mục `sessions/{id}/` — cover tất cả assets.
- **Fork session (nếu có):** clone thư mục; id của asset không đổi.
- **Quota:** phase 1 không áp; phase 6 đo `chars` tổng và cảnh báo nếu
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
  cho session Phase 1 format; không cam kết cho session pre-RFC.

### 6.5. Tool summary extension

`format_tool_summary` và `format_tool_result` hiện ở `core/agent/summary.rs`
đang centralize tri thức về mọi tool trong agent layer. RFC chuyển sang cho
tool tự khai báo:

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

`evidence.rs` chỉ nhận `EvidenceDraft` và persist. Dependency vẫn theo chiều
`agent → tool` (RULES §4), không đảo.

Migration trait: giữ `agent/summary.rs` làm fallback cho phase 2; từng tool
chuyển dần sang trait method riêng.

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

Tool result lớn → evidence → session.json phải an toàn trước crash giữa
chừng (áp dụng từ Phase 2 trở đi, khi evidence thực sự ghi disk). Quy tắc:

1. Ghi blob ra `evidence/{id}.txt.tmp`, `fsync`, rename sang `.txt`.
2. Chỉ sau khi rename thành công, append record với `status = Persisted`.
3. Nếu crash giữa (1) và (2): record chưa tồn tại → blob orphan. Phase 2
   chấp nhận orphan; phase 3+ có thể thêm scan GC khi load session nếu
   cần.
4. Nếu record có `status = Pending` khi load (trường hợp crash sau khi
   append record nhưng trước khi rename blob), planner bỏ qua evidence
   đó nhưng giữ summary trong transcript.

`fix_orphaned_tool_uses` (`agent.rs`) cũng cần sinh placeholder hợp lệ khi
tool_result chứa evidence ref thay vì inline text — cover trong phase 2 test.

### 7.4. Tương thích với mid-turn save

`turn.rs` hiện save sau mỗi assistant message và sau mỗi tool result batch.
Thứ tự mới:

1. assistant message → push vào transcript → save session
2. execute tools
3. với mỗi result lớn: blob write-first (§7.3) → append evidence record
4. push tool_result blocks (chứa evidence ref) vào transcript
5. save session

Phải đo I/O overhead ở phase 2 trước khi scale up (RULES §27).

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

### Evidence loading (phase 4)

Phase 4 **chỉ** implement một rule:

> Load N evidence gần nhất (theo `turn_index`) mà `related_files` giao với
> `handoff.files_in_play`. Budget char cứng; vượt budget thì skip evidence cũ
> nhất trước.

Mọi rule thêm vào sau phải đi kèm:
- test regression (input handoff + evidence → output selection)
- log quyết định (which loaded, which dropped, why) vào session debug log

Rule-set chỉ mở rộng khi có evidence thực từ phase 6 metrics.

### Budget

- Phase 4: char-based (deterministic, không provider-specific).
- Phase 6: bổ sung token-estimate per-provider khi đã có metrics.

Lý do: tokenizer khác nhau giữa Anthropic/OpenAI; pick sớm sẽ bias planner.

## 10. Kế hoạch rollout

Beta stance: project đang ở beta. Schema và path là breaking change chấp nhận
được. **Không migration, không backward-compat layer.** Session pre-RFC có
thể không load được sau Phase 1 — đó là trade-off đã chọn.

### Phase 0 — Ổn định
- dọn `AGENT_RESULT_SAFETY_CAP` site và các truncation dang dở (đã done ở
  commit trước: rename từ `MAX_RESULT_LEN`, share `TRUNCATION_MARKER`)
- chuyển image path flat → `images/{id}` (§6.3). Breaking change đối với
  session cũ tham chiếu image flat; không fallback.
- đảm bảo `check/clippy/test/build` sạch
- mục tiêu: path layout ổn định trước khi `EvidenceStore` đụng disk.

### Phase 1 — Schema
- tách `Session` thành sub-structs (§6.1)
- thêm `EvidenceStore`, `HandoffSnapshot`, `ProviderThreads`
- round-trip test cho format mới (fresh session tạo → save → load → equal)
- **không migration**: session pre-RFC không đảm bảo load. `Session::load()`
  tiếp tục trả `None` cho format không parse được — tương tự hành vi hiện
  tại với 107/175 legacy session (§2.1).

### Phase 2 — Evidence
- implement `core/evidence.rs`
- chuyển `AGENT_RESULT_SAFETY_CAP` truncate thành ingest evidence khi kết
  quả vượt ngưỡng 8K; dưới ngưỡng vẫn inline trong `content`
- thêm `evidence_id` vào `ContentBlock::ToolResult` (§6.4) — breaking wire
  format, provider adapter ignore field này khi serialize lên API
- crash-safe write order (§7.3)
- `fix_orphaned_tool_uses` tương thích evidence ref
- từng tool chuyển sang `classify_evidence` trait method

### Phase 3 — Handoff
- implement `core/handoff.rs`
- deterministic refresh sau tools / verification / provider switch
- `recent_evidence_ids` bounded
- handoff rebuild purely từ transcript + evidence (không state ẩn)

### Phase 4 — Context plan (minimal)
- implement `core/context_plan.rs`
- **một rule duy nhất** (§9)
- char-based budget
- log quyết định planner

### Phase 5 — Provider handoff
- rebuild prepared context khi đổi provider
- `ProviderCacheHint` làm optimization opt-in

### Phase 6 — Metrics + tuning
- context size
- evidence usage (hit/miss per turn)
- provider switch success
- mismatch giữa handoff và verification
- chỉ sau phase này mới bàn thêm rule planner hoặc token-estimate

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
- Thêm I/O (blob writes) — cần đo ở phase 2
- Cần test kỹ heuristic trước khi mở rộng thêm

Tuy vậy, trade-off này chấp nhận được vì:
- source of truth rõ hơn
- multi-provider bền hơn
- correctness tốt hơn hard truncate / transcript compaction

## 13. Quyết định đã chốt

Các câu hỏi mở trước đây được chốt như sau (có bằng chứng từ scan ở §2.1):

- **`EvidenceKind` tối thiểu phase 1:** `ReadExcerpt`, `GrepResult`, `BashLog`,
  `BuildLog` (build/test/clippy), `Other`. Khớp với tool distribution thực tế.
- **Evidence threshold:** 8_000 chars. Dựa trên scan: giảm 73% tool_result bytes
  trong transcript với chỉ 13.7% tool_result (70/512) được promote thành blob.
- **`recent_evidence_ids` bounded:** `HANDOFF_RECENT_EVIDENCE_MAX = 16`. FIFO
  theo `turn_index`.
- **Summary template:** tool tự khai báo qua trait method (§6.5). `evidence.rs`
  chỉ nhận `EvidenceDraft`. Scan confirm: `Edit` tool đã tự bound 88 chars max.
- **Budget:** char-based ở phase 4; token-estimate chỉ cân nhắc sau phase 6
  khi có metrics.
- **Image vs evidence:** giữ tách riêng (`images/` vs `evidence/`). Khác
  lifecycle, khác resolver.
- **Migration:** không. Beta → breaking change. Session pre-RFC không đảm
  bảo load được; tham chiếu image flat không fallback.
- **Provider-specific planner tuning:** không ở phase 1-5. Nếu phase 6 metrics
  cho thấy cần, thêm riêng mà không đổi API planner.

## 14. Rủi ro chính cần canh

- **Planner heuristic drift.** Một khi chạy được, rất dễ thêm rule mà không
  test. Ép nguyên tắc "mỗi rule mới = 1 test regression".
- **I/O overhead.** Mid-turn save hiện đã ghi session.json mỗi batch. Thêm
  blob writes → đo trước khi scale.
- **Evidence orphan / ref gãy.** Crash giữa write blob và persist record →
  record `Pending` hoặc blob orphan. `status` field + loader skip Pending
  để chịu được; blob orphan không gây lỗi, chỉ tốn disk (scan GC khi cần).

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
