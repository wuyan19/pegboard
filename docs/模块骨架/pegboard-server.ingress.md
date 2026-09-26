# pegboard-server / ingress 模块

## 接口

```rust
// crates/pegboard-server/src/ingress/mod.rs

pub mod context;
pub mod error;
pub mod router;

pub use context::RequestContext;
pub use error::ApiError;
pub use router::{build_router, AppState};
```

```rust
// crates/pegboard-server/src/ingress/router.rs

use std::sync::{Arc, RwLock};
use axum::Router;
use pegboard_core::{app::AppRegistry, config::Config, guard::Guard,
                    store::StoreManager, files::FilesManager,
                    proxy::Proxy, identity::Identity, audit::Auditor};

/// 全进程共享状态。构造一次，注入所有 handler。
pub struct AppState {
    pub config: Config,
    pub apps: RwLock<AppRegistry>,
    pub identity: Identity,
    pub guard: Arc<Guard>,
    pub stores: StoreManager,
    pub files: FilesManager,
    pub proxy: Proxy,
    pub auditor: Auditor,
}

/// 组装全部路由。静态与 API 分两组。
pub fn build_router(state: Arc<AppState>) -> Router;
```

```rust
// crates/pegboard-server/src/ingress/context.rs

use std::sync::Arc;
use std::time::Instant;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use pegboard_core::app::AppMeta;
use pegboard_core::identity::Subject;
use crate::ingress::AppState;

/// 每个请求的上下文。由 extractor 构造，handler 直接取用。
pub struct RequestContext {
    pub app: Arc<AppMeta>,
    pub subject: Subject,
    pub started_at: Instant,
}

#[axum::async_trait]
impl FromRequestParts<Arc<AppState>> for RequestContext {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>)
        -> Result<Self, Self::Rejection>;
}

/// 从路径或请求头解析 app_id。
/// - 静态路径：`/apps/:app_id/*`
/// - API 路径：`X-Pegboard-App: <app_id>`
pub fn resolve_app_id(parts: &Parts) -> Option<String>;

/// 从请求构造 identity 输入。
fn identity_request<'a>(parts: &'a Parts) -> pegboard_core::identity::Request<'a>;
```

```rust
// crates/pegboard-server/src/ingress/error.rs

use axum::response::{IntoResponse, Response};
use pegboard_core::{guard::GuardError, store::StoreError, files::FilesError,
                    proxy::ProxyError, identity::IdentityError};

/// 契约错误码的统一表达。handler 返回它，由 IntoResponse 映射为响应。
#[derive(Debug)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
    pub detail: serde_json::Value,
    pub status: u16,
}

impl ApiError {
    pub fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self;
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self;
}

impl IntoResponse for ApiError { /* 序列化为契约错误体 */ }

