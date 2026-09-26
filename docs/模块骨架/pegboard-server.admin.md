# pegboard-server / admin 模块

## 接口

```rust
// crates/pegboard-server/src/admin/mod.rs

pub mod api;
pub mod page;

pub use api::routes as api_routes;
pub use page::routes as page_routes;
```

```rust
// crates/pegboard-server/src/admin/api.rs

use std::sync::Arc;
use axum::extract::{Path, Query, State};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use pegboard_core::audit::{ActionKind, Filter, Outcome};
use crate::ingress::{AppState, ApiError};

/// 管理 API 路由，挂 /api/admin/*。
pub fn routes() -> Router<Arc<AppState>>;

/// 应用摘要，供列表与详情共用。
#[derive(serde::Serialize)]
pub struct AppView {
    pub id: String,
    pub name: String,
    pub entry: String,
    pub enabled: bool,
    pub permissions: PermView,
    pub limits: LimitsView,
    pub installed_at: i64,
}

#[derive(serde::Serialize)]
pub struct PermView {
    pub store: bool,
    pub files: bool,
    pub net: Vec<String>,
    pub ws: Vec<String>,
    pub shim: bool,
}

#[derive(serde::Serialize)]
pub struct LimitsView {
    pub kv_value_bytes: u64,
    pub kv_total_bytes: u64,
    pub file_bytes: u64,
    pub file_total_bytes: u64,
    pub net_rps: u32,
    pub sign_ttl_max: u64,
}

#[derive(serde::Deserialize)]
pub struct LogQuery {
    pub app: Option<String>,
    pub subject: Option<String>,
    pub action: Option<String>,     // "Net" | "Store" | ...
    pub outcome: Option<String>,    // "Ok" | "Denied" | "Error"
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,     // 预留，暂不支持
}

#[derive(serde::Serialize)]
pub struct LogPage {
    pub items: Vec<LogItem>,
    pub next: Option<String>,
}

#[derive(serde::Serialize)]
pub struct LogItem {
    pub ts: i64,
    pub app_id: Option<String>,
    pub subject: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub outcome: String,
    pub error_code: Option<String>,
    pub duration_ms: u64,
}

// ---------- handlers ----------

async fn list_apps(State(s): State<Arc<AppState>>) -> Result<Json<Vec<AppView>>, ApiError>;

async fn get_app(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<AppView>, ApiError>;

async fn enable_app(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<AppView>, ApiError>;

async fn disable_app(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<AppView>, ApiError>;

async fn list_logs(
    State(s): State<Arc<AppState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<LogPage>, ApiError>;

// ---------- helpers ----------

fn to_view(meta: &pegboard_core::app::AppMeta, enabled: bool) -> AppView;

fn parse_action(s: &str) -> Option<ActionKind>;
fn parse_outcome(s: &str) -> Option<Outcome>;
```

```rust
// crates/pegboard-server/src/admin/page.rs

use std::sync::Arc;
use axum::extract::Path;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use crate::ingress::AppState;

/// 管理页路由，挂 /admin/*。返回内嵌前端资源。
pub fn routes() -> Router<Arc<AppState>>;

/// 内嵌管理页资源表。
fn embedded(name: &str) -> Option<(&'static [u8], &'static str)>;
```

## 路由

```
管理页：
  GET /admin                     → index.html（SPA）
  GET /admin/*                   → 静态资源，未命中回退 index.html

管理 API：
  GET  /api/admin/apps                → 列表
  GET  /api/admin/apps/:id            → 详情
  POST /api/admin/apps/:id/enable     → 启用
  POST /api/admin/apps/:id/disable    → 禁用
  GET  /api/admin/logs                → 日志查询
```

## 行为规则

### 应用视图
- `enabled`：来自 `host.db` 的 `apps.enabled`；未落库时默认 `true`。
- `permissions` / `limits`：来自 `AppMeta`，已合并默认值后的生效值。
- `installed_at`：来自 `host.db`。
- 列表按 `id` 升序。

### 启用 / 禁用
- 更新 `host.db` 的 `apps.enabled`；同时更新 `AppRegistry` 中该 app 的可用状态。
- 禁用后：静态仍可访问（产物公开），**能力 API 返回 `APP_NOT_FOUND`**。
- 禁用不删除数据；重新启用即恢复。
- 幂等：重复调用返回当前状态，不报错。

> 关于“禁用后静态是否可访问”：可选两种语义。默认取“静态仍可访问”，因为静态是产物、能力才是宿主资源。若需静态也禁，`ingress` 在解析 app 时检查 `enabled`，返回 404。

