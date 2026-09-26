//! 管理页：/admin 内嵌单页应用（无 CDN、无外部依赖）。
//! 只展示宿主元数据（应用、权限、限额、日志），不含业务数据。

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::ingress::error::ApiError;
use crate::ingress::AppState;

/// 管理页路由，挂 /admin。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(index))
        .route("/admin/", get(index))
        .route("/admin/{*rest}", get(spa_or_404))
}

async fn index() -> Response {
    page_response(ADMIN_HTML, "text/html; charset=utf-8")
}

/// /admin 下未命中资源：无扩展名回退 index（SPA），带扩展名 404。
async fn spa_or_404(axum::extract::Path(rest): axum::extract::Path<String>) -> Response {
    let looks_like_asset = rest
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if looks_like_asset {
        return ApiError::not_found("资源不存在").into_response();
    }
    page_response(ADMIN_HTML, "text/html; charset=utf-8")
}

fn page_response(body: &'static [u8], content_type: &str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response())
}

/// 内嵌管理页（单文件，原生 JS + fetch）。
static ADMIN_HTML: &[u8] = include_bytes!("page.html");
