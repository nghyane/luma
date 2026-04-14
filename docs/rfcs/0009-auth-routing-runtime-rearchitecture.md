# RFC 0009: Auth, Routing, and Provider Runtime Rearchitecture

| Field            | Value                                                  |
| ---------------- | ------------------------------------------------------ |
| RFC              | 0009                                                   |
| Title            | Auth, Routing, and Provider Runtime Rearchitecture     |
| Status           | Draft                                                  |
| Author(s)        | Nghia / Luma                                           |
| Created          | 2026-04-14                                             |
| Updated          | 2026-04-14                                             |
| Tracking issue   | N/A                                                    |
| Supersedes       | Partially supersedes 0002 in auth/runtime boundaries   |
| Superseded by    | N/A                                                    |

## Summary

Thay thế kiến trúc auth/provider hiện tại bằng một kiến trúc phân lớp rõ
giữa `AuthRepository`, `AuthService`, `AuthImporter`, `OAuthProvider`,
`RoutingService`, và provider runtime execution. Mục tiêu là loại bỏ
hidden bootstrap, singleton mutation semantics mơ hồ, account merge theo
`label`, refresh/recovery side effects trong request path, và branching
hacky mỗi khi thêm provider mới. Sau RFC này, mọi flow đăng nhập,
resolve credential, chọn account, route request, refresh token, xử lý
401/429, và hiển thị `/accounts` MUST đi qua các service có source of
truth rõ ràng và error model thống nhất.

## Motivation

### Evidence từ code hiện tại

- `src/config/auth/mod.rs` đang gộp persistence, login state, refresh,
  bootstrap import từ local CLIs, account selection, account health,
  account listing, và auto-recover trong một module singleton.
- `static POOL: OnceLock<Mutex<PoolStore>>` làm auth state trở thành
  global mutable cache, trong khi `resolve()` vừa đọc vừa có thể mutate,
  refresh, bootstrap, và mark relogin.
- `with_pool_mut()` persist sau mutation nhưng không trả `Result`, còn
  `save_pool_locked()` nuốt lỗi IO. Điều này cho phép flow login báo
  thành công dù persist có thể thất bại.
- `resolve_inner()` gọi `ensure_bootstrapped(provider)` trong request
  path. Như vậy account import từ `~/.codex/auth.json`, keychain, hoặc
  local source có thể xảy ra ngầm khi đang resolve request, khác với
  semantics của `/accounts` là chỉ liệt kê pool hiện có.
- Account identity hiện merge bằng `account_id` / `email`, rồi fallback
  về `label`; nhưng `label` đồng thời là display field và update key. Đây
  là coupling khiến việc thêm provider hoặc đổi shape identity dễ phát
  sinh duplicate, overwrite sai, hoặc account “biến mất” khỏi `/accounts`
  dù login/reporting path đã thành công.
- Khi thêm provider mới, code phải branch ở nhiều lớp cùng lúc:
  PKCE/login parsing, local import, refresh policy, gateway base URL,
  protocol, quirks, account selection, và error mapping. Complexity tăng
  theo số provider và số cách auth, thay vì theo các interface ổn định.

### Vấn đề cụ thể đang chặn

- Không thể reason chắc chắn “login thành công” nghĩa là gì: browser
  callback thành công, token exchange thành công, account identity parse
  thành công, save disk thành công, hay `/accounts` nhìn thấy account đó.
- Không có boundary rõ giữa auth state và runtime routing. 401/429 có
  thể mutate auth state từ sâu trong request path mà không qua một policy
  layer thống nhất.
- Việc thêm provider mới đang kéo theo patch chéo nhiều file và dễ sinh
  hack “provider-specific” ở sai layer.
- Testing khó mở rộng: unit test và integration test không bám được vào
  các contract rõ ràng mà phải đi qua side effects ngầm trong singleton.

### Vì sao patch cục bộ không đủ

Các sửa nhỏ như “fix login Codex”, “reload pool trước `/accounts`”, hay
“tối ưu merge by identity” chỉ giải quyết triệu chứng. Root cause là
source-of-truth của auth chưa được cô lập, mutation semantics không
fallible, và request routing đang phụ thuộc trực tiếp vào auth state có
side effects. Vấn đề là kiến trúc, không phải một bug đơn lẻ.

