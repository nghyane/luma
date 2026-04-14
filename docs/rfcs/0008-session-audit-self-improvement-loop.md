# RFC 0008: Session-Audit-Driven Self-Improvement Loop

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0008                                         |
| Title            | Session-Audit-Driven Self-Improvement Loop   |
| Status           | Draft                                        |
| Author(s)        | nghia                                        |
| Created          | 2026-04-14                                   |
| Updated          | 2026-04-14                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Introduce một self-improvement loop bảo thủ, tối ưu chi phí, và
session-first cho Luma. Mọi cải thiện prompt/tool/memory SHOULD bắt đầu
bằng audit trực tiếp từ session cũ và evidence packets nhỏ, thay vì dựa
chủ yếu vào trực giác hoặc prior art từ blog. External research vẫn hữu
ích, nhưng chỉ là supporting prior art sau khi đã có local evidence.

## Motivation

### Vấn đề 1: cải thiện dựa trên cảm giác dễ dẫn tới hallucinated redesign

Khi prompt/tool system có nhiều lớp, rất dễ phản ứng quá mức với một lỗi
đơn lẻ bằng cách thêm rule mới hoặc redesign kiến trúc mà không có đủ
bằng chứng. Điều này gây:

- prompt accretion;
- duplicated guidance;
- regression khó đoán;
- chi phí review và token tăng.

### Vấn đề 2: local sessions đã là dataset thực tế sẵn có

Luma đã lưu session đầy đủ tại `~/.config/luma/sessions/` với:
- system prompt thực tế;
- user/assistant/tool trace;
- evidence blobs;
- tool usage history.

Audit trực tiếp trên local sessions đã cho thấy signal mạnh:
- `<project_instructions>` xuất hiện trong `20/20` sampled sessions;
- skill load chỉ xuất hiện trong `1/20` sampled sessions;
- Bash/exec bị dùng `658` lần cho file read/search/listing trên 30 session
  gần nhất, vượt xa mức kỳ vọng nếu dedicated file tools đang được ưu tiên.

Điều này chứng minh rằng session audit có thể trả lời các câu hỏi design
quan trọng rẻ hơn và chính xác hơn việc suy luận thuần từ prior art.

### Vấn đề 3: review agent luôn chạy sẽ tốn kém và dễ overfit

Nếu mỗi incident đều spawn reviewer agent đọc full transcript, chi phí sẽ
cao và reviewer cũng dễ hallucinate root cause từ context quá dài. Cần
một loop có gate rõ ràng.

## Guide-level explanation


### Audit artifacts must preserve failure signal

Inspired by Manus and Anthropic harness patterns, the audit loop SHOULD
avoid over-cleaning traces. A compact evidence packet is useful only if it
still preserves enough of the wrong turn to explain why the agent failed.

This means:
- do not strip away the specific tool sequence that caused the issue;
- do not keep only the final answer if the failure occurred mid-trace;
- preserve one or more representative observations/errors when they are
  the reason the incident was flagged.

### Mô hình loop mới

```text
session traces
  -> heuristic audit
  -> compact evidence packet
  -> cluster similar incidents
  -> reviewer (batch or high-severity trigger only)
  -> patch / proposal / RFC gate
```

### Triết lý vận hành

- Audit local sessions first.
- Use heuristics for cheap counting and detection.
- Escalate to model review only when a pattern repeats or severity is high.
- Patch the nearest layer first.
- Use RFC only for cross-cutting changes.

### Ví dụ

Nếu 8 session gần nhất cho thấy agent dùng `GhFile` trong khi local file
đã có sẵn:
1. heuristic detector gắn tag `wrong_source`;
2. tạo evidence packet với 2-3 transcript excerpts nhỏ;
3. reviewer đề xuất "base prompt needs local-first policy" với confidence;
4. vì thay đổi này là cross-cutting, loop mở RFC candidate.

Nếu chỉ có 1 session dùng wording hơi dài trong `GhFile` description:
- không RFC;
- chỉ tạo patch proposal nhỏ hoặc không hành động.

