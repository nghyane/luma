# RFC 0007: Instruction Injection & Evidence Hierarchy

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0007                                         |
| Title            | Instruction Injection & Evidence Hierarchy   |
| Status           | Superseded                                   |
| Author(s)        | nghia                                        |
| Created          | 2026-04-14                                   |
| Updated          | 2026-04-14                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | Prompt patches to smart.md and tools_native.md |

## Summary

Redesign phần instruction injection của Luma để tách rõ taxonomy context
của harness: local workspace evidence, project context/memory, session
memory, skills/procedural memory, runtime context, và external prior art.
Mục tiêu là ngăn agent xác minh local implementation bằng GitHub/web
copies, giảm context bloat ở tầng always-injected project memory, và làm
rõ source-of-truth hierarchy trong system prompt và tool descriptions.

## Motivation

### Vấn đề 1: hiện không có source-of-truth hierarchy rõ ràng

Prompt hiện tại không nói rõ phải ưu tiên nguồn nào khi nhiều nguồn cùng
sẵn có.

Đo từ code local:

- `src/config/prompt/smart.md` chỉ nói: "Search code and docs before
  asking the user." Không nói phải ưu tiên local workspace trước.
- `src/config/prompt/tools_native.md` nói dùng `WebSearch`, `WebFetch`,
  và `Gh*` cho external lookups và repo inspection, nhưng không nói rằng
  GitHub tools KHÔNG dùng cho file đã có local.
- `src/tool/gh_file/mod.rs`, `src/tool/gh_ls.rs`, `src/tool/gh_search.rs`
  hiện mô tả capability remote nhưng không có boundary against local.
- `src/tool/web_fetch/mod.rs` không nói "not for local repo content".

Failure mode thực tế đã xảy ra trong audit: agent dùng GitHub/remote copy
để đọc file vốn đã tồn tại trong workspace local. Đây là bug ở instruction
system, không phải bug một tool riêng lẻ.


### Evidence từ session audit thực tế

Audit local session store (`~/.config/luma/sessions/`) cho thấy đây không
phải vấn đề lý thuyết:

- Trên 20 session gần nhất, `<project_instructions>` xuất hiện trong
  `20/20` system prompts.
- Trên 20 session gần nhất, skill body chỉ được load thực sự trong `1/20`
  session (`Read("artifact://skill/..." )`).
- Trên 30 session gần nhất, Bash/exec được dùng `658` lần cho file
  search/read/listing, so với `181` verify invocations và `129` git
  invocations.
- Trên 20 session gần nhất, có ít nhất `2` session trộn local reads với
  remote/external lookups trong cùng một audit flow, cho thấy ambiguity
  trong source-selection là có thật, không chỉ là hypothetical.

Các số liệu này hỗ trợ ba kết luận:
1. project instructions hiện là always-paid context cost;
2. procedural memory qua skills đang bị underused mạnh;
3. thiếu source-of-truth hierarchy dẫn tới mixed-source audits.

### Vấn đề 2: project instructions hiện là always-injected memory không có budget policy

`src/config/instructions.rs`:
- `discover()` đi từ current working directory lên git root để collect
  instructions outermost → innermost.
- `build_instructions()` inject toàn bộ content của mỗi file vào
  `<project_instructions>` trong system prompt.
- Không có khái niệm memory class, budget, hay guidance về việc file nào
  nên luôn được inject và file nào nên thành skill hoặc on-demand doc.

Trong repo hiện tại, `RULES.md` có kích thước lớn hơn 6 KB. Khi được
inject nguyên xi mỗi turn, nó hoạt động như một always-loaded memory
file. Audit session thực tế cho thấy cost này đang bị trả trên mọi sampled
turns có user work, không chỉ ở edge cases. Điều này không sai về tính
năng, nhưng thiếu policy nên dễ dẫn tới context bloat.


### Prompt prefix stability và deterministic assembly

Theo Manus context engineering, prompt prefix càng ổn định thì cache hit
rate càng tốt và hành vi across turns càng dễ dự đoán. RFC này vì vậy
coi các nguyên tắc sau là quan trọng:

- prompt assembly SHOULD giữ thứ tự các block ổn định;
- tool registry MUST giữ ordering deterministic;
- redesign instruction injection SHOULD NOT dẫn tới add/remove tools theo
  từng turn chỉ vì source-selection policy;
- volatile runtime facts SHOULD ở phần sau của prompt thay vì phần đầu.

### Vấn đề 3: skills đã là progressive disclosure, nhưng prompt không dạy khi nào nên dùng

`src/config/skills.rs` đã có thiết kế hợp lý:
- system prompt chỉ inject catalog (name + description + location)
- thân skill chỉ được load khi agent gọi `Read("artifact://skill/..." )`

Tuy nhiên audit session cho thấy behavior thực tế vẫn lệch khỏi ý định này:
skills hiếm khi được load, ngay cả khi task có tín hiệu procedural khá rõ
(ví dụ commit/simplify flows).

