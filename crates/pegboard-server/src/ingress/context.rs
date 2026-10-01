//! 请求上下文：应用识别 + 身份解析集中一处，handler 直接取用。
//!
//! 应用识别规则（ingress 骨架）：
//! - API 路径：`X-Pegboard-App: <app_id>`；缺失 → 400；不在注册表 → 404。
//! - 静态路径不经此 extractor（statics 自行从 Path 取）。

use std::sync::Arc;
use std::time::Instant;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;

use pegboard_core::app::AppMeta;
use pegboard_core::identity::Subject;

use crate::ingress::error::ApiError;
use crate::ingress::AppState;

/// 应用身份头：SDK 在 window.host 初始化时注入并附带到所有请求。
pub const APP_HEADER: &str = "x-pegboard-app";

/// 每个能力请求的上下文。由 extractor 构造，handler 直接取用。
pub struct RequestContext {
    pub app: Arc<AppMeta>,
    pub subject: Subject,
    pub started_at: Instant,
}

impl FromRequestParts<Arc<AppState>> for RequestContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let app_id = resolve_app_id(parts)
            .ok_or_else(|| ApiError::invalid_request(format!("缺少应用身份头 `{APP_HEADER}`")))?;
        let app = state.app_meta(&app_id)?;
        Ok(Self {
            app,
            subject: state.identity.resolve(),
            started_at: Instant::now(),
        })
    }
}

/// 从请求头解析 app_id（应用身份不得来自请求体）。
pub fn resolve_app_id(parts: &Parts) -> Option<String> {
    app_id_from_headers(&parts.headers)
}

/// 从 HeaderMap 解析 app_id。
pub fn app_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(APP_HEADER)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 手工构造上下文（handler 内已消费 extractor 输入时使用）。
pub fn manual_context(
    state: &crate::ingress::AppState,
    headers: &HeaderMap,
) -> Result<RequestContext, ApiError> {
    let app_id = app_id_from_headers(headers)
        .ok_or_else(|| ApiError::invalid_request(format!("缺少应用身份头 `{APP_HEADER}`")))?;
    let app = state.app_meta(&app_id)?;
    Ok(RequestContext {
        app,
        subject: state.identity.resolve(),
        started_at: Instant::now(),
    })
}
