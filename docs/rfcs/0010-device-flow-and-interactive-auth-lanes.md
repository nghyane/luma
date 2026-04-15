# RFC 0010: Device Flow and Interactive Auth Lanes

| Field            | Value                                                       |
| ---------------- | ----------------------------------------------------------- |
| RFC              | 0010                                                        |
| Title            | Device Flow and Interactive Auth Lanes                      |
| Status           | Draft                                                       |
| Author(s)        | Nghia / Luma                                                |
| Created          | 2026-04-15                                                  |
| Updated          | 2026-04-15                                                  |
| Tracking issue   | N/A                                                         |
| Supersedes       | Complements 0009                                            |
| Superseded by    | N/A                                                         |

## Summary

Thêm một abstraction chuẩn cho interactive auth flows gồm `BrowserPkce`
và `DeviceCode`, bắt đầu với Kiro Builder ID device flow và giữ Kiro
social login ở browser PKCE lane. Mục tiêu là tránh ép mọi login vào
localhost callback parser, đồng thời tạo một nền chung để sau này hỗ trợ
GitHub device flow hoặc các provider khác có verification URL + user code.

## Motivation

- Evidence thực tế: Kiro Builder ID không trả terminal callback dạng
  `code + state` mà đi qua URL như:
  `http://localhost:3128/signin/callback?login_option=builderid&issuer_url=...&idc_region=...&state=...`
  nhưng không có `code`.
- Probe local `kiro-cli login --license free --use-device-flow --verbose`
  cho thấy upstream hỗ trợ hẳn device flow và in `Confirm the following
  code in the browser` cùng user code.
- Probe local binary `kiro-cli` cho thấy string evidence của device/SSO
  lane: `DeviceCode`, `BuilderIdToken`, `SSO OIDC`, `CreateToken`,
  `authorization pending`, `slow down`, `startUrl`, `issuer_url`.
- Kiến trúc hiện tại của `src/auth/oauth/*` mới chuẩn hoá tốt browser
  PKCE flows, nhưng chưa có vocabulary và polling engine cho device flow.
- Nếu tiếp tục vá Kiro Builder ID vào browser callback parser sẽ làm auth
  layer trộn hai luồng có semantics khác nhau và rất khó mở rộng.

## Guide-level explanation

Sau RFC này, login interactive trong luma không còn bị hiểu mặc định là
"mở browser rồi chờ localhost callback". Thay vào đó, mỗi provider có
thể khai báo lane đăng nhập phù hợp:

- Browser PKCE:
  - mở browser,
  - nhận callback,
  - exchange code,
  - resolve identity,
  - save + read-back verify.

- Device flow:
  - gọi device authorization endpoint,
  - hiển thị verification URL + user code,
  - người dùng approve trong browser,
  - app poll token endpoint,
  - resolve identity,
  - save + read-back verify.

Kiro sẽ là provider đầu tiên dùng cả hai lane:

- Google / GitHub social -> browser flow.
- Builder ID -> device flow.

Ở đây Google/GitHub là upstream identity providers bên trong Kiro auth
surface, không phải các provider độc lập của Luma. Nếu sau này Luma hỗ
trợ một GitHub provider riêng, provider đó MAY tái sử dụng shared device
flow engine, nhưng MUST NOT bị xem là cùng lane với Kiro social GitHub
login.

Từ góc nhìn người dùng:

- `luma login kiro` sẽ cho chọn login method rõ ràng.
- Nếu chọn Builder ID, app sẽ hiển thị code để xác thực thay vì chờ
  localhost callback mơ hồ.
- Sau khi approval thành công, app tự poll và báo signed-in như các flow
  khác.

Thiết kế này cũng mở đường cho GitHub device flow hoặc các provider khác
trong tương lai mà không phải viết polling loop lại từ đầu.

## Reference-level explanation

### 1. File layout

Code SHOULD được thêm theo layout:

```text
src/auth/oauth/
  mod.rs
  shared.rs
  device.rs
  claude.rs
  codex.rs
  kiro.rs
```

`device.rs` MUST chứa shared device-flow primitives, không chứa logic
vendor-specific.

### 2. Shared device-flow types

Thiết kế SHOULD có các type tối thiểu:

```rust
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in_secs: u64,
    pub interval_secs: u64,
}

pub enum DevicePollOutcome {
    Authorized(OAuthTokens),
    Pending,
    SlowDown,
    Denied,
    Expired,
}
```

### 3. Flow vocabulary

Provider login lane MUST được biểu diễn rõ ràng:

```rust
pub enum InteractiveAuthLane {
    BrowserPkce,
    DeviceCode,
}
```

