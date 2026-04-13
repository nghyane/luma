# RFC: Evidence-backed Handoff for Multi-Provider Context Planning

- Status: M1 shipped. Phase A (context planner) explored, measured,
  rolled back. Phase B (handoff + provider switch) deferred
  indefinitely — wait for evidence of user need.
- Author: Nghia / Luma
- Updated: 2026-04-12

## 0. Implementation status

RFC này đã trải qua 3 giai đoạn thực tế: M1 ship (useful), Phase A
ship rồi rollback (architectural mistake), pull-mode resource scheme
(shipping). Thứ còn sống trong code hôm nay:

- **M1 — Evidence store (shipped).** Commits `6224251`, `880b31b`.
  Oversized tool results (≥ 8K chars) được promote sang
  `sessions/{id}/evidence/{ev_id}.txt`. Transcript giữ summary + optional
  `evidence_id`. Crash-safe write (tmp → fsync → rename → append
  record). Image path scoped sang `sessions/{id}/images/`.
- **M1 summary improvements (shipped).** Commits `1f86532`, `235bfa5`.
  `normalize_path` cho dedup potential (strip `./`, rewrite
  cwd-prefix → relative). Partial Read summary tag
  `(partial: lines X-Y, …)` khi args có `offset`/`limit`.
- **Phase A — Context planner (reverted).** Commits `6508100` →
  `22276c7` (8 commit) đã ship một `core/context_plan.rs` inject
  evidence blob vào wire payload trước `provider.stream()`. Sau 5
  session đo lường, rollback toàn bộ trong `deb5cbb` (revert). Lý do
  architectural — xem §3.
- **Pull-mode resource scheme (shipped).** Commits `3011a39`, `d0bbdb2`.
  Thay vì planner push evidence vào context, tool `Read` chấp nhận
  URI `artifact://{type}/{id}` để agent chủ động re-đọc khi nó
  quyết định cần:
  - `artifact://ev/{id}` — re-read evidence blob đã lưu (session-scoped).
  - `artifact://skill/{name}` — load `SKILL.md` body (frontmatter
    stripped, đã có trong catalog system prompt).
  Scheme mở để thêm `artifact://session/…`, `artifact://trace/…` sau
  mà không mở rộng tool registry. Xem §5.5, §5.6.
- **Phase B — Handoff + provider switch (deferred indefinitely).**
  Không có plan cụ thể. Đợi evidence user cần.

## 1. Tóm tắt ban đầu (outdated, giữ làm historical reference)

RFC này ban đầu đề xuất tách 3 trách nhiệm hiện đang bị dồn vào
`session.messages` thành 3 lớp riêng:

1. `transcript`: lịch sử hội thoại canonical
2. `evidence`: dữ liệu dài, log, excerpt, output cần truy hồi
3. `handoff`: working memory có cấu trúc để agent tiếp tục task hoặc đổi
   provider

Sau thực nghiệm: M1 (evidence store) có giá trị và được giữ. Phase A
(planner inject evidence vào context) **không** hoạt động vì assumption
về model trust sai. Handoff không có user signal nên defer.

## 2. Động cơ ban đầu (valid)

Hiện trạng khi viết RFC:

- Tool output dài làm phình `session.messages` (hard truncate ở
  `AGENT_RESULT_SAFETY_CAP = 32_000` trong `turn.rs`).
- Hard truncate làm mất thông tin và khó debug.
- Transcript đang vừa là lịch sử, vừa là working memory, vừa là provider
  input.

M1 đã giải quyết điểm 1 và 2 — đây là lý do M1 vẫn đang ship.

### 2.1. Feasibility scan (175 sessions, pre-M1)

Scan từ `core::session::tests::rfc_feasibility_scan`:

- **Tool output thực sự phình transcript.** 512 tool_result: p50 = 382
  chars, p90 = 9.6K, p99 = 31.9K, max = 32_012 (đã hit cap).
- **Hard-truncate đang mất info.** 26/512 tool_result (5.1%) đã mang
  marker `[truncated]`.
- **Ngưỡng 8K là sweet spot.** Threshold 8K: 70 tool_result (13.7%)
  thành blob → giảm 73% bytes tool_result trong transcript.
