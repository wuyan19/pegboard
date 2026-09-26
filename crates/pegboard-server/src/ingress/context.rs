//! 请求上下文：应用识别 + 身份解析集中一处，handler 直接取用。
//!
//! 应用识别规则（ingress 骨架）：
//! - API 路径：`X-Pegboard-App: <app_id>`；缺失 → 400；不在注册表 → 404。
//! - 静态路径不经此 extractor（statics 自行从 Path 取）。

use std::sync::Arc;
use std::time::Instant;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use pegboard_core::app::AppMeta;
use pegboard_core::identity::{IdentityError, Request as IdentityRequest, Subject};

use crate::ingress::error::{code, ApiError};
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
        let identity_request = identity_request(parts);
        let subject = state
            .identity
            .resolve(&identity_request)
            .map_err(identity_error)?;
        Ok(Self {
            app,
            subject,
            started_at: Instant::now(),
        })
    }
}

/// 从请求头解析 app_id（应用身份不得来自请求体）。
pub fn resolve_app_id(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(APP_HEADER)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 从请求构造 identity 输入。peer 为 None（ConnectInfo 在反代身份模式后置接入）。
fn identity_request(parts: &Parts) -> IdentityRequest<'_> {
    IdentityRequest {
        headers: &parts.headers,
        peer: None,
    }
}

/// 身份错误映射：PERMISSION_DENIED + 401（仅 token / forwarded 模式出现；
/// v1 固定模式不产生 401 路径，见 API 契约 §6）。
fn identity_error(e: IdentityError) -> ApiError {
    ApiError::new(code::PERMISSION_DENIED, 401, format!("身份解析失败: {e}"))
}
