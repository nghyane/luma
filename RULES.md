# LUMA Rust — Coding Rules

Ưu tiên code đúng, rõ, và dễ bảo trì. Rules này là default mạnh; khi lệch khỏi default, phải có lý do kỹ thuật rõ ràng. Mỗi PR phải pass format, clippy, test, và build.

## I. Kiến trúc

1. **Owner mutates.** Ai sở hữu data, người đó cầm `&mut`. Không bypass facade để sửa field trực tiếp.
2. **Giao tiếp đơn giản và một chiều.** Dùng `&T` cho read-only, return value cho sync flow, `Event` enum qua channel cho async flow.
3. **Tránh shared mutable state.** Không `static mut`. Tránh `Arc<Mutex<_>>` và global mutable state; chỉ dùng khi ownership hoặc message passing không phù hợp và phải giải thích trong review.
4. **Dependency chảy xuống, không vòng.** `app → output → {store, scroll, viewport}`. `app → agent → provider`. `agent → tool`. Không đi ngược chiều.
5. **Giữ ranh giới rõ.** Data model, rendering, IO, và orchestration không trộn lẫn trong cùng một nơi nếu có thể tách hợp lý.

## II. Code

6. **Public API phải tự giải thích.** Mọi `pub fn` có doc comment ngắn mô tả contract hoặc tác dụng chính. Private fn chỉ comment khi logic không hiển nhiên.
7. **Error handling tử tế.** Mọi thứ có thể fail trả về `Result<T, E>`. Không `.unwrap()` ngoài test. `.expect("lý do cụ thể")` chỉ khi invariant chắc chắn. Dùng `thiserror` cho module error, `anyhow` cho app-level boundary.
8. **Thêm context ở boundary quan trọng.** Khi đẩy lỗi lên trên, message phải giúp người đọc biết lỗi xảy ra ở bước nào và làm gì tiếp theo.
9. **Naming rõ nghĩa.** Module `snake_case`, type `PascalCase`, fn `snake_case` và nói rõ hành động. Không viết tắt trừ convention quen thuộc như `tx`, `rx`, `buf`, `cfg`, `ctx`. Bool dùng `is_*`, `has_*`, `should_*`.
10. **Tránh magic number trong domain logic.** Trích thành `const` khi giá trị không tự hiển nhiên. Giá trị local nhỏ, ngắn-lived, và obvious không cần ép thành constant.
11. **Ưu tiên code gọn và cohesive.** Struct nên giữ ít field và một trách nhiệm rõ ràng. Hàm nên ngắn, ít nesting, và có một luồng logic dễ đọc. File nên đủ nhỏ để đọc trọn trong một lần. Nếu vượt ngưỡng quen dùng, chỉ giữ nguyên khi nội dung vẫn cohesive và dễ theo dõi.
12. **Ưu tiên type diễn đạt invariant.** Dùng enum, newtype, và pattern matching để encode trạng thái hợp lệ thay vì dựa vào comment hoặc boolean rời rạc.
13. **Comment giải thích lý do, không lặp lại code.** Nếu comment chỉ mô tả lại câu lệnh đang làm gì, hãy xóa và đặt lại tên cho rõ hơn.

## III. Vận hành

14. **Module có logic phải có test.** Test theo behavior và regression, không theo số lượng hình thức. Public behavior mới, bugfix, và edge case quan trọng nên có ít nhất một test giá trị cao.
15. **Viết test gần logic.** Ưu tiên `#[cfg(test)] mod tests` trong file hoặc module liên quan để reader thấy behavior và implementation cùng chỗ.
16. **Không để TODO mơ hồ.** Nếu chưa implement, dùng `todo!("mô tả cụ thể")`. Nếu cần ghi chú công việc, comment phải có context rõ ràng và không được thành bãi rác.
17. **Không `unsafe`.** Chỉ cân nhắc khi có chứng minh rõ ràng rằng không có cách safe tương đương; mặc định của project là không dùng.
18. **Dependencies phải có lý do.** Allowlist hiện tại: `tokio`, `reqwest`, `serde`, `serde_json`, `smallvec`, `tokio-util`, `thiserror`, `anyhow`, `crossterm`. Thêm dependency mới phải justify bằng lợi ích rõ ràng.
19. **Clippy clean.** `cargo clippy -- -D warnings` phải sạch. Không `#[allow(clippy::...)]` trừ false positive có comment giải thích.
20. **Format mặc định.** Dùng `rustfmt` default. Không custom `rustfmt.toml`.

## IV. Cross-platform

21. **Ưu tiên compile-time selection.** Dùng `#[cfg(unix)]`, `#[cfg(windows)]`, hoặc module-level `#[cfg(...)]` thay vì runtime branch bằng `cfg!(...)` khi khác biệt là theo platform.
22. **Tách module theo platform.** Code platform-specific nằm ở file riêng như `shell/unix.rs`, `shell/windows.rs`; parent module cung cấp API chung.
23. **Test theo platform bằng `#[cfg]`.** Test dùng hành vi hoặc lệnh platform-specific phải được guard ở compile time. Tránh duplicate logic test nếu có thể share expectation.

## V. Process

24. **Làm từ lõi nhỏ nhất ra ngoài.** Ưu tiên types và data model trước, sau đó mới tới trait nếu thật sự cần, rồi simplest impl, test, và cuối cùng mới mở rộng thêm module.
25. **Không generic cho tương lai.** Chỉ generic khi đã có ít nhất hai concrete use cases hoặc abstraction giúp code đơn giản hơn ngay bây giờ.
26. **Owned ở boundary, borrowed ở hot path.** Public API nhận/trả owned type khi hợp lý; internal path ưu tiên borrow để tránh clone không cần thiết.
27. **Đo trước khi optimize.** Chỉ thêm pre-allocation, `SmallVec`, caching, hoặc tối ưu phức tạp khi có evidence hoặc bottleneck rõ ràng.
28. **Commit sạch và có mục đích.** Dùng Conventional Commits (`type(scope): summary`). Một commit nên giải quyết một mục đích rõ ràng; nếu hai thay đổi độc lập thì tách, nếu cùng sửa một bug thì gộp.

## VI. Numeric casts

29. **Widening cast (smaller → larger) phải dùng `From`.** `u64::from(byte)` thay vì `byte as u64`. Lint `clippy::cast_lossless` enforce điều này.
30. **Narrowing cast (larger → smaller) chỉ dùng `as` khi bound rõ ràng và có comment giải thích.** Nếu bound không hiển nhiên hoặc có thể overflow, dùng `try_from` thay vì im lặng truncate.
