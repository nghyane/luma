# RFC 0006: Prompt & Tool System Redesign

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0006                                         |
| Title            | Prompt & Tool System Redesign                |
| Status           | Draft                                        |
| Author(s)        | nghia                                        |
| Created          | 2026-04-14                                   |
| Updated          | 2026-04-14                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Redesign toàn bộ hệ thống prompt và tool descriptions để tách biệt rõ
hai tầng: **behavior** (system prompt dạy agent làm gì) và **capability**
(tool description mô tả tool làm gì). Hiện tại hai tầng này bị trộn lẫn,
gây lặp lại instruction ở nhiều nơi và thiếu behavior quan trọng trong
`smart.md`. Thay đổi bao gồm: thêm `base.md` shared, rewrite `smart.md`,
trim tool descriptions, và rút gọn `tools_native/patch.md`.

## Motivation

### Vấn đề 1: Tool descriptions dạy workflow, không mô tả capability

Đo từ code (`src/tool/*.rs`):

- "Call in parallel" xuất hiện trong descriptions của: `Read`, `Grep`,
  `GhFile`, `GhLs`, `GhSearch`, `WebFetch` — 6 tool descriptions.
- `tools_native.md` line 3: "Maximize parallel tool calls for independent
  operations." — lặp lần thứ 7.
- `tools_patch.md` line 3: tương tự — lặp lần thứ 8.
- "Read before editing / Never guess": có trong `Edit`, `MultiEdit`,
  `tools_native.md`, `tools_patch.md`, `deep.md` — 5 lần.
- `Bash` description: 14 dòng, trong đó 5 dòng liệt kê "do NOT use for
  file operations" với sub-bullets cho từng tool thay thế.

Anthropic's "Writing tools for agents" guide (đọc trực tiếp): "Lots of
redundant tool calls might suggest some rightsizing" — tool description
dài/mơ hồ gây redundant calls. Tool description nên mô tả capability và
boundary, không dạy workflow.

### Vấn đề 2: smart.md thiếu behavior mà rush.md và deep.md đã có

Đo từ code:

| Behavior | rush.md | smart.md | deep.md |
|---|:---:|:---:|:---:|
| Read before editing | ✓ | ✗ | ✓ (tools_native) |
| Maximize parallel tool calls | ✓ | ✗ | ✓ (tools_native) |
| Verify before done | ✓ | ✓ (1 dòng) | ✓ |
| Investigation workflow | ✗ | ✗ | ✗ |
| Error discipline / root cause | ✗ | ✗ | ✗ |

`smart.md` là mode mặc định nhưng thiếu cả "read before editing" và
"parallel tool calls" — hai behavior cơ bản nhất mà ngay cả `rush.md`
(mode tối giản nhất) cũng có.

### Vấn đề 3: smart.md và deep.md duplicate Git Safety + Response Style

Đo từ code — 3 section gần như giống nhau:

```
smart.md # Git Safety (4 bullets)  ≈  deep.md # Git Safety (5 bullets)
smart.md # Response Style (5 lines) ≈  deep.md # Response Style (5 lines)
```

