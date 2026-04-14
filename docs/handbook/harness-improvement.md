# Harness Improvement Handbook

Mục tiêu của handbook này là giữ việc cải thiện prompt, tools, memory,
and audit đúng hướng: ít ảo giác hơn, ít phức tạp hóa hơn, và bám sát
vấn đề thật từ session cũ.

## 1. Local truth first

Với code hiện có trong repo, local files là source of truth.

- Đọc code local trước.
- Đọc session local trước.
- Chỉ dùng GitHub hoặc web khi local evidence không đủ, hoặc khi task là
  external/upstream/current-information by nature.

## 2. Keep memory small

Always-injected memory phải nhỏ và ổn định.

- Repo-wide invariants vào `AGENTS.md`.
- Procedural guidance vào project skills.
- Không nhét checklist dài hoặc niche workflow vào always-loaded memory.

## 3. Audit before redesign

Không redesign từ cảm giác hoặc từ prior art alone.

- Audit session cũ trước.
- Xem incidents trước.
- Xem representative cases trước.
- Chỉ redesign khi pattern lặp lại và có evidence đủ rõ.

## 4. Separate inspection from mutation

Inspection và mutation là hai việc khác nhau.

- Inspection nên dùng dedicated local tools (`Read`, `Glob`, `Grep`).
- Mutation nên dùng `Edit` / `MultiEdit` hoặc `apply_patch`.
- Không ép local inspection đi qua shell nếu đã có dedicated tools phù hợp.

## 5. Shell is for runtime work, not default local reading

Shell rất tốt cho:
- verify (`cargo check`, `cargo test`, `cargo clippy`)
- git operations
- runtime/system tasks

Shell không nên là default path cho:
- đọc file local
- search local code
- listing/counting local files

trừ khi current tool surface thực sự không có lựa chọn tốt hơn.

## 6. Heuristics must stay humble

Heuristic audit và proposal chỉ là first-pass support.

- Chúng không phải final diagnosis.
- Chúng phải có confidence thấp hoặc clearly bounded scope.
- Chúng phải dẫn tới review tốt hơn, không phải tự động sửa hệ thống.

## 7. Aggregate metrics must be paired with concrete incidents

Đừng hành động chỉ từ aggregate numbers.

Luôn đi qua:
- aggregate -> incident list -> detail view -> proposal

Nếu aggregate và incidents không khớp, refine audit logic trước khi sửa
prompt/tool system.

## 8. Change one layer per iteration

Mỗi vòng cải thiện chỉ nên nhắm một lớp chính:
- patch prompt/tool wording
- skill tuning
- `AGENTS.md`
- RFC-level architecture

Không làm tất cả cùng lúc. Nếu không, sẽ không biết thay đổi nào có tác
động thực tế.

## 9. Prefer measurable improvements over elegant complexity

Một cơ chế mới chỉ đáng giữ nếu nó giúp ít nhất một trong ba việc:
- nhìn rõ hơn,
- quyết định nhanh hơn,
- hoặc giảm lỗi đo được.

Nếu không có tác dụng thực tế, đừng giữ chỉ vì nó đẹp về kiến trúc.