- **`EvidenceKind` tối thiểu đủ cover.** Distribution ≥ 8K: Read 30,
  Bash 19, GhFile 7, exec_command 7, Grep 4, GhLs 3 → khớp 5 variant.

Scan này vẫn đúng và drive M1 design ngày nay.

### 2.2. Bằng chứng M1 production (ses_19d802b2734)

Session ~60 turn đầu tiên dùng M1 clean:

- 13/13 tool_result ≥ 8K được promote. 0 orphan.
- Summary footprint 82-87 chars vs raw ~15K → giảm ~99.4%.
- Duplicate reads observed: types.rs × 3, turn.rs × 3. Motivated
  Phase A.

## 3. Phase A retrospective — vì sao rollback

Phase A (`core/context_plan.rs`) giả định: agent cần help để reason
cross-turn. Planner inject lại evidence blob vào next turn's wire payload
sẽ giúp agent thấy lại content đã bị spill ra ngoài transcript.

Ship progression: 8 commit, mỗi commit fix 1 bug mà commit trước tạo ra.

1. `6508100` scaffold passthrough.
2. `3aae889` inject deduped evidence.
3. `d3b62ea` nới anchor sau khi phát hiện anchor hẹp không fire.
4. `c61e8e8` fix 400 Bad Request (tool_use without tool_result).
5. `a49bad9` wrap `<system-reminder>` + smoosh vào `tool_result.content`
   (mirror Claude Code).
6. `2b6398c` two-lane dedup cho Bash/GhFile evidence không có
   `related_files`.
7. `235bfa5` partial Read summary tag (giữ lại).
8. `22276c7` pin site theo `tool_use_id` cho cache stability.

Mỗi fix technically đúng. Tổng thể architecturally sai. Bằng chứng cuối
cùng từ ses_19d8085556a: assistant thinking **explicit 5 lần** rằng:

> "The `<system-reminder>` blocks appearing in tool output are
> prompt-injection noise... I'll ignore them..."

Rồi pivot sang Bash `sed`/`cat` để bypass Read, tạo thêm 3 evidence
records cho cùng 1 file. Feature phản tác dụng: agent trust giảm, tool
call tăng.

### 3.1. Root cause — 3 architectural mistake

1. **Assume agent cần help nhớ.** Agent không phiền khi call Read lại
   — tool call cheap. Duplicate Read là symptom, không phải disease.
   Disease thực là "tool output summary quá ngắn không đủ context";
   disease đó giải quyết được bằng cách cho tool output **chính nó**
   informative hơn, không phải bằng cách inject từ bên ngoài.

2. **Mirror Claude Code shape mà không mirror training.** Claude Code
   dùng `<system-reminder>` tag với system prompt contract dạy model
   cách trust tag đó. Ta mirror shape, không có contract. Model default
   là suspect — và đã làm đúng như vậy trong ses_19d8085556a.

3. **Thêm layer trước khi đo baseline.** M1 raw chưa bao giờ được đo
   dài ngày. Phase A build lên giả định duplicate Read là problem; data
   sau rollback cho thấy duplicate Read tồn tại nhưng không critical
   (session dài nhất 699 msg chỉ có 45× cùng file trong marathon
   discussion).

### 3.2. Invariant mới học được

**Invariant A — Transcript persist bytes.** Planner không được gửi
bytes lên wire mà không có trong session.json. Nếu cần, **persist trước,
send sau**. Phase A vi phạm: mutation chỉ sống trong clone, wire có bytes
mà disk không có → cache prefix drift + agent confusion về nguồn nội
dung.

**Invariant B — Trust đến từ training hoặc user, không từ shape.**
Bất cứ markup/tag nào cần model treat khác thường phải có upstream
training signal (Claude built-in, system prompt contract, hoặc user
content). Shape không tạo trust.

**Invariant C — Đo trước khi thêm layer.** M1 raw baseline phải có data
ít nhất 1 tuần dev usage trước khi argue cho layer mới. Architecture
decision không trên hypothesis.

## 4. Architecture hiện tại (post-revert, post-artifact-scheme)

