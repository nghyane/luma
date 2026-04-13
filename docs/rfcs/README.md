# RFCs

Thư mục này chứa các design document theo format chuẩn cho Luma. Mục
đích: ghi lại quyết định kỹ thuật có impact rộng trước khi code, để
review dễ và tham chiếu sau dễ.

## Khi nào cần RFC

RFC bắt buộc khi thay đổi:

- Public API của một crate/module lõi (`core`, `provider`, `config`).
- Wire format (tool schema, event bus, persistence layout).
- Kiến trúc cross-cutting (auth, retry, streaming, evidence store).
- Breaking change cho user-facing config hoặc CLI.

Không cần RFC cho: bugfix, refactor cục bộ, thêm test, đổi copy, tối
ưu nội bộ không đổi behavior.

## Quy trình

1. Copy `0000-template.md` thành `NNNN-short-name.md` (số thứ tự
   tăng dần, không reuse).
2. Điền các mục bắt buộc. Status khởi tạo = `Draft`.
3. Mở PR, discuss trực tiếp trong PR (inline comment) hoặc issue
   tracking.
4. Khi đồng thuận, đổi status = `Accepted`, merge RFC.
5. Implement qua các PR tham chiếu RFC number.
6. Khi ship xong, đổi status = `Implemented` và cập nhật mục
   "Implementation status" trong RFC.
7. Nếu rollback/deprecate, giữ file, đổi status = `Withdrawn` hoặc
   `Superseded by NNNN`, ghi lý do.

Status hợp lệ: `Draft` | `Accepted` | `Implemented` | `Withdrawn` |
`Superseded`.

## Normative language

Các RFC SHOULD dùng RFC 2119 / RFC 8174 keywords (MUST, SHOULD, MAY,
…) ở các mục reference-level khi mô tả yêu cầu hành vi bắt buộc. Các
mục narrative (motivation, rationale) dùng tiếng Việt/Anh tự nhiên.

## Đánh số

- `0000-template.md` — template, không implement.
- `0001-evidence-backed-handoff.md` — migrate từ file cũ khi convenient.
- `0002-provider-architecture.md` — RFC hiện tại.

Số RFC không reuse; nếu rút, giữ slot.

## Template

Xem `0000-template.md`. Cấu trúc dựa trên rust-lang/rfcs, rút gọn cho
project nhỏ:

- Metadata header
- Summary
- Motivation
- Guide-level explanation
- Reference-level explanation
- Drawbacks
- Rationale and alternatives
- Prior art
- Unresolved questions
- Future possibilities
- Implementation status (cập nhật sau ship)
