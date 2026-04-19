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
ưu nội bộ không đổi behavior, prompt wording patch, tool description
tweak, heuristic tuning nhỏ.

Chỉ mở RFC khi có local evidence hoặc session evidence rõ ràng. Không
mở RFC cho brainstorming chưa có bằng chứng thực tế.

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

| RFC  | Title | Status | Notes |
| ---- | ----- | ------ | ----- |
| 0001 | Evidence-Backed Handoff | Implemented (partial) | M1 shipped. Phase A reverted. Phase B deferred. |
| 0002 | Provider Architecture | Accepted | Core architecture, still active. |
| 0003 | Paste Image UX | Draft | Feature RFC, pending implementation. |
| 0004 | Capability-Aware Read | Draft | Pending implementation. |
| 0005 | Kiro Provider | Implemented | Provider shipped. |
| 0006 | Prompt & Tool System Redesign | Superseded | Core intent implemented directly in `smart.md` and `tools_native.md`. Superseded by 0007. |
| 0007 | Instruction Injection & Evidence Hierarchy | Superseded | Source-of-truth hierarchy and local-first policy implemented directly in `smart.md` and `tools_native.md`. Superseded by prompt patches. |
| 0008 | Session-Audit Self-Improvement Loop | Draft (scoped down) | Audit pipeline implemented. Intent matching and complex routing removed. Audit is detector/triage only. |
| 0009 | Auth, Routing, and Provider Runtime Rearchitecture | Draft | Replaces singleton auth mutation and hidden bootstrap with explicit auth/routing services and typed error handling. |
| 0010 | Device Flow and Interactive Auth Lanes | Draft | Adds shared device-flow architecture for Builder ID and future providers while keeping browser PKCE as a separate lane. |
| 0015 | MCP (Model Context Protocol) Integration | Accepted | Phase 1 stdio shipped. Phase 2 streamable HTTP in progress. |
| 0016 | MCP Remote Auth Stack | Draft | Full remote OAuth/discovery/auth lifecycle for MCP servers. |

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