```
user message  →  session.messages (canonical)  →  provider.stream()
                        ↑
                        ↓
                  tool_use loop
                        ↓
               tool result (≥ 8K?)
                   /         \
                 yes           no
                 ↓             ↓
           evidence store     inline in transcript
           + summary (advertises artifact://ev/{id})
           in transcript

                  ┌──────────────────────────────────┐
  agent decides   │  Read { path: "artifact://ev/…"  │
  to re-read      │         or "artifact://skill/…"  │
                  │         or "/abs/path"        }  │
                  └────────────┬─────────────────────┘
                               ↓
                  core/session::resolve_resource_path
                               ↓
                  dispatch by artifact sub-type:
                     ev   → sessions/{id}/evidence/{id}.txt
                     skill → discovered SKILL.md + strip frontmatter
                     _    → plain filesystem path
                               ↓
                  Read tool reads, applies optional transform,
                  returns line-numbered body
```

Components hiện ship:

- `core::evidence` — `classify`, `ingest` (tmp→fsync→rename), data model.
- `core::session.evidence: EvidenceStore` — serde-default field.
- `core::session::Resolved` enum + `resolve_resource_path` — URI dispatch.
- `core::session::scope_current_session` — task-local session_id propagation.
- `ContentBlock::ToolResult.evidence_id: Option<String>` — internal ref,
  adapter drop trước khi gửi provider.
- `sessions/{id}/evidence/{ev_id}.txt` — blob storage.
- `maybe_promote_to_evidence` trong `turn::run_turn` — promote hook.
- `SAFETY_FALLBACK_CAP = 32_000` — fallback cap khi ingest I/O fail.
- `config::skills::build_catalog` — advertise `artifact://skill/{name}`
  thay vì absolute path.

Components bị xoá (revert):

- `core::context_plan` toàn bộ module (planner).
- `build_prepared_messages` và wire vào `stream_with_retry`.
- Pub escape trên `session_assets_dir` (revert về private).

## 5. Data model

### 5.1. `EvidenceKind`, `EvidenceRecord`, `EvidenceStore`

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    ReadExcerpt,   // Read
    GrepResult,    // Grep, GhSearch
    BashLog,       // Bash, exec_command, shell
    BuildLog,      // cargo/npm/pytest/… detected từ command prefix
    Other,         // fallback (GhFile, WebFetch, …)
}

