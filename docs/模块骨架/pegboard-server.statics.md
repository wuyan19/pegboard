# pegboard-server / statics 模块

## 接口

```rust
// crates/pegboard-server/src/statics/mod.rs

use std::sync::Arc;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

use crate::ingress::AppState;

/// 挂载静态路由：GET /apps/:app_id/* 与 GET /sdk.js、/sdk.d.ts。
pub fn routes() -> Router<Arc<AppState>>;

/// 静态托管 handler。命中文件直出；未命中回退 entry（SPA）。
async fn serve_app(
    State(state): State<Arc<AppState>>,
    Path((app_id, path)): Path<(String, String)>,
) -> Response;

/// 托管单文件（entry 或普通文件）。
async fn serve_file(root: &std::path::Path, rel: &str) -> Response;

/// SDK 资源，编译期内嵌。
async fn serve_sdk(Path(name): Path<String>) -> Response;

/// 内嵌 SDK 资源表。
fn embedded_sdk(name: &str) -> Option<&'static [u8]>;
```

## 路由

```
GET /apps/:app_id/*        应用产物；命中即直出，未命中回退 entry
GET /sdk.js                SDK 运行时
GET /sdk.d.ts              SDK 类型
GET /favicon.ico           宿主图标（可选）
```

- `:app_id` 只接受合法 ID（`[a-z0-9_-]{1,64}`）；非法直接 404。
- `*` 为剩余路径，允许为空（视为 `/`），映射到 `entry` 所在目录的 `index.html` 或 `entry`。

## 行为规则

### 路径解析
- `app_id` 必须在 `AppRegistry` 中，否则 `APP_NOT_FOUND`。
- 剩余路径做 URL 解码；解码后拼接 `AppMeta.root`。
- 用 `canonicalize` + 前缀检查，防 `..` 越界；越界返回 404。
- 空路径 → 用 `AppMeta.entry`。
- 目录路径（以 `/` 结尾）→ 拼 `index.html`。

### 命中与回退
- 文件存在 → 直出。
- 文件不存在且请求看似资源（含扩展名）→ 404。
- 文件不存在且请求看似导航（无扩展名、`Accept: text/html`）→ 回退到 `entry`，返回 `200`。

回退判定：
```
若 path 含 '.'（扩展名）→ 资源，404
否则 → 导航，回退 entry
```
简化规则，避免解析 `Accept` 的复杂度；内部工具足够。

### 响应头
- `Content-Type` 由 `mime_guess` 或 `tower-http` 推断。
- `ETag`：基于文件 mtime + size，弱 ETag。
- `Last-Modified`：文件 mtime。
- `Cache-Control`：
  - 带 hash 的资源（路径含内容哈希风格）→ `public, max-age=31536000, immutable`
  - 其余 → `no-cache`（每次校验）
- `Content-Length`：文件大小；Range 时按范围。
- `Accept-Ranges: bytes`。

### Range
- 支持单范围 `bytes=start-end`、`bytes=start-`、`bytes=-suffix`。
- 返回 `206` + `Content-Range`。
- 非法范围 → `416` + `Content-Range: bytes */size`。
- 多范围不合并，降级为整文件 `200`（内部工具足够）。

### 方法
- 仅 `GET` 与 `HEAD`。`HEAD` 返回相同头、空体。
- 其余方法 → 405。

### 不经过的层
- 不经 `identity`、不经 `guard`、不写 `audit`。
- 不检查权限：应用产物本身是公开的，访问控制由部署层（反代 / 网络）负责。

## 缓存策略

- 使用 `tower_http::services::ServeDir` 处理文件与 Range，外包一层回退逻辑。
- 或手写 `serve_file`：`tokio::fs::File` + `tokio_util::io::ReaderStream`，配 `Body::from_stream`。
- 选择手写，理由：回退逻辑与自定义头更直接；`ServeDir` 的 fallback 语义与我们的规则不完全对齐。

## SDK 内嵌

- `sdk.js` / `sdk.d.ts` 由 `pegboard-sdk` crate 构建产出。
- 通过 `include_bytes!` 编译进 `pegboard-server`。
- 响应头：`Content-Type: application/javascript` / `application/typescript`、`Cache-Control: no-cache`、`ETag` 基于内容哈希。

```rust
fn embedded_sdk(name: &str) -> Option<&'static [u8]> {
    match name {
        "sdk.js"   => Some(include_bytes!(concat!(env!("OUT_DIR"), "/sdk.js"))),
        "sdk.d.ts" => Some(include_bytes!(concat!(env!("OUT_DIR"), "/sdk.d.ts"))),
        _          => None,
    }
}
```

`OUT_DIR` 由 `build.rs` 调用 `pegboard-sdk` 构建脚本产出。