Tuy nhiên current system prompt không có hierarchy rõ ràng giữa:
- project instructions (always loaded)
- skills (on demand)
- external docs (on demand)

LangChain DeepAgents docs nói memory luôn được inject vào system prompt,
không có progressive disclosure, nên phải giữ minimal; skills thì dùng
cho detailed workflows và load khi cần. Luma hiện chưa encode principle
này vào prompt hoặc config policy.

## Guide-level explanation


### Naming note: project context vs memory

Để tránh nhập nhằng với các hệ memory khác, RFC này dùng:
- **project context / project memory** cho repo-wide instructions luôn có giá trị;
- **session memory** cho handoff notes, progress state, persisted artifacts của work hiện tại;
- **runtime context** cho env facts như cwd, shell, CLI availability;
- **skills / procedural memory** cho workflows load on demand.

Ở implementation hiện tại, `project_instructions` vẫn là tên code-level hợp
lý; RFC chỉ làm rõ taxonomy ở tầng design.

### Mô hình context mới

```
Tier 1  Local workspace evidence
        Files, directories, tests, configs, git state in current workspace
        → source of truth for current implementation

Tier 2  Project instructions / memory
        AGENTS.md / CLAUDE.md / RULES.md / COPILOT instructions
        → source of truth for repo conventions, quality bar, process rules

Tier 3  Session memory / persisted artifacts
        progress files, evidence artifacts, handoff notes, durable local state
        → source of truth for what happened in this workspace over time

Tier 4  Skills
        On-demand workflows and specialized instructions
        → load only when task matches the skill

Tier 5  Runtime context
        cwd, shell, git branch, CLI availability, model/runtime flags
        → per-run environment facts, not project policy

Tier 6  External sources
        GitHub repos, vendor docs, blog posts, web search results
        → prior art, upstream docs, current facts
```

### Cách agent nên hoạt động sau redesign

Nếu user hỏi về code hiện tại trong repo:
1. Đọc local files bằng `Read` / `Grep` / `Glob`.
2. Áp dụng project conventions từ `project_instructions` đã inject.
3. Nếu task khớp một workflow chuyên biệt, load skill tương ứng.
4. Chỉ khi thiếu evidence local hoặc user yêu cầu compare/reference,
   mới dùng GitHub/Web tools.

Nếu user hỏi về API/framework bên ngoài:
1. Tìm external docs bằng `WebFetch` / `WebSearch`.
2. Nếu cần compare với local integration code, quay lại đọc local files.

### Quy tắc thiết kế docs và rules sau RFC này

- File được always inject SHOULD ngắn, ổn định, và chứa only rules that
  truly apply to every task in the repo.
- Workflow dài, checklist, hay procedural playbook SHOULD thành skill,
  không nên nằm trong `RULES.md`.
- External prior art MUST NOT được xem là source of truth cho code đang
  chạy trong workspace local khi local evidence sẵn có.

## Reference-level explanation

### Evidence hierarchy policy

System prompt MUST chứa một section `# Evidence and Source of Truth` với
ít nhất các rules sau:

- For files in the current workspace, local files are the source of truth.
- The agent SHOULD use local file tools before GitHub or web tools.
- If local persisted artifacts or progress files exist for the task, the agent SHOULD inspect them before using external sources.
- GitHub tools SHOULD only be used for remote repositories, other refs, or
  explicit comparisons.
- Web tools SHOULD only be used for external documentation, current facts,
  and prior art.
- External sources MUST NOT be used to verify local implementation
  details when the local file is available.

### Instruction classes

Luma SHOULD formalize 5 context/instruction classes:

1. **Memory / project instructions**
   - ALWAYS injected.
   - MUST be repo-scoped conventions, quality bars, and durable process
     rules.
   - SHOULD remain concise.

2. **Session memory / persisted artifacts**
   - NOT always injected as full bodies.
   - SHOULD be read on demand from the local filesystem or artifact store.
   - MUST be treated as more authoritative than external summaries about
     what happened in this workspace.

3. **Skills**
   - Catalog injected; body loaded on demand.
   - MUST be for specialized workflows, multi-step procedures, or domain
     knowledge not needed on every task.

4. **Runtime context**
   - Per-run facts like cwd, shell, git branch, and available CLIs.
   - MUST NOT be treated as project policy or durable memory.

5. **External references**
   - Never pre-injected.
   - Fetched only when needed.

### Discovery policy

`src/config/instructions.rs` discovery order MAY stay unchanged:
- `AGENTS.md`
- `CLAUDE.md`
- `.claude/settings.json#instructions`
- `RULES.md`
- `COPILOT.md` / `.github/copilot-instructions.md`

However, prompt assembly MUST treat discovered files as project memory,
not generic context. Documentation and tests SHOULD call this out
explicitly.

### Content guidance for project instructions

Project instruction files SHOULD:
- encode repository-wide conventions and non-negotiable constraints;
- avoid long procedural walkthroughs better represented as skills;
- avoid duplicated tool-usage guidance already covered by the base system
  prompt;