### 日志查询
- 全部过滤条件透传给 `audit.query`。
- `action` / `outcome` 字符串解析失败 → `INVALID_REQUEST`。
- `limit` 上限 1000，默认 200。
- 时间倒序。
- `cursor` 暂不支持，返回 `next: null`。

### 管理页
- 单页应用，内嵌；不依赖外部 CDN。
- 访问 `/admin` 直出 `index.html`；`/admin/xxx` 未命中回退。
- 无鉴权，由部署层（反代 / 网络）控制访问。
- 不含业务数据；只展示宿主元数据。

## 是否鉴权

- 需求文档列为待定。**默认不鉴权**：内部工具，访问控制交给部署层。
- 若需要，后续在 `ingress` 增加可选的管理路径鉴权策略，不改 admin 内部。

## 单元 / 集成测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tempfile::TempDir;

    fn state() -> (TempDir, Arc<AppState>) { /* 两个测试 app + 少量日志 */ }

    // ---------- 应用列表 / 详情 ----------

    #[tokio::test]
    async fn list_apps_returns_all() {
        let (_d, s) = state();
        let r = build_router(s).oneshot(
            Request::get("/api/admin/apps").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        // body 含 2 个 app，按 id 升序
    }

    #[tokio::test]
    async fn get_app_returns_view() {
        // 断言 permissions / limits 字段齐全，且 limits 为合并后的生效值
    }

    #[tokio::test]
    async fn get_unknown_app_returns_404() {
        // APP_NOT_FOUND
    }

    // ---------- 启用 / 禁用 ----------

    #[tokio::test]
    async fn disable_then_capability_denied() {
        let (_d, s) = state();
        // disable
        let r = build_router(s.clone()).oneshot(
            Request::post("/api/admin/apps/t/disable").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // 能力 API 返回 APP_NOT_FOUND
        let r = build_router(s).oneshot(
            Request::get("/api/store/kv/x")
                .header("x-pegboard-app", "t")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn enable_restores_capability() {
        // disable → enable → 能力 API 恢复
    }

    #[tokio::test]
    async fn disable_is_idempotent() {
        // 连续两次 disable，均 200
    }

    #[tokio::test]
    async fn disable_static_still_served() {
        // 默认语义：禁用后静态仍 200
    }

    // ---------- 日志 ----------

    #[tokio::test]
    async fn logs_returned_desc() {
        // 造若干日志，断言倒序
    }

    #[tokio::test]
    async fn logs_filter_by_app() {
        // app=a 的过滤，断言数量与 app_id
    }

    #[tokio::test]
    async fn logs_filter_by_action_and_outcome() {
        // 组合过滤
    }

    #[tokio::test]
    async fn logs_invalid_action_returns_400() {
        // action=Bad → INVALID_REQUEST
    }

    #[tokio::test]
    async fn logs_limit_capped_at_1000() {
        // limit=9999 → 实际最多 1000
    }

    // ---------- 管理页 ----------

    #[tokio::test]
    async fn admin_page_served() {
        let (_d, s) = state();
        let r = build_router(s).oneshot(
            Request::get("/admin").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get("content-type").unwrap()
            .to_str().unwrap().starts_with("text/html"));
    }

    #[tokio::test]
    async fn admin_spa_fallback() {
        // /admin/anything → index.html
    }

    #[tokio::test]
    async fn admin_missing_asset_404() {
        // /admin/x.js（不存在）→ 若含扩展名则 404
    }

    // ---------- 边界 ----------

    #[tokio::test]
    async fn admin_does_not_read_app_data() {
        // 写一个 KV，调用 admin API，断言响应中不含该 KV 内容
    }
}
```

## 依赖

```toml
[dependencies]
pegboard-core = { path = "../pegboard-core" }
axum = { workspace = true, features = ["macros"] }
tokio = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
tower = { workspace = true }

[dev-dependencies]
tower = { workspace = true, features = ["util"] }
tempfile = { workspace = true }
```

无新依赖；管理页资源走 `include_bytes!`。

## 设计要点

- **只读为主**：除启用/禁用外，admin 不写任何状态；不做业务数据操作。
- **不感知业务**：响应只含宿主元数据（清单、限额、日志）；不暴露应用 KV / 文件内容。
- **禁用语义清晰**：静态公开、能力关闭；如需静态也关闭，改 ingress 一处即可。
- **日志透传**：`Filter` 由 `audit.query` 定义，admin 只做字符串解析与上限裁剪。
- **无鉴权默认**：内部工具，部署层负责；若需鉴权，加在 ingress，不改 admin。
- **管理页内嵌**：单页应用，无 CDN、无外部依赖；升级宿主即升级管理页。
- **不引入新错误码**：复用契约错误码，admin 不新增。
