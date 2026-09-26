//! Proxy API handler：/api/proxy（任意方法）与 /api/asset（资源代理）。
//! 协议转换只在此层；白名单/SSRF/限流/头清理在 core::proxy + core::guard。
//!
//! 入站请求体流式转发（不缓冲）；出站响应流式透传（SSE/chunked 不缓冲）。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use futures_util::StreamExt;
use serde::Deserialize;

use pegboard_core::audit::ActionKind;
use pegboard_core::guard::Action;
use pegboard_core::proxy::{BodyStream, ProxyError};

use crate::ingress::context::RequestContext;
use crate::ingress::error::{code, ApiError};
use crate::ingress::{trace_audit, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/proxy", any(proxy_handler))
        .route("/api/asset", get(asset_handler))
}

#[derive(Debug, Deserialize)]
struct TargetQuery {
    url: String,
}

fn parse_target(raw: &str) -> Result<url::Url, ApiError> {
    let url = url::Url::parse(raw)
        .map_err(|e| ApiError::invalid_request(format!("url 参数非法: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::invalid_request("url 必须是 http(s) 绝对地址"));
    }
    Ok(url)
}

/// 审计 target 脱敏：只记 scheme://host[:port]，不记路径与查询串。
fn safe_target(url: &url::Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(port) = url.port() {
        s.push_str(&format!(":{port}"));
    }
    s
}

async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Query(query): Query<TargetQuery>,
    method: Method,
    headers: HeaderMap,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let target = parse_target(&query.url)?;
    let audit_target = safe_target(&target);
    trace_audit(&state, &ctx, ActionKind::Net, Some(audit_target), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Net)
            .map_err(ApiError::from)?;
        let body = request_body_stream(request);
        let proxy = Arc::clone(&state.proxy);
        let app = Arc::clone(&ctx.app);
        let outcome = proxy
            .forward(&app, method, target, headers, body)
            .await
            .map_err(ApiError::from)?;
        Ok(proxy_response(outcome))
    })
    .await
}

async fn asset_handler(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Query(query): Query<TargetQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let target = parse_target(&query.url)?;
    let audit_target = safe_target(&target);
    trace_audit(&state, &ctx, ActionKind::Net, Some(audit_target), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Net)
            .map_err(ApiError::from)?;
        let proxy = Arc::clone(&state.proxy);
        let app = Arc::clone(&ctx.app);
        let outcome = proxy
            .forward(&app, Method::GET, target, headers, None)
            .await
            .map_err(ApiError::from)?;
        Ok(proxy_response(outcome))
    })
    .await
}

/// 请求体 → 流（有体时流式转发；空体为 None）。
fn request_body_stream(request: Request<Body>) -> Option<BodyStream> {
    use http_body::Body as _;
    let body = request.into_body();
    if body.is_end_stream() {
        return None;
    }
    let stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(|e| ProxyError::Io(std::io::Error::other(e.to_string()))));
    Some(Box::pin(stream))
}

/// ProxyResponse → axum Response（流式体透传）。
fn proxy_response(outcome: pegboard_core::proxy::ProxyResponse) -> Response {
    let mut builder = Response::builder().status(outcome.status);
    for (name, value) in outcome.headers.iter() {
        builder = builder.header(name, value);
    }
    let stream = outcome
        .body
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));
    match builder.body(Body::from_stream(stream)) {
        Ok(response) => response,
        Err(e) => {
            ApiError::new(code::UPSTREAM_ERROR, 502, format!("响应构造失败: {e}")).into_response()
        }
    }
}

/// core ProxyError → ApiError（API 契约 §6）。
impl From<ProxyError> for ApiError {
    fn from(e: ProxyError) -> Self {
        match e {
            ProxyError::Denied(guard_error) => ApiError::from(guard_error),
            ProxyError::InvalidUrl(msg) => ApiError::invalid_request(msg.clone()),
            ProxyError::Upstream(msg) => ApiError::new(code::UPSTREAM_ERROR, 502, msg.clone()),
            ProxyError::Timeout => ApiError::new(code::TIMEOUT, 504, "上游超时"),
            ProxyError::RedirectLimit => ApiError::new(code::UPSTREAM_ERROR, 502, "重定向次数超限"),
            ProxyError::RedirectDenied(msg) => {
                ApiError::new(code::TARGET_DENIED, 403, format!("重定向目标被拒: {msg}"))
            }
            ProxyError::TooLarge { size, max } => ApiError::new(
                code::LIMIT_EXCEEDED,
                413,
                format!("响应过大: {size} > {max}"),
            ),
            ProxyError::RedirectWithBody => ApiError::new(
                code::UPSTREAM_ERROR,
                502,
                "重定向要求重放请求体（流式体不可重放）",
            ),
            ProxyError::Io(err) => ApiError::new(code::UPSTREAM_ERROR, 502, format!("io: {err}")),
        }
    }
}

/// 供契约测试观察转发状态码的便捷常量。
#[allow(dead_code)]
const PROXY_OK: StatusCode = StatusCode::OK;