- avoid embedding external documentation excerpts.

A practical test from current audits: if a block is paid on every sampled
turn but rarely changes the chosen workflow, it is a strong candidate to
move out of always-injected memory.

Project instruction files MUST NOT be silently truncated by the harness.
If a repository wants smaller always-injected memory, it MUST author a
smaller memory file (for example `AGENTS.md`) rather than rely on harness
trimming.

This local-first policy does NOT mean "never browse". It means browse
only after local truth is exhausted, or when the question is explicitly
external, remote, or time-sensitive.

### Tool boundary updates

The following tool descriptions MUST be updated to reflect evidence
hierarchy:

- `Read` SHOULD state it is the default reader for current workspace
  files and local artifacts.
- `GhFile`, `GhLs`, `GhSearch` SHOULD state they are not for files already
  present in the current workspace.
- `WebFetch` SHOULD state it is not for local repo content.
- `WebSearch` SHOULD state it is for current external information, not
  code already present locally.

### Prompt assembly responsibilities

`src/tui/app/agent.rs` and `src/tui/app/commands.rs` currently assemble:

```text
base_prompt + env_context + skill_catalog + instructions_block
```

After implementing RFC 0006 and this RFC:
- `base_prompt` MUST include evidence hierarchy policy.
- `skill_catalog` MUST remain lightweight metadata only.
- `instructions_block` MUST continue to inject source path + content,
  because provenance matters during reasoning.

### Test plan

Add or update tests for:
- prompt output contains `# Evidence and Source of Truth` for smart and
  deep modes;
- `GhFile` / `GhLs` / `GhSearch` descriptions mention remote-only
  boundary;
- `WebFetch` mentions external-only boundary;
- `Read` mentions current workspace preference;
- instruction discovery semantics remain unchanged.

Add audit baselines (non-CI or fixture-based) for:
- sampled sessions containing `<project_instructions>`;
- sampled sessions loading skills via `artifact://skill/...`;
- ratio of Bash file-inspection commands vs dedicated file tools.

### Rollout

1. Accept RFC 0006 first or in parallel.
2. Add evidence hierarchy section to base prompt.
3. Update GitHub/web/local tool descriptions with explicit boundaries.
4. Document guidance for what belongs in `RULES.md` vs `SKILL.md`.
5. Optionally add README/docs section for repository authors.

### Rollback

Rollback is a prompt/docs revert. Discovery behavior does not need to
change for this RFC to be partially adopted.

## Drawbacks

- Adds more explicit policy text to the base prompt.
- Repository authors now need to think about what belongs in memory vs
  skill files.
- Does not itself solve large `RULES.md`; it only defines the contract
  that the harness will not truncate it.

## Rationale and alternatives

### Tại sao không giải quyết bằng truncation?

Silent truncation changes semantics of project instructions and hides the
tradeoff from repo authors. The source of truth for project rules should
stay under repository control.

### Alternative: remove GitHub tools from normal coding modes

Too restrictive. GitHub tools are still useful for reading upstream
references, external repos, or explicit comparisons.

### Alternative: rely on user phrasing alone

Not sufficient. The audit already showed the agent can mis-pick a source
when the system prompt lacks hierarchy.

### Impact của việc không làm gì

- Agent can continue to verify local implementation against stale remote
  copies.
- Project memory can continue to grow without any explicit contract.
- Skills remain useful but under-specified relative to always-injected
  instructions.

## Prior art

- **LangChain DeepAgents context engineering**: memory is always loaded
  into the system prompt, while skills are progressive disclosure; keep
  memory minimal to avoid context overload.
  https://docs.langchain.com/oss/python/deepagents/context-engineering

- **Manus context engineering**: keep prompt prefix stable, use the file
  system as context, and avoid confusing context mutations. This RFC
  applies those ideas to local-first evidence and deterministic assembly.
  https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus

- **Anthropic long-running harnesses**: separate environment scaffolding,
  progress memory, and coding behavior instead of relying on a single
  high-level prompt. This supports distinguishing project memory from
  session memory and external prior art.
  https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents

## Unresolved questions

1. Should Luma add a dedicated `MEMORY.md` class, or continue mapping to
   existing conventions (`AGENTS.md`, `RULES.md`, etc.)?
   Default: keep existing discovery; improve docs and prompt semantics.

2. Should repository authors receive linting or warnings when
   `project_instructions` become very large?
   Default: maybe later; not part of this RFC.

3. Should `env_context` remain always injected, or should some of it move
   behind tools?
   Default: leave unchanged in this RFC.

## Future possibilities

- Memory budget reporting in diagnostics.
- Lint command that suggests moving procedural content from `RULES.md`
  into `SKILL.md`.
- Subagent-based investigation mode to isolate heavy external research.

## Implementation status

Chưa implement.

Session-audit evidence incorporated on 2026-04-14 from local store under
`~/.config/luma/sessions/`.