## 单元 / 集成测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;
    use tempfile::TempDir;

    fn state_with_app(files: &[(&str, &[u8])]) -> (TempDir, Arc<AppState>) {
        // 建临时 apps/<id>/，写入给定文件，构造 AppState
        // entry = "index.html" 或 "dist/index.html"
    }

    // ---------- 基本命中 ----------

    #[tokio::test]
    async fn serves_index() {
        let (_d, s) = state_with_app(&[("index.html", b"<h1>hi</h1>")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(header::CONTENT_TYPE).unwrap()
            .to_str().unwrap().starts_with("text/html"));
    }

    #[tokio::test]
    async fn serves_nested_asset() {
        let (_d, s) = state_with_app(&[("assets/a.js", b"console.log(1)")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/assets/a.js").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(header::CONTENT_TYPE).unwrap()
            .to_str().unwrap().contains("javascript"));
    }

    #[tokio::test]
    async fn empty_path_serves_entry() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    // ---------- SPA 回退 ----------

    #[tokio::test]
    async fn spa_fallback_on_navigation() {
        let (_d, s) = state_with_app(&[("index.html", b"app")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/some/route").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        // body 为 index.html 内容
    }

    #[tokio::test]
    async fn missing_asset_returns_404() {
        let (_d, s) = state_with_app(&[("index.html", b"app")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/missing.js").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    // ---------- 安全 ----------

    #[tokio::test]
    async fn traversal_rejected() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/../../etc/passwd").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert!(r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_app_id_returns_404() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/INVALID/x").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_app_returns_404() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/nope/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    // ---------- Range ----------

    #[tokio::test]
    async fn range_returns_206() {
        let (_d, s) = state_with_app(&[("f.bin", &[0u8; 1024])]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/f.bin")
                .header(header::RANGE, "bytes=0-99")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers().get(header::CONTENT_RANGE).unwrap(), "bytes 0-99/1024");
    }

    #[tokio::test]
    async fn invalid_range_returns_416() {
        let (_d, s) = state_with_app(&[("f.bin", &[0u8; 100])]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/f.bin")
                .header(header::RANGE, "bytes=200-300")
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    // ---------- 方法 ----------

    #[tokio::test]
    async fn post_rejected() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::post("/apps/t/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn head_returns_headers_only() {
        let (_d, s) = state_with_app(&[("index.html", b"hello")]);
        let r = build_router(s).oneshot(
            Request::head("/apps/t/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        // body 为空，Content-Length 存在
    }

    // ---------- 缓存头 ----------

    #[tokio::test]
    async fn etag_and_last_modified_present() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert!(r.headers().get(header::ETAG).is_some());
        assert!(r.headers().get(header::LAST_MODIFIED).is_some());
    }

    #[tokio::test]
    async fn html_not_immutable() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/apps/t/index.html").body(Body::empty()).unwrap()
        ).await.unwrap();
        let cc = r.headers().get(header::CACHE_CONTROL).unwrap().to_str().unwrap();
        assert!(cc.contains("no-cache"));
    }

    // ---------- SDK ----------

    #[tokio::test]
    async fn sdk_js_served() {
        let (_d, s) = state_with_app(&[("index.html", b"x")]);
        let r = build_router(s).oneshot(
            Request::get("/sdk.js").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(header::CONTENT_TYPE).unwrap()
            .to_str().unwrap().contains("javascript"));
    }

    #[tokio::test]
    async fn sdk_dts_served() {
        // 同上，Content-Type 含 typescript
    }
}
```

## 依赖

```toml
[dependencies]
pegboard-core = { path = "../pegboard-core" }
axum = { workspace = true, features = ["macros"] }
tokio = { workspace = true, features = ["fs", "io-util"] }
tokio-util = { workspace = true, features = ["io"] }
tower = { workspace = true }
tower-http = { workspace = true, features = ["set-header", "trace"] }
http = { workspace = true }
mime_guess = { workspace = true }
percent-encoding = { workspace = true }
bytes = { workspace = true }
```

`mime_guess` 做扩展名推断；`percent-encoding` 做 URL 解码。

## 设计要点

- **不经治理层**：静态产物是应用的一部分，公开可访问；访问控制由部署层负责，宿主不做。
- **手写而非 `ServeDir`**：回退规则与自定义头更直接，避免 `ServeDir` 的隐式行为。
- **路径安全**：`canonicalize` + 前缀检查；越界即 404，不暴露文件系统细节。
- **回退规则简单**：含 `.` 视为资源，否则视为导航；避免解析 `Accept`。
- **SDK 内嵌**：编译期打包，运行时无文件依赖；升级宿主即升级 SDK。
- **Range 单范围**：满足 `<video>` / `<audio>` / 大文件下载；多范围降级为整文件。
- **缓存分级**：带 hash 的资源长缓存，其余 `no-cache` + ETag；无需构建工具配合。
- **不写审计**：避免噪声；审计只针对能力调用。