## Guide-level explanation

Sau RFC này, hệ thống auth và provider của luma hoạt động theo mô hình:

1. `AuthRepository` là nơi duy nhất đọc/ghi `auth.json`.
2. `AuthImporter` đọc account từ Codex CLI / Claude / Kiro local source,
   nhưng không được phép tự save.
3. `OAuthProvider` lo login, exchange code, refresh token, và resolve
   identity cho từng vendor.
4. `AuthService` điều phối login/import/refresh/account health và là nơi
   duy nhất được mutate auth state.
5. `RoutingService` chọn gateway, protocol, account phù hợp cho mỗi
   request. Nó không đọc local CLI store và không tự parse token.
6. Provider runtime chỉ gửi request và normalize lỗi. Nó không được phép
   trực tiếp mutate pool global.

Người dùng nhìn từ bên ngoài sẽ thấy semantics đơn giản hơn:

- `luma login openai`:
  - mở browser,
  - exchange token,
  - resolve identity,
  - lưu account atomically,
  - read-back verify,
  - rồi mới báo success.

- `luma accounts` và `/accounts`:
  - luôn đọc cùng một auth store,
  - không tự import ngầm,
  - không có chuyện “resolve dùng được nhưng accounts không thấy” chỉ vì
    hai code path đang quan sát hai lớp state khác nhau.

- Khi request chạy:
  - routing chọn một account active,
  - runtime gửi request,
  - nếu 429 thì account được cooldown qua `AuthService`,
  - nếu 401 thì `AuthService` quyết định refresh hay mark relogin,
  - nếu protocol/transport error thì request fail nhưng auth state không
    bị mutate sai.

Thêm provider mới cũng đơn giản hơn. Một provider mới chỉ cần khai báo:

- cách login/refresh/identity (`OAuthProvider` hoặc `ApiKeyProvider`),
- optional importer nếu có local credentials,
- gateway,
- protocol,
- error normalization.

Không cần chạm vào core auth singleton hay thêm branch chéo ở mọi nơi.

## Reference-level explanation

### 1. Module layout mới

Code MUST được tách thành các module sau:

```text
src/auth/
  mod.rs
  domain.rs
  error.rs
  repo.rs
  service.rs
  selection.rs
  import/
    mod.rs
    codex.rs
    claude.rs
    kiro.rs
  oauth/
    mod.rs
    openai.rs
    anthropic.rs
    kiro.rs

src/routing/
  mod.rs
  intent.rs
  planner.rs
  policy.rs
  error.rs

src/provider/
  mod.rs
  gateway.rs
  registry.rs
  runtime.rs
  protocol/
  gateways/
  quirks/
```

`src/config/auth/*` SHOULD được thay thế dần bằng `src/auth/*`. Trong
trạng thái cuối, `config` MUST NOT chứa active business logic auth.

### 2. Domain model

#### 2.1. Account identity

`label` MUST chỉ là presentation field. Nội bộ MUST dùng key ổn định:

```rust
pub struct AccountKey {
    pub vendor: AuthVendor,
    pub subject: AccountSubject,
}

pub enum AccountSubject {
    AccountId(String),
    Email(String),
    ExternalUserId(String),
    Anonymous(String),
}
```

Quy tắc chọn key:

1. Nếu provider trả account ID ổn định, MUST dùng `AccountId`.
2. Nếu không có nhưng có email ổn định, MUST dùng `Email(lowercase)`.
3. Nếu provider có external user id khác email/account_id, MAY dùng
   `ExternalUserId`.
4. Nếu không có identity ổn định ngay sau login/import, MUST tạo
   `Anonymous(uuid)`; MUST NOT fallback sang `label`.

#### 2.2. Account record

```rust
pub struct AccountRecord {
    pub key: AccountKey,
    pub vendor: AuthVendor,
    pub display_name: String,
    pub email: Option<String>,
    pub auth: AuthState,
    pub health: AccountHealth,
    pub metadata: AccountMetadata,
}

pub enum AuthState {
    OAuth(OAuthCredential),
    ApiKey(ApiKeyCredential),
}

pub struct OAuthCredential {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}

pub struct ApiKeyCredential {
    pub token: SecretString,
}

pub enum AccountHealth {
    Active,
    CoolingDown { until_unix: u64 },
    NeedsRelogin { reason: ReloginReason },
    Disabled,
}
```