## Reference-level explanation

### Failure taxonomy

Luma SHOULD bắt đầu với taxonomy tối thiểu sau:

- `wrong_source` — dùng GitHub/web khi local evidence đã đủ;
- `bash_file_overuse` — dùng shell cho file read/search/list thay vì tool chuyên dụng;
- `missing_verification` — code-changing task không có verify signal đủ mạnh;
- `missed_skill` — task có tín hiệu procedural rõ nhưng không load skill;
- `duplicated_guidance` — cùng một instruction xuất hiện ở nhiều layer;
- `context_bloat` — prompt/tool payload lớn nhưng ít tác động tới hành vi.

### Evidence packet schema

Reviewer MUST làm việc trên compact evidence packets, không phải full
transcript mặc định. Mỗi packet SHOULD chứa:

- session id;
- task/title;
- mode/provider/tool style;
- relevant prompt fragments or hashes;
- compact tool sequence summary;
- 1-3 representative excerpts, including the failure signal itself;
- heuristic tags;
- counts/stats (tool usage, verification signals, local vs remote usage);
- confidence that the incident is real, if available.

### Audit pipeline

The default audit path SHOULD be non-LLM first:

1. parse local sessions;
2. compute cheap metrics and heuristic tags;
3. cluster incidents by failure type;
4. create evidence packets for top clusters;
5. run reviewer only on selected packets.

### Reviewer trigger policy

A reviewer MAY be triggered immediately for:
- destructive or safety-sensitive incidents;
- repeated user-visible failures;
- severe source-of-truth violations.

Otherwise reviewer SHOULD run in batch on clustered incidents.

### RFC gate

Reviewer output SHOULD classify each suggestion into one of:
- `patch` — local wording or boundary fix;
- `proposal` — moderate change needing discussion but not RFC;
- `rfc` — cross-cutting architecture or policy change.

RFC is REQUIRED when the proposed change affects:
- prompt assembly layers;
- instruction loading model;
- memory taxonomy;
- source-of-truth hierarchy;
- review/self-improvement workflow itself.

### Cost controls

The loop MUST optimize for low marginal cost:
- heuristic audit first;
- reviewer on packets, not raw transcripts;
- batch review by default;
- no automatic merge of prompt/tool architecture changes.

### Test and validation plan

Before adopting a prompt/tool change suggested by the loop:
- preserve the evidence packet that motivated it;
- add at least one regression scenario or audit check;
- record whether the change reduced the target failure type.

## Drawbacks

- Adds audit infrastructure and taxonomy maintenance.
- Heuristics can be noisy, especially for missed-skill detection.
- Requires discipline to avoid turning every cluster into an RFC.

## Rationale and alternatives

### Tại sao session-first?

Because local sessions are the closest thing to a production transcript
for this harness. They capture the real prompt, real tool boundaries, and
real user corrections.

### Alternative: prior-art-first redesign

Useful for inspiration, but too easy to overfit to other systems and miss
Luma-specific behavior.

### Alternative: reviewer on every incident

Too expensive and too noisy.

## Prior art

- **Anthropic effective harnesses**: emphasize structured progress,
  verification, and learning from observed failure modes.
- **Manus context engineering**: treat context design as empirical work,
  not just theory.
- **LangChain DeepAgents**: separate memory, skills, and runtime context.
- **OpenHarness / Hermes**: treat memory, skills, tools, and safety as
  distinct harness subsystems.

## Unresolved questions

1. Where should audit artifacts live: session store, separate audit dir,
   or generated on demand?
   Default: generate on demand first.

2. Should session audit run as a CLI command, background task, or both?
   Default: CLI/manual first.

3. How much of missed-skill detection can stay heuristic before needing a
   model classifier?
   Default: start heuristic, escalate only if useful.

## Future possibilities

- Per-mode audit dashboards.
- Prompt/tool A/B evaluation tied to evidence packets.
- Automatic draft generation for RFCs from reviewer output.
- Audit-friendly session artifacts and handoff files designed explicitly
  for downstream review.

## Implementation status

Chưa implement.
