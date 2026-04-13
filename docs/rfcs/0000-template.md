# RFC NNNN: <title>

| Field            | Value                                        |
| ---------------- | -------------------------------------------- |
| RFC              | NNNN                                         |
| Title            | <title>                                      |
| Status           | Draft                                        |
| Author(s)        | <name>                                       |
| Created          | YYYY-MM-DD                                   |
| Updated          | YYYY-MM-DD                                   |
| Tracking issue   | <link or N/A>                                |
| Supersedes       | N/A                                          |
| Superseded by    | N/A                                          |

## Summary

Một đoạn, explain-like-I-know-the-codebase. Cái gì thay đổi, ở đâu,
tại sao bây giờ.

## Motivation

- Vấn đề cụ thể đang chặn cái gì.
- Evidence (code path, số liệu, case study).
- Vì sao các workaround nhỏ không đủ.

## Guide-level explanation

Giải thích thiết kế mới như thể đã ship. Dùng ví dụ, không API dump.
Reader rời mục này phải hiểu shape tổng quan và có thể đọc code mới.

## Reference-level explanation

Chi tiết kỹ thuật đủ để implement. Các yêu cầu hành vi bắt buộc dùng
RFC 2119 keywords:

- MUST / MUST NOT — bắt buộc tuyệt đối.
- SHOULD / SHOULD NOT — khuyến nghị mạnh, có thể lệch nếu có lý do.
- MAY — tùy chọn.

Bao gồm: data model, trait signature, file layout, migration plan,
test plan, rollout, rollback.

## Drawbacks

Tại sao *không* nên làm. Chi phí, rủi ro, churn, UX regression.

## Rationale and alternatives

- Vì sao thiết kế này, không phải các phương án khác.
- Ít nhất 2 alternative đã cân nhắc, và lý do loại.
- Impact của việc không làm gì cả.

## Prior art

Các project/standard đã giải quyết vấn đề tương tự. Link code, RFC,
blog. Ngắn gọn — chỉ những gì ảnh hưởng quyết định.

## Unresolved questions

Các câu hỏi cần chốt trong quá trình review hoặc implement. Mỗi câu
kèm đề xuất mặc định.

## Future possibilities

Hướng mở rộng tự nhiên sau RFC này. Không phải commit; chỉ để reader
biết design không chặn tương lai.

## Implementation status

Cập nhật sau khi ship. Liệt kê commit/PR, ngày, phạm vi.