`AccountMetadata` SHOULD chứa quota snapshot, last success, imported
source, profile ARN hoặc vendor-specific non-secret metadata.

### 3. Repository

Repository MUST là lớp duy nhất đọc/ghi store on-disk:

```rust
pub trait AuthRepository {
    fn load(&self) -> Result<AuthStore, AuthStoreError>;
    fn save(&self, store: &AuthStore) -> Result<(), AuthStoreError>;
    fn replace(&self, store: AuthStore) -> Result<AuthStore, AuthStoreError>;
}
```

`AuthStore`:

```rust
pub struct AuthStore {
    pub version: u32,
    pub accounts: Vec<AccountRecord>,
}
```

Yêu cầu:

- Save MUST là atomic write: ghi file tạm rồi rename.
- Save MUST NOT nuốt lỗi IO.
- Mutation API MUST trả `Result`.
- Repository MUST NOT import local CLI accounts.
- Repository MUST NOT refresh token.
- Repository MUST NOT contain account selection policy.

### 4. Importers

Importer contract:

```rust
pub trait AuthImporter {
    fn vendor(&self) -> AuthVendor;
    fn import_accounts(&self) -> Result<Vec<ImportedAccount>, AuthImportError>;
}
```

Importer chỉ làm:

- đọc local keychain / file,
- parse và normalize về `ImportedAccount`,
- không save,
- không merge,
- không mark relogin/cooldown.

`AuthService` là nơi duy nhất merge imported accounts vào store.

### 5. OAuth providers

OAuth provider contract:

```rust
pub trait OAuthProvider {
    fn vendor(&self) -> AuthVendor;
    fn start_login(&self) -> Result<LoginSession, OAuthError>;
    fn exchange_code(&self, callback: OAuthCallback) -> Result<OAuthTokens, OAuthError>;
    fn refresh(&self, refresh_token: &str) -> Result<OAuthTokens, OAuthError>;
    fn resolve_identity(&self, tokens: &OAuthTokens) -> Result<AccountIdentity, OAuthError>;
}
```

`resolve_identity()` MUST là bước chính thức để lấy `AccountKey`, email,
display name. Logic fallback giữa `id_token`, `access_token`, hoặc profile
endpoint MUST nằm ở layer này, không được rải vào login/import/refresh
code path khác nhau.

### 6. AuthService

`AuthService` là orchestration layer duy nhất được mutate auth state:

```rust
pub struct AuthService<R> {
    repo: R,
    importers: ImportRegistry,
    oauth: OAuthRegistry,
    selection: AccountSelectionPolicy,
}
```

Public use-cases SHOULD gồm:

```rust
impl AuthService {
    pub fn list_accounts(&self) -> Result<Vec<AccountView>, AuthError>;
    pub fn sync_imported_accounts(&self, vendor: AuthVendor) -> Result<SyncReport, AuthError>;
    pub async fn login(&self, vendor: AuthVendor) -> Result<LoginResult, AuthError>;
    pub async fn refresh_account(&self, key: &AccountKey) -> Result<AccountRecord, AuthError>;
    pub fn mark_rate_limited(&self, key: &AccountKey, retry_after_secs: u64) -> Result<(), AuthError>;
    pub fn mark_auth_failed(&self, key: &AccountKey, reason: AuthFailure) -> Result<(), AuthError>;
    pub fn disable_account(&self, key: &AccountKey) -> Result<(), AuthError>;
    pub fn remove_account(&self, key: &AccountKey) -> Result<(), AuthError>;
}
```

Yêu cầu hành vi:

- `login()` MUST only report success after save succeeds and read-back
  verification confirms the account exists in the store.
- `list_accounts()` MUST read the same repository state used by request
  routing.
- `sync_imported_accounts()` MUST be explicit. Import from local CLIs
  MUST NOT happen implicitly in request resolution.
- `mark_rate_limited()` MUST only transition account health, not mutate
  tokens.