pub struct EvidenceRecord {
    pub id: String,
    pub kind: EvidenceKind,
    pub tool_use_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_path: Option<String>,
    pub chars: usize,
    pub turn_index: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_files: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct EvidenceStore {
    #[serde(default)]
    pub records: Vec<EvidenceRecord>,
}
```

`related_files` vẫn populate qua `extract_related_files` +
`normalize_path` (strip `./`, cwd-prefix → relative). Hiện tại chỉ
dùng làm metadata debug. Nếu Phase B quay lại planner, đây là dedup
key sẵn sàng.

### 5.2. `ContentBlock::ToolResult` — `evidence_id` field

```rust
ToolResult {
    tool_use_id: String,
    content: String,              // summary (advertises artifact://ev/{id})
                                  // hoặc full result khi < 8K
    is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_id: Option<String>,  // internal handle, không lên wire
}
```

Provider adapter (Claude, OpenAI, Codex) destructure thủ công và drop
`evidence_id` khi serialize lên API — field này không có nghĩa với
model. **Bản thân URI nằm trong `content`** (dòng `stored as
artifact://ev/{id}`) — đó là nơi model thấy và có thể feed lại qua
Read. `evidence_id` chỉ phục vụ debug/tooling phía app.

Trade-off: `content` và `evidence_id` redundant về thông tin (URI có
thể derive từ `evidence_id`). Giữ cả hai vì:

- `content` là bytes model thực sự thấy; contract wire-visible.
- `evidence_id` là typed handle cho code Rust, không cần parse
  summary string để tìm id.

### 5.3. Session asset layout

```
sessions/
  {id}.json
  {id}/
    images/{image_id}            # multimodal
    evidence/{evidence_id}.txt   # text blobs only
```

Tách riêng vì khác lifecycle: image immutable resolve-at-send-time,
evidence blob có thể multiple per session.

### 5.4. `Session` giữ flat

```rust
pub struct Session {
    pub id, title, created_at, updated_at: String,
    pub messages: Vec<Message>,
    #[serde(default)] pub usage: SessionUsage,
    #[serde(default)] pub turn_durations: Vec<f64>,
    #[serde(default)] pub evidence: EvidenceStore,
}
```

Không tách `SessionMeta`/`Transcript`/`SessionStats`. Tách là cosmetic
khi struct có 5-6 field; đợi Phase B thêm nếu có field mới.

### 5.5. Resource scheme `artifact://`

Một scheme duy nhất, sub-typed. Model chỉ cần nhớ một URI form qua
mọi loại tài nguyên:

```
artifact://{type}/{id}

type = ev     → evidence blob của session hiện tại
type = skill  → SKILL.md body của skill đã discover
type = …      → mở cho session://, trace://, cache:// sau
```

**API:**

```rust
pub enum Resolved {
    Path(PathBuf),                  // read verbatim
    PathStripFrontmatter(PathBuf),  // drop leading ---…--- YAML
}

pub fn resolve_resource_path(path: &str) -> io::Result<Resolved>;
```

Plain filesystem path (không có `://`) pass-through thành
`Resolved::Path`. Tool `Read` là consumer đầu tiên; tool khác có thể
adopt mà không cần đổi trait.

**Security:**

- `is_safe_id_segment` guard dùng chung: reject empty, `/`, `\`, `..`.
  Block path traversal trước khi ghép đường dẫn.
- `ev` resolver yêu cầu `scope_current_session` active (task-local).
  Ngoài scope → NotFound. Cross-session access chưa support.

**Session context propagation:**

`core::session::CURRENT_SESSION` là `tokio::task_local!<String>`.
Agent loop wrap tool execution trong
`scope_current_session(session_id, execute_tools(...))` nên tool
observe đúng id khi run. Tránh phải thêm `session_id` vào `Tool` trait
— scope propagation thuần qua tokio runtime.

### 5.6. Skill URI (`artifact://skill/{name}`)

Skill catalog ở system prompt giờ advertise URI thay vì absolute path:

```xml
<skill name="commit-work">
  <description>…</description>
  <location>artifact://skill/commit-work</location>
  <directory>/Users/me/.agents/skills/commit-work</directory>
</skill>
```

Khi agent gọi `Read { path: "artifact://skill/commit-work" }`:

1. `resolve_resource_path` dispatch → `resolve_skill("commit-work")`.
2. Resolver gọi `config::skills::discover()` để match theo name, trả
   `PathStripFrontmatter(<abs path>)`.
3. `Read` mở file, tính `count_frontmatter_lines` (đếm hai fence
   `---`), skip số dòng đó, numbering bắt đầu từ 1 ở body.

**Lý do strip frontmatter:** name + description đã nằm trong catalog
system prompt. Body model đọc không cần lặp lại → tiết kiệm 4-10
dòng × số skill load.

**Lý do vẫn giữ `<directory>` raw path:** skill có thể reference
relative file (`./examples/`); model cần absolute prefix để resolve.
Không leak implementation khi skill chưa chạy — chỉ user cố tình gọi
Read skill mới thấy path thật trong body.

## 6. Cách hoạt động

### 6.1. Normal turn

1. User message push vào `session.messages`.
2. `provider.stream(&session.messages)` — không planner, không inject.
3. Stream trả assistant text / tool_use.
4. Agent loop wrap tool execution trong
   `scope_current_session(session.id, execute_tools(...))`.
5. Tool thực thi. Read path có thể là:
   - filesystem absolute path (phổ biến, pass-through).
   - `artifact://ev/{id}` — re-đọc blob của session này.
   - `artifact://skill/{name}` — load skill body (frontmatter stripped).
6. `maybe_promote_to_evidence(...)` cho mỗi result:
   - `len < 8K` → inline, `evidence_id = None`.
   - `len ≥ 8K` → `classify` → `ingest` (tmp→fsync→rename) →
     summary (có URI `artifact://ev/{id}`) + `evidence_id`.
7. Tool_result blocks push vào transcript.
8. Session save.

### 6.2. Crash recovery

Write order:
1. Ghi blob ra `evidence/{id}.txt.tmp`, fsync.
2. Rename sang `.txt`.
3. Append record vào `EvidenceStore.records`.

Crash giữa 1-2 → blob orphan (vô hại, không có record trỏ vào).
Crash giữa 2-3 → blob durable trên disk nhưng record không append
vào session.json. Session load vẫn clean.

### 6.3. Abort / `fix_orphaned_tool_uses`

Placeholder `[aborted]` là `ContentBlock::ToolResult` hợp lệ với
`evidence_id = None`. Không đặc biệt hoá.

## 7. Quyết định đã chốt

- **`EvidenceKind`:** 5 variant (ReadExcerpt, GrepResult, BashLog,
  BuildLog, Other). Khớp tool distribution §2.1.
- **`EVIDENCE_PROMOTION_THRESHOLD = 8_000`:** từ scan §2.1 (73% bytes
  reduction với 13.7% promotion rate).
- **`SAFETY_FALLBACK_CAP = 32_000`:** fallback khi ingest I/O fail
  (hiếm, defense-in-depth).
- **Summary template centralize** ở `core::evidence::classify` — 5
  tool cover hết, không cần `Tool` trait extension.
- **Summary format carry URI:** `stored as artifact://ev/{id}` thay
  vì `stored as evidence`. Model thấy URI trực tiếp, có thể feed lại
  qua Read mà không phải reconstruct. Template dùng `{id}`
  placeholder ở classify time, `ingest` substitute thật sau khi gen
  id.
- **`EvidenceDraft.blob: String`** (non-optional) — caller decide
  promote trước khi gọi ingest.
- **Image vs evidence:** tách (`images/` vs `evidence/`).
- **Migration:** không. Beta → breaking change.
- **Partial Read summary tag:** `(partial: lines X-Y, …)` khi có
  offset/limit (235bfa5).
- **Path normalization:** `normalize_path` ở classify time (1f86532).
  Strip `./`, rewrite cwd-prefix absolute → relative. Không dùng
  `canonicalize()` (Write có path chưa tồn tại).
- **Single scheme `artifact://{type}/{id}`:** thay vì scheme-per-type
  (`ev://`, `skill://`). Model chỉ cần nhớ 1 URL form; new resource
  types slot in qua typed resolver chứ không qua scheme list.
- **Session scope qua `tokio::task_local`:** không extend `Tool`
  trait với session context. Resolver tự lấy session id từ
  task-local khi cần. Tool nào không dùng URI → không biết tới
  scope, zero impact.
- **Skill body strip frontmatter:** frontmatter đã nằm trong catalog
  system prompt; Read không cần trả lại. Tiết kiệm 4-10 dòng mỗi
  skill load.

## 8. Rủi ro vẫn canh

- **Evidence orphan trên disk.** Crash giữa tmp→rename → blob orphan.
  Không có record → không GC. Không cần cleanup cho tới khi disk
  pressure thành vấn đề.
- **I/O overhead per oversized tool.** Blob fsync + rename mỗi
  tool_result ≥ 8K. Chưa đo ở production.
- **Stale blob sau Edit.** `ev_100` đọc file X, sau đó `Edit X` → blob
  cũ vẫn trên disk. Với pull-mode scheme (§5.5), nếu agent gọi
  `Read artifact://ev/ev_100` sau Edit, nó sẽ nhận content cũ mà
  không biết. Mitigation hiện tại: summary trong transcript ghi rõ
  turn_index của blob, agent có thể reason "blob ghi trước Edit →
  stale". Nếu signal này không đủ, cân nhắc invalidate blob khi
  Edit same path (chưa làm).
- **Skill discovery race.** `resolve_skill` gọi `discover()` mỗi lần
  → walk project + user dirs. Nếu skill bị xoá giữa lúc catalog
  build và lúc Read, resolver trả NotFound. Agent có thể retry hoặc
  user sửa. Rare, không cần cache.
- **Provider adapter quên drop `evidence_id`.** Adapter phải
  destructure thủ công; adapter mới có thể quên → `evidence_id`
  leak lên wire và model confuse. Mitigation: test per-provider
  assert `evidence_id` không xuất hiện trong payload (đã có ở
  codex test).

## 9. Phase A rollback postmortem

### 9.1. Session evidence

- `ses_19d8049f736` (§2.3 cũ): anchor fire 0 lần vì guard quá hẹp.
  → fix d3b62ea.
- `ses_19d8085556a` (§2.4 cũ, **key evidence**): wrapper fire, agent
  thinking explicit ignore 5 lần, pivot sang Bash bypass.
- `ses_19d80798d45` (§2.5-2.6 cũ): cache hit ratio scan, chỉ ra
  prefix drift ~25K/iter trước fix 22276c7. Fix giảm xuống ~1-6K/iter
  nhưng không đổi agent behavior.

### 9.2. Bài học cho Phase B (nếu có)

Bất cứ feature nào inject content vào wire phải:

1. **Persist bytes vào session trước khi send.** Nếu byte X gửi lên
   wire ở turn N, byte X phải có trong session.json sau turn N. Không
   có exception.
2. **Bằng chứng upstream training cho bất kỳ trust tag nào.** Link tới
   Claude Code source / Anthropic docs. Shape-only mimicry không đủ.
3. **Đo trước khi ship.** Ít nhất 3 session dev với baseline hiện có.
   Có data thì mới justify layer.
4. **Rollback plan viết trước khi code.** Biết exit trước khi enter.

### 9.3. Pull > push cho resource access

Một lesson độc lập từ scheme `artifact://`: **cho model chủ động
request resource cheaper và an toàn hơn là bí mật inject**. Lý do:

- Pull là model-initiated → không trigger prompt-injection defense.
  Không cần `<system-reminder>` wrapper, không cần training signal
  đặc biệt.
- Pull là wire-visible ở cả hai chiều: request (tool_use args) và
  response (tool_result content). Debug/replay trivial.
- Pull zero-cost khi không dùng. Push mất bytes mỗi turn kể cả khi
  agent không cần.
- Pull framework extensible — thêm resource type = thêm resolver, không
  thay trait, không thay system prompt contract.

Push vẫn có chỗ dùng: khi dữ liệu thực sự cần mỗi turn (ví dụ
verification status trong handoff snapshot). Nhưng default nên là
pull; push phải có justification rõ.

## 10. Phase B — defer

Các phần sau không triển khai cho tới khi có user signal:

- `core/handoff.rs` — handoff snapshot deterministic.
- Planner với ranking (`unresolved > user-mentioned > recency`).
- Provider switch rebuild context.
- `ProviderCacheHint` optimization.
- `Session` sub-struct split.

Signal để consider Phase B:

- User report mất context khi switch provider giữa task.
- Session chạm context window dù evidence đã spill.
- Hallucination verification status (model claim "test pass" sau
  BuildLog fail).

Không signal nào kích hoạt → M1 đủ.

Theo dõi signal này cách nào: manual inspection session.json qua
script audit khi rỗi. Không add metric infrastructure (RULES §27).

## 11. Non-goals

- Semantic memory / vector DB.
- Model-generated handoff.
- Provider-owned memory policy.
- Token-optimized budget trước khi có latency measurement thật.

## 12. Khuyến nghị forward

- Giữ M1 + scheme `artifact://`. Đừng thêm push-mode layer nữa trừ
  khi 3 invariant §3.2 thoả **và** pull-mode không đủ.
- Thêm resource type vào scheme (`artifact://session/…`,
  `artifact://trace/…`) khi có use case cụ thể. Cost = 1 resolver
  function + tests.
- Nếu duplicate Read vẫn thành vấn đề dù đã có pull URI: cân nhắc
  preview-on-promote (tool output tự carry đầu blob trong summary).
  Reversible, không cross transcript boundary.
- ROADMAP v0.5 có MCP support, file watcher, custom tools — đây là
  user-visible value. Ưu tiên cao hơn Phase B.