Kiro SHOULD có login method riêng:

```rust
pub enum KiroLoginMethod {
    Google,
    GitHub,
    BuilderId,
}
```

### 4. Device flow shared engine

Shared engine trong `device.rs` MUST:

1. start device authorization;
2. return verification URL + user code;
3. poll token endpoint đến khi success hoặc terminal failure;
4. handle typed polling outcomes:
   - `authorization_pending` -> continue;
   - `slow_down` -> tăng interval;
   - expired/denied -> fail typed error;
5. race polling sleep/request với cancellation token nếu caller cung cấp.

Polling engine MUST NOT save tokens trực tiếp.

### 5. Provider responsibilities

Provider-specific module MUST chỉ làm:

- build request body/headers;
- parse device authorization response;
- parse token response;
- resolve identity từ tokens/profile endpoint.

Provider module MUST NOT contain persistence logic.

### 6. Kiro-specific requirements

Kiro implementation SHOULD:

- route Google/GitHub social login qua browser lane;
- route Builder ID qua device flow lane;
- fail clearly nếu Builder ID flow không trả usable tokens;
- preserve `profileArn` trong metadata nếu token response có field này.

Kiro Builder ID lane MUST NOT depend on localhost callback parser.

### 7. AuthService integration

`AuthService` MUST remain the orchestration layer:

- start lane,
- receive tokens,
- resolve identity,
- save via repository,
- read-back verify,
- only then report success.

CLI/TUI MUST call service use-cases, không trực tiếp gọi provider polling
loop hoặc token endpoints.

### 8. UI/CLI behavior

CLI/TUI for device flow SHOULD show:

- verification URL;
- user code;
- waiting/polling status;
- final success/error.

If `verification_uri_complete` exists, UI MAY prefer opening that URL.

### 9. Test plan

MUST thêm tests cho:

1. parse device authorization response;
2. polling transitions: pending / slow_down / success / expired / denied;
3. Kiro Builder ID lane request/response parsing;
4. `AuthService` integration: device flow -> save -> read-back verify.

## Drawbacks

- Tăng số abstraction trong auth/oauth layer.
- Kiro auth giờ có nhiều lane nên CLI/TUI UX cần rõ ràng hơn.
- Polling logic thêm complexity và cần test kỹ để tránh spam endpoint.

## Rationale and alternatives

### Chọn lane riêng thay vì vá localhost callback parser

Builder ID evidence cho thấy callback shape không phải terminal auth code.
Thiết kế lane riêng giữ semantics rõ ràng hơn và bền hơn về lâu dài.

### Alternative 1: Cố support Builder ID bằng browser callback parser

Không đủ. Callback hiện tại thiếu `code`, và local evidence từ upstream
CLI cho thấy có device flow chính thức.

### Alternative 2: Chỉ support social login, bỏ Builder ID

Không tốt về UX vì free-license users có thể dùng Builder ID hợp lệ qua
upstream CLI.

### Alternative 3: Special-case riêng Kiro Builder ID không có shared engine

Không tốt vì GitHub hoặc provider khác có thể cần device flow về sau.
Shared engine giảm duplication và ép behavior nhất quán hơn. Tuy nhiên,
reuse shared engine về sau cho provider khác (ví dụ một GitHub provider
riêng) MUST không làm mờ ranh giới rằng Kiro social GitHub login vẫn là
auth lane của provider Kiro, không phải auth lane của provider GitHub.

## Prior art

- OAuth 2.0 Device Authorization Grant (RFC 8628).
- AWS SSO / IAM Identity Center OIDC flows (`CreateToken`, `startUrl`).
- Upstream `kiro-cli` option `--use-device-flow` và local evidence từ
  binary strings cho `DeviceCode`, `BuilderIdToken`, `SSO OIDC`.

## Unresolved questions

1. Kiro GitHub social có nên hỗ trợ cả browser flow lẫn device flow không?
   Mặc định: browser first, device flow only for Builder ID.
2. TUI có nên hiển thị QR/complete URL nếu `verification_uri_complete`
   tồn tại không? Mặc định: chưa, in URL + code là đủ.
3. Poll cancellation có cần surfaced như typed `Cancelled` riêng không?
   Mặc định: có.

## Future possibilities

- Một provider khác trong tương lai MAY dùng chung engine trong
  `device.rs` (ví dụ GitHub provider riêng), nhưng reuse này tách biệt
  với Kiro social Google/GitHub login.
- Generic `LoginProgress` event stream cho TUI auth screens.
- Import local Builder ID session nếu upstream CLI đã login sẵn.

## Implementation status

Chưa implement.