- `mark_auth_failed()` MUST classify refreshable vs relogin-required
  failures via typed reasons.

### 7. RoutingService

Routing MUST được tách khỏi auth persistence:

```rust
pub struct RoutingService<A> {
    auth: A,
    gateways: GatewayRegistry,
    selection: AccountSelectionPolicy,
}
```

```rust
pub struct RequestIntent {
    pub source: String,
    pub model_id: String,
    pub mode: AgentMode,
}

pub struct RoutePlan {
    pub gateway_id: GatewayId,
    pub protocol_id: ProtocolId,
    pub account_key: AccountKey,
    pub endpoint: String,
    pub retry_policy: RetryPolicy,
}
```

`resolve_route()` MUST:

1. resolve binding `(gateway, protocol, model)`;
2. ask `AuthService` for eligible accounts of the vendor;
3. apply `AccountSelectionPolicy`;
4. return an immutable `RoutePlan`.

Routing MUST NOT:

- import accounts from local CLIs,
- parse/merge auth state,
- write directly to store,
- infer provider-specific relogin policy ad hoc.

### 8. Runtime execution and error normalization

Provider runtime MUST only send requests and normalize outcomes into typed
errors:

```rust
pub enum ProviderError {
    Transport(TransportError),
    Auth(AuthFailure),
    RateLimited(RateLimitInfo),
    Protocol(ProtocolError),
    Remote(RemoteError),
}
```

Auth failure taxonomy SHOULD tối thiểu gồm:

```rust
pub enum AuthFailure {
    Unauthorized,
    RefreshRejected,
    Revoked,
    MissingRefreshToken,
    InvalidGrant,
}
```

Handling requirements:

- 429 MUST map to `ProviderError::RateLimited` with structured retry data.
- 401/invalid token MUST map to `ProviderError::Auth(...)`.
- Protocol parse failure MUST NOT mutate auth state.
- Transport failure MUST NOT mutate auth state.

Mutation back into auth state MUST happen only through `AuthService`.

### 9. Account selection policy

Selection policy MUST be explicit and testable:

```rust
pub trait AccountSelectionPolicy {
    fn select(&self, accounts: &[AccountRecord], gateway: GatewayId, now_unix: u64)
        -> Option<AccountKey>;
}
```

Default policy SHOULD:

1. exclude `Disabled`;
2. exclude `NeedsRelogin`;
3. exclude `CoolingDown` until its deadline;
4. prefer valid OAuth over API key when the gateway expects renewable auth;
5. prefer richer identity and more recently successful accounts when tie-breaking.

### 10. Persistence schema v3

On-disk auth schema MUST migrate to v3 with stable account keys.

Indicative shape:

```json
{
  "version": 3,
  "accounts": [
    {
      "key": { "vendor": "openai", "subject": { "account_id": "acc_123" } },
      "display_name": "me@example",
      "email": "me@example.com",
      "auth": {
        "kind": "oauth",
        "access_token": "...",
        "refresh_token": "...",
        "expires_at": 1770000000,
        "scopes": ["openid", "email"]
      },
      "health": { "kind": "active" },
      "metadata": {
        "last_success_at": 1770000000,
        "usage": {}
      }
    }
  ]
}
```

Migration from v2 MUST:

- preserve existing tokens,
- convert `label` into `display_name`,
- derive `AccountKey` from `account_id` or `email` when possible,
- generate `Anonymous(uuid)` when old entries lack stable identity,
- preserve cooldown / disabled / relogin state.

### 11. CLI/TUI integration

CLI/TUI MUST call use-cases, not internal auth helpers.

- `luma login` -> `AuthService::login()`.
- `luma accounts` -> `AuthService::list_accounts()`.
- `/accounts` -> `AuthService::list_accounts()`.
- account toggle/remove -> `AuthService` mutation methods.

UI MUST NOT directly read singleton pool state.

### 12. Migration plan

Implementation SHOULD land in phases.

#### Phase 1: repository and domain

- add `src/auth/domain.rs`, `src/auth/repo.rs`, `src/auth/error.rs`;
- introduce `AccountKey`, `AccountRecord`, schema v3 migration;
- keep existing behavior through compatibility adapters.

#### Phase 2: service extraction

