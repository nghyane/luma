# RFC 0008: Session-Audit-Driven Self-Improvement Loop

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | 0008                                         |
| Title            | Session-Audit-Driven Self-Improvement Loop   |
| Status           | Draft (scoped down)                          |
| Author(s)        | nghia                                        |
| Created          | 2026-04-14                                   |
| Updated          | 2026-04-14                                   |
| Tracking issue   | N/A                                          |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Introduce một audit loop tối giản, session-first cho Luma. Mọi cải thiện
prompt/tool/workflow SHOULD bắt đầu bằng audit trực tiếp từ session cũ,
thay vì dựa vào trực giác hoặc prior art từ blog.

Audit là **detector và triage**, không phải decision engine. Kết quả audit
dùng để hướng dẫn con người sửa prompt/workflow, không phải để tự động
thay đổi kiến trúc.

Pipeline mặc định:

```text
session traces
  -> heuristic audit (non-LLM)
  -> compact evidence packet
  -> cluster by failure type
  -> manual review or batch reviewer
  -> patch prompt / workflow / tool description
```

## Motivation

### Vấn đề 1: cải thiện dựa trên cảm giác dễ dẫn tới hallucinated redesign

Khi prompt/tool system có nhiều lớp, rất dễ phản ứng quá mức với một lỗi
đơn lẻ bằng cách thêm rule mới hoặc redesign kiến trúc mà không có đủ
bằng chứng. Điều này gây prompt accretion, duplicated guidance, và
regression khó đoán.

### Vấn đề 2: local sessions đã là dataset thực tế sẵn có

Luma lưu session đầy đủ tại `~/.config/luma/sessions/`. Audit trực tiếp
trên local sessions đã cho thấy signal mạnh:

- `<project_instructions>` xuất hiện trong `20/20` sampled sessions;
- skill load chỉ xuất hiện trong `1/20` sampled sessions;
- Bash/exec bị dùng `658` lần cho file read/search/listing trên 30 session
  gần nhất.

### Vấn đề 3: review agent luôn chạy sẽ tốn kém và dễ overfit

Cần một loop có gate rõ ràng. Reviewer chỉ chạy khi pattern lặp lại hoặc
severity cao.

## Guide-level explanation

### Triết lý vận hành

- Audit local sessions first.
- Use heuristics for cheap counting and detection.
- Escalate to model review only when a pattern repeats or severity is high.
- Patch the nearest layer first (tool description → developer instruction → system prompt assembly).
- Use RFC only for cross-cutting changes.
- Audit is a signal, not a verdict.

### Ví dụ

Nếu 8 session gần nhất cho thấy agent dùng `GhFile` trong khi local file
đã có sẵn:
1. heuristic detector gắn tag `wrong_source`;
2. tạo evidence packet với 2-3 transcript excerpts nhỏ;
3. reviewer hoặc con người đề xuất patch tool boundary description;
4. nếu thay đổi cross-cutting, mở RFC candidate.

## Reference-level explanation

### Failure taxonomy

Taxonomy tối thiểu, chỉ mở rộng khi có local evidence lặp lại:

- `wrong_source` — dùng GitHub/web khi local evidence đã đủ;
- `premature_external_research` — gọi web/GitHub quá sớm trong local task;
- `bash_file_overuse` — dùng shell cho file read/search/list thay vì tool chuyên dụng;
- `missing_verification` — code-changing task không có verify signal đủ mạnh;
- `missed_skill` — task có tín hiệu procedural rõ nhưng không load skill;
- `unknown_pattern` — incident có signal thật nhưng chưa fit taxonomy hiện tại.

### Evidence packet schema

Mỗi packet SHOULD chứa:

- session id, title, task preview;
- failure types, severity, reviewer eligibility;
- compact tool sequence summary;
- 1-3 representative excerpts và span refs (message index, block index);
- supporting counts (tool uses, local reads, remote uses, edits, verify signals);
- detector version.

Packet MUST giữ đủ signal để tái tạo wrong turn. Không strip tool sequence
gây lỗi. Không chỉ giữ final answer sạch.

### Audit pipeline

1. parse local sessions;
2. compute cheap heuristic metrics;
3. cluster incidents by `failure_type + task_family + subsystem + detector_version`;
4. create evidence packets for flagged sessions;
5. run reviewer only on selected packets.

### Reviewer trigger policy

Reviewer MAY be triggered immediately for:
- destructive or safety-sensitive incidents;
- repeated user-visible failures;
- severe source-of-truth violations.

Otherwise reviewer SHOULD run in batch on clustered incidents.

### RFC gate

Reviewer output SHOULD classify each suggestion into:
- `patch` — local wording or boundary fix, no RFC needed;
- `proposal` — moderate change needing discussion;
- `rfc` — cross-cutting architecture or policy change.

RFC is REQUIRED when the proposed change affects prompt assembly layers,
instruction loading model, memory taxonomy, or source-of-truth hierarchy.

### Cost controls

- heuristic audit first, non-LLM;
- reviewer on packets, not raw transcripts;
- batch review by default;
- no automatic merge of prompt/tool architecture changes.

### Scope limits

Audit SHOULD NOT:
- auto-patch prompt or tool descriptions;
- make architecture decisions;
- replace manual review for high-severity incidents;
- grow into a complex intent classifier.

## Implementation status

MVP implemented:
- `src/core/audit.rs` — session scan, heuristic detection, evidence packets, clustering;
- `src/core/improve.rs` — heuristic proposal by failure type and task family;
- CLI: `luma audit sessions`, `luma audit incidents`, `luma audit packets`, `luma audit clusters`, `luma audit show`;
- CLI: `luma improve propose --session <id>`.

Audit results used to patch `src/config/prompt/smart.md` and
`src/config/prompt/tools_native.md` with local-first and
verification-after-edit policies.

Not implemented:
- reviewer agent integration;
- persistent audit artifacts;
- scheduled or background audit runs.