Không có shared base. Khi cần thêm rule mới (ví dụ: "NEVER amend unless
asked" đã có trong deep nhưng không có trong smart), phải sửa 2 file.

### Vấn đề 4: rush.md thiếu git safety đầy đủ

`rush.md` chỉ có: `NEVER use destructive git commands (reset --hard,
checkout --)`. Thiếu so với smart/deep:
- "NEVER revert or modify changes you didn't make"
- "NEVER amend a commit unless explicitly requested"
- "Non-interactive git commands only"

Rush agent có thể amend commit hoặc revert work của người khác.


### Vấn đề 5: thiếu hierarchy of truth giữa local code, project memory, và external sources

Trong quá trình audit đã xuất hiện failure mode rõ ràng: agent dùng
GitHub/remote copy để đọc file vốn đã tồn tại trong workspace local.
Đây không phải lỗi tool riêng lẻ; đây là lỗi hệ thống prompt vì không có
một policy rõ ràng cho việc chọn nguồn sự thật.

Đo từ code và hành vi hiện tại:

- `smart.md` chỉ nói: "Search code and docs before asking the user." Nó
  không nói phải ưu tiên local workspace trước.
- `tools_native.md` chỉ nói dùng `WebSearch`, `WebFetch`, và `Gh*` cho
  external lookups và repo inspection; không có boundary "không dùng
  GitHub cho file đã có local".
- `src/config/instructions.rs` inject toàn bộ project instructions vào
  system prompt trên mọi agent loop.
- `src/config/skills.rs` inject skill catalog vào system prompt nhưng
  skill bodies chỉ được progressive disclosure qua `artifact://skill/...`.

LangChain DeepAgents context engineering docs nói memory luôn được inject
vào system prompt và SHOULD được giữ minimal để tránh context overload,
trong khi skills là progressive disclosure. Điều này map trực tiếp với
Luma: `project_instructions` đang đóng vai trò memory luôn-injected, còn
skills thì đã là lazy-load. Hiện tại prompt system chưa nói rõ thứ tự ưu
 tiên: local code → injected project memory → external prior art.

Hệ quả:

- Agent có thể verify local implementation bằng remote mirror cũ hoặc
  sai branch.
- Agent có thể bỏ qua evidence local để đi tìm web/GitHub vì prompt chỉ
  bảo "search" mà không bảo "search where first".
- External prior art bị dùng như source of truth thay vì chỉ là reference.

## Guide-level explanation

### Tầng phân tách sau redesign

```
System prompt (behavior)          Tool description (capability)
─────────────────────────         ──────────────────────────────
Dạy agent: làm gì, khi nào,      Mô tả: tool làm gì, input/output,
theo thứ tự nào, verify thế nào  khi nào dùng, khi nào không dùng
```


### Evidence hierarchy sau redesign

```
1. Local workspace files            ← source of truth cho implementation hiện tại
2. Project instructions / memory    ← source of truth cho repo conventions và policy
3. Skills                           ← opt-in detailed workflows, load khi task match
4. External docs / GitHub / web     ← prior art, upstream docs, current facts
```

Quy tắc đọc hiểu rất đơn giản:
- Nếu file tồn tại trong workspace hiện tại, agent SHOULD đọc local file trước.
- Nếu cần biết project convention, dùng `project_instructions` đã inject.
- Nếu cần workflow chuyên biệt, MAY load skill tương ứng.
- Chỉ khi local evidence không đủ hoặc user yêu cầu compare/reference,
  mới dùng GitHub hoặc web tools.

### File layout mới

```
src/config/prompt/
  base.md          ← MỚI: Git Safety + Response Style (shared)
  rush.md          ← SỬA: thêm git safety đầy đủ từ base
  smart.md         ← REWRITE: Agency + Investigation + Verification
                               + Error Discipline
  deep.md          ← SỬA: bỏ phần trùng với base, giữ Autonomy
                           + Pragmatism + Editing Constraints + Review
  tools_native.md  ← RÚT: chỉ giữ 2 dòng thực sự cần
  tools_patch.md   ← RÚT: tương tự
```

### Tool-style routing by mode

Current code maps tool style directly from provider source (`codex` ->
`Patch`, everything else -> `Native`). Session audit now shows that this
provider-first mapping is too coarse for Luma's workflows:

- `Patch` style relies on `exec_command` + `apply_patch` and therefore
  pushes local read/search/list tasks into shell commands (`rg`, `sed`,
  `cat`, python file reads).
- This is acceptable for long autonomous coding flows, but it distorts
  audit/improve workflows and makes `Smart` sessions more shell-heavy than
  necessary.
- `Native` style provides clearer local evidence tools (`Read`, `Grep`,
  `Glob`) and is a better default for `Rush` and `Smart`.

Therefore the default routing SHOULD be mode-first:

- `Rush` -> `Native`
- `Smart` -> `Native`
- `Deep` -> `Patch` preferred, with `Native` fallback if needed

Provider support still matters, but it becomes a compatibility concern,
not the primary policy for choosing tool style.

### Prompt assembly layers

Sau redesign, system prompt SHOULD được nhìn như composition của các lớp
riêng biệt, thay vì một blob text phẳng:

```text
system_prompt =
  base_prompt          // global agent policy
  + mode_prompt        // rush / smart / deep behavior
  + tool_style_prompt  // native / patch usage notes, chosen by mode-first routing
  + env_context        // cwd, shell, git, CLI availability
  + skill_catalog      // metadata only, not full skill bodies
  + project_memory     // AGENTS.md / CLAUDE.md / RULES.md / ...
```

Trách nhiệm từng lớp MUST rõ ràng:
- `base_prompt`: universal policy như git safety, response style,
  evidence hierarchy.
- `mode_prompt`: behavioral differences giữa rush/smart/deep.
- `tool_style_prompt`: notes riêng cho native vs patch interaction style.
- `env_context`: runtime local environment facts.
- `skill_catalog`: discovery metadata, progressive disclosure only.
- `project_memory`: repo-specific conventions và quality bar.


`prompt::build()` thay đổi:

```rust
// Trước:
format!("{behavior}\n{tools}")

// Sau:
match mode {
    AgentMode::Rush => format!("{rush}"),          // rush tự chứa base
    _ => format!("{base}\n{behavior}\n{tools}"),
}
```

### smart.md mới — cấu trúc

```markdown
You are a powerful coding agent. You help the user with software engineering tasks.

# Agency
- end-to-end: implement, verify, report outcomes
- question/plan mode: when user asks a question or wants a plan, answer first
- no narration of tool usage in responses

# Investigation
- read code before editing; never guess at code you haven't seen
- use offset/limit when reading large files; read only what you need
- parallel tool calls for independent lookups (Grep, Glob, Read, GhFile)
- prefer targeted searches over broad dumps

# Verification
- before reporting done, run the checks the project specifies
- report what you ran and the outcome
- fix the cause of failures; do not suppress errors

# Error Discipline
- address root causes, not symptoms
- read the full error/stack trace, not just the first line
- if the same action fails twice the same way, stop and re-plan

# Handling Ambiguity
- search code and docs before asking the user
- if a decision is needed, present 2-3 options with a recommendation; wait
```

### Session lifecycle and startup workflow

Anthropic long-running harnesses và Cole Medin's Linear harness đều cho
thấy một pattern nhất quán: coding agent hoạt động ổn định hơn khi có
startup workflow rõ ràng trước khi edit.

Vì vậy `smart.md` SHOULD dạy một checklist ngắn kiểu:
- orient to current workspace state before editing;
- inspect relevant files and recent local state (`git`, tests, configs,
  prior artifacts if any);
- verify the current baseline when relevant;
- make one coherent change at a time;
- verify and report outcomes before moving on.

RFC này KHÔNG bắt buộc procedural checklist dài theo từng command,
nhưng MUST encode ít nhất tinh thần "get your bearings first" và
"clean handoff / verified progress".


Git Safety và Response Style đến từ `base.md`.


### Source-selection policy trong prompt và tool layer

`base.md` hoặc `smart.md` MUST có một section ngắn kiểu:

```markdown
# Evidence and Source of Truth
- For files in the current workspace, local files are the source of truth.
- Use local file tools before using GitHub or web tools.
- Use GitHub tools only for remote repositories, other refs, or explicit comparisons.
- Use web sources for documentation, current external facts, and prior art — not to verify local implementation details.
```

Tool descriptions SHOULD reinforce boundary này:
- `Read`: SHOULD hint rằng đây là default reader cho current workspace.
- `GhFile` / `GhLs` / `GhSearch`: SHOULD state "not for files already present locally" hoặc equivalent boundary.
- `WebFetch`: SHOULD state "not for local repo content" hoặc equivalent boundary.

### Tool descriptions sau redesign — ví dụ

Các ví dụ trong section này là design sketches, không phải byte budget cứng.
RFC quan tâm tới trách nhiệm và boundary của text, không ép số dòng
chính xác cho từng tool.


`Read` (hiện 12 dòng → 5 dòng):
```
Read a file or list a directory. Returns content with line numbers.
- path: filesystem path or artifact:// URI (ev/{id} for evidence, skill/{name} for skills).
- Use offset/limit for large files (default 2000 lines, required for >10MB).
- For directories, returns entries with trailing / for subdirectories.
- Not for searching — use Grep for content search, Glob for file search.
```

`Bash` SHOULD được rút gọn đáng kể so với bản hiện tại, nhưng không cần
đóng cứng xuống đúng 5 dòng. Mục tiêu là bỏ workflow duplication và giữ
những boundary quan trọng nhất.

`tools_native.md` (hiện 5 dòng → 2 dòng):
```
- Prefer dedicated file tools (Read/Write/Edit/MultiEdit/Glob/Grep) over Bash.
- Never refer to tool names when speaking to the user.
```

## Reference-level explanation

### base.md

MUST chứa:
- `# Git Safety`: 5 bullets (superset của smart hiện tại, bao gồm "NEVER
  amend", "NEVER revert changes you didn't make", "non-interactive only",
  "dirty worktree handling").
- `# Response Style`: concise, no emoji, inline code, fenced blocks,
  follow project instructions as ground truth, verify before done.

- `# Evidence and Source of Truth`: local workspace first; project
  instructions second; external GitHub/web only for explicit remote
  inspection, documentation, or prior art.

### rush.md

MUST thêm git safety đầy đủ (hiện thiếu 3 bullets).

Rush MAY inline nội dung shared policy để giữ file standalone, hoặc MAY
được compose từ `base.md` nếu implementation thấy sạch hơn. Đây là quyết
định implementation, không phải design constraint cứng.

SHOULD giữ ngắn gọn đáng kể so với smart/deep.

### smart.md

MUST có các sections: `# Agency`, `# Investigation`, `# Verification`,
`# Error Discipline`, `# Handling Ambiguity`.

MUST NOT chứa Git Safety hoặc Response Style (đến từ base.md).

MUST NOT chứa coding philosophy cụ thể (smallest change, naming, error
types) — đó là project-level, thuộc RULES.md/AGENTS.md.

SHOULD có "parallel tool calls" trong `# Investigation`.

SHOULD có "read before editing" trong `# Investigation`.

### deep.md

MUST NOT duplicate Git Safety hoặc Response Style với base.md.

MUST giữ: `# Autonomy`, `# Pragmatism`, `# Editing Constraints`,
`# Review Mindset`.

### tools_native.md và tools_patch.md

SHOULD NOT chứa "call in parallel" nếu rule này đã có trong system prompt.

SHOULD NOT chứa "read before editing" nếu rule này đã có đủ rõ ở layer khác.

MUST chứa: "prefer dedicated tools over Bash" (native) / "prefer
exec_command + apply_patch" (patch).

MUST chứa: "never refer to tool names to the user".

MAY chứa schema strictness reminder ("follow tool call schemas exactly").

### Tool descriptions

MUST mô tả: tool làm gì, input/output shape, khi nào dùng, khi nào không.

SHOULD tránh duplicate global workflow policy đã có trong base/system
prompt (ví dụ: "call in parallel" lặp ở nhiều tool descriptions).

MAY giữ một lượng nhỏ usage guidance nếu nó giúp tránh misuse trực tiếp
của tool (ví dụ boundary của `Edit` hoặc `apply_patch`).

MUST chứa source boundary khi tool có nguy cơ overlap với tool khác:
- local file tools SHOULD say they are for current workspace content;
- GitHub tools SHOULD say they are for remote repositories / refs / comparisons;
- web tools SHOULD say they are for external pages, not local repo content.

MAY chứa boundary guidance ("not for searching — use Grep instead").

### Tool-style routing API

Tool style SHOULD be chosen by a mode-aware helper rather than provider
source alone:

```rust
pub fn for_mode(mode: AgentMode, source: &str) -> Self {
    match mode {
        AgentMode::Rush | AgentMode::Smart => Self::Native,
        AgentMode::Deep => match source {
            "codex" => Self::Patch,
            _ => Self::Native,
        },
    }
}
```

`for_source` MAY remain as a lower-level compatibility helper, but call
sites in the app SHOULD use `for_mode(...)`.

### prompt::build() — thay đổi signature

```rust
const BASE: &str = include_str!("prompt/base.md");

pub fn build(mode: AgentMode, style: ToolStyle) -> String {
    match mode {
        AgentMode::Rush => RUSH.to_owned(),
        AgentMode::Smart => {
            let tools = match style { ToolStyle::Native => TOOLS_NATIVE, ToolStyle::Patch => TOOLS_PATCH };
            format!("{BASE}\n{SMART}\n{tools}")
        }
        AgentMode::Deep => {
            let tools = match style { ToolStyle::Native => TOOLS_NATIVE, ToolStyle::Patch => TOOLS_PATCH };
            format!("{BASE}\n{DEEP}\n{tools}")
        }
    }
}
```

### Test plan

Các test hiện tại trong `src/config/prompt.rs` cần update:

- `smart_structure`: thay assert `# Pragmatism` → assert `# Investigation`,
  `# Verification`, `# Error Discipline`. Giữ assert `# Agency`,
  `# Handling Ambiguity`.
- `deep_native_structure`: thêm assert `!p.contains("# Git Safety")` (vì
  Git Safety giờ đến từ base, không nằm trong deep.md text trực tiếp —
  nhưng vẫn có trong output của `build()`).
- Thêm test `base_included_in_smart_and_deep`: assert output của
  `build(Smart, _)` và `build(Deep, _)` đều chứa "reset --hard" và "emoji".
- `all_variants_have_git_safety`: giữ nguyên — vẫn pass vì base inject vào
  smart/deep, rush tự chứa.

### Rollout

1. Tạo `base.md`.
2. Thêm `# Evidence and Source of Truth` vào `base.md` hoặc `smart.md`.
3. Rewrite `smart.md`.
4. Sửa `deep.md` (bỏ duplicate sections).
5. Sửa `rush.md` (thêm git safety đầy đủ).
6. Trim `tools_native.md` và `tools_patch.md`.
7. Trim tool descriptions (từng tool một, có thể tách PR).
8. Update `prompt::build()` và tests.
9. Change tool-style routing from provider-first to mode-first.

Các bước 1–6 có thể làm trong một PR. Bước 7 tách PR riêng vì nhiều file.

### Rollback

Tất cả thay đổi là text files và một hàm `build()`. Rollback = revert PR.
Không có migration data, không có wire format change.

## Drawbacks

- **Churn test**: các test `smart_structure` và `deep_native_structure`
  phải update. Rủi ro thấp vì test chỉ assert string contains.
- **Behavior regression**: thay đổi system prompt có thể thay đổi agent
  behavior theo cách khó predict. Cần test thủ công với các task thực tế
  sau khi ship.
- **base.md thêm một file mới**: người đọc codebase cần biết thêm một
  indirection. Tuy nhiên đây là tradeoff rõ ràng với việc bỏ duplicate.

## Rationale and alternatives

### Tại sao tách base.md thay vì chọn một file làm canonical?

Nếu chọn `deep.md` làm canonical và `smart.md` import từ đó: deep và smart
có behavior khác nhau (Autonomy vs Agency), không thể dùng chung toàn bộ.
`base.md` chỉ chứa phần thực sự shared (Git Safety + Response Style), giữ
hai file độc lập về behavior.

### Alternative: giữ nguyên, chỉ sửa smart.md

Không giải quyết được vấn đề duplicate giữa smart và deep. Khi thêm rule
mới vào Git Safety vẫn phải sửa 2 chỗ.

### Alternative: merge smart và deep thành một file với conditional sections

Phức tạp hơn, khó đọc, không match với cách `prompt::build()` hoạt động.

### Impact của việc không làm gì

- "Call in parallel" tiếp tục lặp 8+ lần — mỗi API request gửi ~1.7KB
  tool schemas với nội dung trùng lặp.
- smart.md tiếp tục thiếu investigation và verification workflow — agent
  smart mode hoạt động kém hơn rush mode về hai behavior cơ bản này.
- Mỗi lần thêm git safety rule phải sửa 2 file.
- `codex`-backed Smart sessions continue to inherit `Patch` style by
  provider mapping, forcing local inspection/search through shell-heavy
  workflows that are worse for auditability and local-first reasoning.

## Prior art

- **LangChain DeepAgents**: system prompt được compose từ nhiều phần
  (base agent prompt + memory prompt + skills prompt + filesystem prompt).
  Mỗi phần có trách nhiệm rõ ràng. Tool descriptions chỉ mô tả capability.
  (Nguồn: docs.langchain.com/oss/python/deepagents/context-engineering,
  đọc trực tiếp)

- **LangChain DeepAgents**: docs cũng nói memory luôn được inject vào
  system prompt, còn skills là progressive disclosure; memory SHOULD
  minimal để tránh context overload. Điều này hỗ trợ việc tách rõ
  `project_instructions` khỏi skills và thêm hierarchy of truth.

- **Linear Coding Agent Harness** (coleam00): `coding_prompt.md` không
  chứa tool descriptions — tool chỉ được nhắc tên khi cần trong workflow
  steps. Behavior instructions ("NEVER mark Done without verification")
  nằm trong prompt, không trong tool schema.
  (Nguồn: github.com/coleam00/Linear-Coding-Agent-Harness, đọc trực tiếp)

- **Anthropic "Writing tools for agents"**: "Lots of redundant tool calls
  might suggest some rightsizing" — tool description dài/mơ hồ gây
  redundant calls. Tool description nên clear về capability và boundary.
  (Nguồn: anthropic.com/engineering/writing-tools-for-agents, đọc trực tiếp)

- **Manus context engineering**: "Keep your prompt prefix stable" — stable
  prefix tối ưu KV-cache. Tool definitions thay đổi invalidate cache từ
  điểm đó trở đi. Luma đã dùng BTreeMap cho deterministic tool order
  (comment trong registry.rs). RFC này giữ nguyên điều đó.
  (Nguồn: manus.im/blog/Context-Engineering-for-AI-Agents, đọc trực tiếp)

## Unresolved questions

1. **rush.md có nên dùng base.md qua `prompt::build()` không?**
   Đề xuất mặc định: không — rush inline base content để giữ path đơn
   giản và rush intentionally minimal. Nếu base thay đổi, rush cần update
   thủ công (acceptable vì base ít thay đổi).

2. **Tool descriptions: trim từng tool trong cùng PR hay tách?**
   Đề xuất mặc định: tách PR riêng sau PR chính (prompt files). Dễ review
   và rollback độc lập.

3. **`# Handling Ambiguity` có nên vào base.md không?**
   Hiện chỉ có trong smart.md, không có trong deep.md. Deep có "Autonomy"
   thay thế (assume implement, không hỏi). Đề xuất: giữ nguyên — không
   shared, không vào base.


4. **`# Evidence and Source of Truth` nên nằm ở base.md hay smart.md?**
   Đề xuất mặc định: `base.md`, vì đây là global policy cho mọi mode,
   bao gồm rush/deep, không chỉ smart.

5. **Should Patch be Deep-only by default?**
   Đề xuất mặc định: yes — `Rush` and `Smart` use `Native`; `Deep` uses
   `Patch` when the provider supports it well enough.

## Future possibilities

- **Per-mode tool registry**: một số tool (ví dụ GhFile/GhLs/GhSearch)
  có thể không cần thiết cho rush mode. RFC này không thay đổi registry
  nhưng không chặn hướng đó.
- **Tool description versioning**: nếu tool descriptions được tối ưu qua
  eval, có thể muốn version chúng độc lập với prompt files.
- **Skill-aware smart prompt**: khi skills được load, smart prompt có thể
  inject một dòng reminder về cách dùng `artifact://skill/` — hiện tại
  chỉ có trong skill catalog XML.

## Implementation status

Chưa implement.