- move login/import/refresh/mark operations into `AuthService`;
- convert all save paths to fallible, atomic writes;
- make CLI/TUI call service methods.

#### Phase 3: routing extraction

- add `RoutingService` and `RoutePlan`;
- move candidate selection out of auth store singleton;
- normalize provider runtime errors into `ProviderError`.

#### Phase 4: remove legacy hacks

- remove implicit bootstrap in resolve path;
- remove auto-recover by hidden local re-read during request execution;
- delete label-based merge code;
- delete singleton pool API from old module.

### 13. Test plan

The new architecture MUST add tests at four levels.

1. Domain tests:
   - account-key derivation,
   - lifecycle transitions,
   - selection policy.
2. Repository tests:
   - load/save/migrate,
   - atomic write,
   - malformed store handling.
3. OAuth/importer contract tests:
   - token exchange parsing,
   - refresh parsing,
   - identity resolution,
   - import normalization.
4. Integration tests:
   - login -> save -> read-back -> list,
   - request -> 401 -> refresh -> retry,
   - request -> 429 -> cooldown -> next account,
   - refresh failure -> relogin required.

### 14. Rollout and rollback

Rollout SHOULD keep v2 reader support during migration. Writer MAY stay on
v2 for one intermediate PR if needed, but final implementation MUST write
v3 only after migration code is stable.

Rollback path:

- if service extraction lands without routing extraction, request path MAY
  temporarily adapt old provider runtime to new `AuthService`;
- if v3 writer causes regressions, code MAY keep read-v3/write-v2 behind a
  temporary compile-time branch during the rollout PR series.

### 15. PR plan

RFC này SHOULD được implement bằng một chuỗi PR nhỏ. Mỗi PR MUST để lại
main branch ở trạng thái build được, test pass, và có behavior rõ ràng.

#### PR1: Auth domain + error types + compatibility scaffold

Scope:

- thêm `src/auth/domain.rs`, `src/auth/error.rs`, `src/auth/mod.rs`;
- định nghĩa `AccountKey`, `AccountSubject`, `AccountRecord`, `AuthState`,
  `AccountHealth`, `ReloginReason`, `ProviderError`, `AuthFailure`;
- thêm type chuyển đổi tạm từ legacy `config::auth` sang domain mới;
- chưa đổi behavior runtime.

Out of scope:

- chưa thay persistence,
- chưa thay login,
- chưa thay routing.

Done when:

- domain model compile được;
- có unit tests cho account-key derivation và lifecycle basics;
- không có behavior change ở CLI/TUI.

Why first:

- tạo vocabulary chuẩn trước khi tách service/repository.

#### PR2: Auth repository + atomic persistence + v2 reader

Scope:

- thêm `src/auth/repo.rs`;
- implement load/save atomic cho auth store mới;
- thêm adapter đọc schema v2 hiện tại vào domain mới;
- writer vẫn MAY tạm thời ghi v2-compatible shape nếu cần giảm rủi ro;
- loại bỏ save-path nuốt lỗi trong lớp mới.

Out of scope:

- chưa migrate tất cả call-site sang repository mới;
- chưa write v3-only nếu migration chưa ổn định.

Done when:

- có repository tests cho load/save/malformed file/atomic write;
- mọi path save trong lớp mới trả `Result`;
- có read path ổn định từ file auth hiện tại.

Why second:

- tách source of truth khỏi singleton cũ trước khi chạm service logic.

#### PR3: Auth service skeleton + list accounts + mutations

Scope:

- thêm `src/auth/service.rs` và `src/auth/selection.rs`;
- implement `list_accounts`, `disable_account`, `remove_account`,
  `mark_rate_limited`, `mark_auth_failed` trên repository mới;
- CLI `luma accounts` và TUI `/accounts` chuyển sang `AuthService`;
- thêm compatibility adapter cho `config::auth::list_accounts()` nếu cần.

Out of scope:

- chưa chuyển login/refresh;
- chưa chuyển resolve request path.

Done when:

- `/accounts` và `luma accounts` dùng cùng service/repository;
- không còn read trực tiếp singleton cũ trong UI path;
- selection policy có unit tests cơ bản.

Why third:

- làm cho read path user-visible đi qua source of truth mới sớm nhất.

#### PR4: OAuth provider abstraction + OpenAI implementation

Scope:

- thêm `src/auth/oauth/mod.rs` + provider contract;
- migrate OpenAI/Codex PKCE login + refresh + identity resolution vào
  `src/auth/oauth/openai.rs`;
- giữ CLI behavior cũ nhưng backend gọi service mới;
- `login()` MUST save + read-back verify trước khi báo success.

Out of scope:

- chưa migrate Anthropic/Kiro;
- chưa bỏ hoàn toàn legacy PKCE helpers nếu còn adapter.

Done when:

- `luma login openai` dùng service mới end-to-end;
- có contract tests cho exchange/refresh/identity resolution;
- login success không thể xảy ra nếu save fail.

Why fourth:

- OpenAI/Codex là nhánh auth có nhiều evidence lỗi nhất, nên migrate đầu
  tiên để validate kiến trúc.

#### PR5: OAuth provider abstraction + Anthropic + Kiro implementations

Scope:

- migrate Anthropic và Kiro vào `src/auth/oauth/anthropic.rs` và
  `src/auth/oauth/kiro.rs`;
- unify login outcome shape qua `AuthService`;
- remove provider-specific login branching khỏi lớp orchestration chung.

Out of scope:

- local importer vẫn có thể còn ở legacy adapter nếu chưa tách xong.

Done when:

- tất cả OAuth login path đi qua `AuthService` + `OAuthProvider`;
- PKCE/token parsing không còn nằm ở nhiều code path khác nhau;
- tests cho từng vendor pass.

Why fifth:

- hoàn tất login/refresh architecture trước khi đụng import và request routing.

#### PR6: Importer extraction + explicit auth sync

Scope:

- thêm `src/auth/import/*`;
- migrate đọc local Codex/Claude/Kiro credentials sang importer riêng;
- thêm `AuthService::sync_imported_accounts()`;
- thêm command explicit như `luma auth sync` hoặc service entry point tương đương;
- remove implicit bootstrap/import khỏi path mới.

Out of scope:

- request routing vẫn có thể còn adapter tạm gọi legacy resolve.

Done when:

- import local accounts không còn nằm trong repository hoặc request resolve;
- import có report imported/merged/skipped;
- không còn hidden bootstrap ở service path mới.

Why sixth:

- sau khi login path ổn định, mới đưa local import về đúng layer để tránh
  trộn concerns khi debug.

#### PR7: Routing service + route plan + account selection integration

Scope:

- thêm `src/routing/*`;
- implement `RequestIntent`, `RoutePlan`, `RoutingService`;
- chuyển candidate selection từ legacy auth module sang policy/service mới;
- provider binding lookup vẫn tái sử dụng registry hiện có.

Out of scope:

- runtime error normalization có thể còn adapter tạm.

Done when:

- request path resolve account qua `RoutingService`;
- auth module không còn trực tiếp quyết định route;
- selection/retry policy có integration tests.

Why seventh:

- tách auth state khỏi request planning sau khi auth service đã ổn định.

#### PR8: Provider runtime error normalization

Scope:

- normalize 401/429/transport/protocol errors thành `ProviderError`;
- runtime không tự mutate auth state nữa;
- mutation back (`cooldown`, `refresh`, `relogin`) đi qua `AuthService`.

Out of scope:

- cleanup legacy module có thể để PR9.

Done when:

- 429 -> cooldown qua service;
- 401 -> refresh/relogin qua service;
- protocol/transport errors không làm bẩn auth state;
- integration tests request->error->state transition pass.

Why eighth:

- đây là bước hoàn tất ranh giới giữa execution và auth policy.

#### PR9: Schema v3 writer + legacy adapter removal

Scope:

- bật writer v3 chính thức;
- migrate remaining call-sites khỏi `config::auth` legacy module;
- xóa singleton pool API cũ, label-based merge, hidden auto-recover;
- cập nhật docs, commands, và tests theo kiến trúc mới.

Done when:

- legacy `config::auth` chỉ còn shim rất mỏng hoặc bị xóa;
- auth store write v3 by default;
- không còn request-path bootstrap/import;
- RFC 0009 có thể chuyển từ `Draft` sang `Accepted` hoặc `Implemented`
  tùy phạm vi ship.