// 各 core 错误到 ApiError 的映射。集中定义，避免散落。
impl From<GuardError> for ApiError;
impl From<StoreError> for ApiError;
impl From<FilesError> for ApiError;
impl From<ProxyError> for ApiError;
impl From<IdentityError> for ApiError;
```

## 路由表

```
静态：     GET /apps/:app_id/*              → statics
管理页：   GET /admin, /admin/*             → admin（内嵌前端）
管理 API： /api/admin/*                     → admin
SDK：      GET /sdk.js, /sdk.d.ts           → sdk（内嵌）

能力 API：
  /api/store/kv/*                           → store handler
  /api/files/*                              → files handler
  /api/proxy                                → proxy handler
  /api/ws-proxy                             → ws handler
  /api/asset                                → asset handler
```

- 静态与 SDK 不过 guard、不要求 subject。
- 能力 API 全部经过 `RequestContext` extractor：解析 app → 解析 subject → 构造上下文。
- 签名访问 `/api/files/:id?token=...`：跳过 subject 解析，走 token 校验。

## 应用识别

规则：
- 静态路径：从 `Path` 提取 `app_id`；不在注册表 → `APP_NOT_FOUND`。
- API 路径：从 `X-Pegboard-App` 头提取；缺失 → `INVALID_REQUEST`；不在注册表 → `APP_NOT_FOUND`。
- 二者冲突（API 请求头 app 与 Referer 路径 app 不同）不校验；以头为准，因为静态路径不经 API。
- 应用 ID 由 SDK 在 `window.host` 初始化时注入，SDK 在所有请求上带该头。

## 身份与治理编排

每个能力请求的处理顺序：

1. `RequestContext` 构造：
   - 解析 app_id → 取 `AppMeta`（`RwLock` 读锁，短持有）。
   - `identity.resolve()` → `Subject`；失败 → `ApiError`（401）。
2. handler 调用 `guard.check_capability(app, action)`；失败 → 403。
3. 目标类（Net/Ws）：`guard.check_target(app, action, &url)`；失败 → 403。
4. `guard.check_rate(app, action)`；失败 → 429。
5. 调用对应 core 能力。
6. 记录审计事件：app、subject、action、target、outcome、duration。
7. 返回响应；core 错误映射为 `ApiError`。

签名访问顺序：解析 `app_id` → 验 token（`Signer::verify`）→ 直接 `files.open`，跳过 1–4。

## 错误映射

| core 错误 | code | HTTP |
|---|---|---|
| `AppRegistry::get` 未命中 | `APP_NOT_FOUND` | 404 |
| `IdentityError` 全部变体 | `PERMISSION_DENIED` | 401 |
| `GuardError::PermissionDenied` | `PERMISSION_DENIED` | 403 |
| `GuardError::TargetDenied` / `SsrfBlocked` | `TARGET_DENIED` | 403 |
| `GuardError::RateLimited` | `LIMIT_EXCEEDED` | 429 |
| `StoreError::QuotaExceeded` / `ValueTooLarge` | `LIMIT_EXCEEDED` | 429 / 413 |
| `FilesError::TooLarge` | `LIMIT_EXCEEDED` | 413 |
| `FilesError::QuotaExceeded` | `LIMIT_EXCEEDED` | 429 |
| `FilesError::NotFound` | `NOT_FOUND` | 404 |
| `FilesError::TokenInvalid` | `TOKEN_INVALID` | 403 |
| `ProxyError::Timeout` | `TIMEOUT` | 504 |
| `ProxyError::Upstream` | `UPSTREAM_ERROR` | 502 |
| `ProxyError::RedirectDenied` | `TARGET_DENIED` | 403 |
| `ProxyError::TooLarge` | `LIMIT_EXCEEDED` | 413 |
| 其余未归类 | `INVALID_REQUEST` | 400 |

响应体统一：
```json
{ "error": { "code": "TARGET_DENIED", "message": "...", "detail": {} } }
```

## 审计集成

每个能力 handler 通过辅助函数记录，统一入口：

```rust
pub async fn traced<T, F>(
    state: &Arc<AppState>,
    ctx: &RequestContext,
    action: ActionKind,
    target: Option<String>,
    fut: F,
) -> Result<T, ApiError>
where F: std::future::Future<Output = Result<T, ApiError>>;
```

- 进入时记 `started_at`，退出时算 `duration_ms`。
- `ApiError` 的 code 映射为 `Outcome::Denied` 或 `Error`。
- 成功映射 `Outcome::Ok`。
- 静态 / SDK / admin 请求不进审计；仅能力路径。

## 单元 / 集成测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> { /* 临时目录 + 一个测试 app */ }

    // ---------- 应用识别 ----------

    #[tokio::test]
    async fn static_app_not_found() {
        let s = test_state();
        let r = build_router(s).oneshot(
            Request::get("/apps/nope/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_missing_app_header_rejected() {
        let s = test_state();
        let r = build_router(s).oneshot(
            Request::get("/api/store/kv/x").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // body.code == "INVALID_REQUEST"
    }

    #[tokio::test]
    async fn api_unknown_app_header_rejected() {
        // X-Pegboard-App: nope → 404 APP_NOT_FOUND
    }

    // ---------- 身份 ----------

    #[tokio::test]
    async fn tokens_missing_returns_401() {
        // IdentityConfig::Tokens 下无 Authorization → 401 PERMISSION_DENIED
    }

    #[tokio::test]
    async fn tokens_valid_passes() {
        // 有效 Bearer → 请求进入 handler
    }

    // ---------- 治理 ----------

    #[tokio::test]
    async fn store_permission_denied() {
        // app 未声明 store → 403 PERMISSION_DENIED
    }

    #[tokio::test]
    async fn target_denied_returns_403() {
        // proxy 目标不在白名单 → 403 TARGET_DENIED
    }

    #[tokio::test]
    async fn rate_limited_returns_429() {
        // 连续请求超限 → 429 LIMIT_EXCEEDED
    }

    // ---------- 签名访问 ----------

    #[tokio::test]
    async fn signed_file_access_without_subject() {
        // 无 Authorization 但带合法 token → 200
    }

    #[tokio::test]
    async fn signed_file_wrong_token_rejected() {
        // 伪造 token → 403 TOKEN_INVALID
    }

    // ---------- 错误映射 ----------

    #[tokio::test]
    async fn error_body_matches_contract() {
        let s = test_state();
        let r = build_router(s).oneshot(
            Request::get("/apps/nope/x").body(Body::empty()).unwrap()
        ).await.unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"]["code"].is_string());
        assert!(v["error"]["message"].is_string());
    }

    // ---------- 审计 ----------

    #[tokio::test]
    async fn capability_request_audited() {
        // 发一次能力请求，shutdown auditor，query 断言有一条事件
    }

    #[tokio::test]
    async fn static_request_not_audited() {
        // 静态请求不产生审计事件
    }
}
```

## 依赖

```toml
[dependencies]
pegboard-core = { path = "../pegboard-core" }
axum = { workspace = true, features = ["macros", "ws", "multipart"] }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "net"] }
tower = { workspace = true }
tower-http = { workspace = true, features = ["fs", "trace", "limit"] }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
http = { workspace = true }
futures-util = { workspace = true }

[dev-dependencies]
tower = { workspace = true, features = ["util"] }
tempfile = { workspace = true }
```

## 设计要点

- **单一入口**：所有能力请求统一经 `RequestContext` extractor，应用识别 + 身份解析集中一处，handler 不重复。
- **错误集中映射**：core 错误到契约错误码只在 `error.rs` 定义，避免散落判断。
- **审计只走能力路径**：静态、SDK、admin 不写审计，避免噪声淹没真实事件。
- **签名访问旁路**：只跳 subject 解析，不跳 app 识别与 token 校验；仍受限额。
- **不持有 `RwLock` 跨 await**：`AppMeta` 用 `Arc` 拷贝后立即释放读锁。
- **状态不可变**：`AppState` 内除 `AppRegistry` 外全部只读；注册表变更走显式接口。
- **不感知业务**：ingress 只做分发与编排，业务逻辑全在 core。