### 16. Review gates per PR

Mỗi PR trong plan trên MUST trả lời rõ 4 câu hỏi trong description:

1. Source of truth của auth state ở PR này là gì?
2. PR này đã loại bỏ hidden side effect nào?
3. Nếu PR fail giữa rollout, rollback path là gì?
4. Test nào chứng minh behavior mới ổn định hơn behavior cũ?

PR SHOULD giữ diff nhỏ theo từng concern. Nếu một PR chạm đồng thời
repository, login, routing, và runtime execution thì SHOULD tách nhỏ hơn.

## Drawbacks

- Đây là refactor lớn, chạm nhiều file và đòi hỏi migration có kiểm soát.
- Trong ngắn hạn, code sẽ có lớp adapter giữa legacy auth module và auth
  service mới, tăng tạm thời complexity.
- Cần viết thêm contract tests và migration tests trước khi behavior trở
  nên đơn giản hơn.

## Rationale and alternatives

### Chọn hướng rearchitecture thay vì vá dần

Kiến trúc hiện tại đã trộn persistence, auth policy, routing, import, và
runtime error handling. Tiếp tục vá trong `config/auth/mod.rs` sẽ làm
singleton này ngày càng khó reason và khó test. Tách layer rõ ràng là
cách duy nhất để complexity ngừng lan ra mỗi khi thêm provider mới.

### Alternative 1: Giữ singleton, chỉ làm save fallible

Không đủ. Nó cải thiện login semantics nhưng không giải quyết hidden
bootstrap, request-path mutation, hay provider-specific branching.

### Alternative 2: Giữ auth hiện tại, chỉ thêm `RoutingService`

Không đủ. Routing mới vẫn phải dựa trên một auth module có source of
truth mơ hồ và merge semantics không ổn định.

### Alternative 3: Bỏ local import hoàn toàn

Có thể làm code sạch hơn, nhưng UX regression lớn và không cần thiết.
Vấn đề không phải local import tồn tại, mà là local import đang xảy ra ở
sai layer và sai thời điểm. Import explicit qua `AuthService` là cân bằng
hợp lý hơn.

### Không làm gì cả

Nếu không làm, mỗi provider mới sẽ tiếp tục thêm branch và side effects
vào auth singleton, khiến bug auth/routing khó tái hiện và khó sửa hơn.

## Prior art

- Rust RFC process: tách guide-level và reference-level để đảm bảo design
  review trước khi code.
- OAuth client implementations hiện đại thường tách `token acquisition`,
  `identity resolution`, và `token storage` thành các layer riêng.
- Nhiều SDK/provider runtimes normalize lỗi thành typed errors trước khi
  retry/auth policy quyết định mutate state. Đây là pattern phù hợp cho
  multi-provider clients.

## Unresolved questions

1. `AccountKey::Anonymous` có nên tự động được nâng cấp sang
   `AccountId/Email` khi identity đầy đủ xuất hiện sau refresh không?
   Mặc định: có, nếu merge là lossless.
2. `AuthService::sync_imported_accounts()` nên chạy explicit qua command
   riêng hay một lần lúc app startup? Mặc định: explicit command + manual
   trigger từ UI, không chạy trong request path.
3. Secrets có cần tách riêng khỏi `auth.json` để lưu qua keychain/secure
   storage về sau không? Mặc định: chưa, nhưng domain model MUST không
   chặn khả năng này.
4. Có nên giữ compatibility adapter cho `config::auth` trong toàn bộ một
   release beta hay migrate call-sites trong cùng series PR? Mặc định:
   giữ adapter ngắn hạn, xóa ngay khi CLI/TUI/provider call-sites chuyển xong.

## Future possibilities

- Secure storage backend cho secrets thay vì plaintext file.
- Weighted account scheduling theo quota/latency/success history.
- Explicit `luma auth sync` command với report chi tiết imported/merged/
  skipped accounts.
- Structured auth diagnostics screen trong TUI.
- Telemetry nội bộ cho auth/routing transitions để debug beta issues.

## Implementation status

Chưa implement.
