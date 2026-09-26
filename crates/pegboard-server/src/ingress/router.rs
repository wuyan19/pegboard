//! 路由装配与全局状态。静态与 SDK 不过 guard、不要求 subject；
//! 能力 API 经 RequestContext extractor（应用识别 + 身份解析）。

use std::sync::{Arc, RwLock};

use axum::Router;
use pegboard_core::app::{AppMeta, AppRegistry};
use pegboard_core::audit::AuditorHandle;
use pegboard_core::config::Config;
use pegboard_core::guard::Guard;
use pegboard_core::identity::Identity;
use pegboard_core::store::StoreManager;

use crate::ingress::error::ApiError;
use crate::ingress::store_api;
use crate::statics;

/// 全进程共享状态。构造一次，注入所有 handler。
/// 除 AppRegistry 外全部只读；注册表变更走显式接口。
pub struct AppState {
    pub config: Config,
    pub apps: RwLock<AppRegistry>,
    pub auditor: AuditorHandle,
    pub identity: Identity,
    pub guard: Arc<Guard>,
    pub stores: StoreManager,
}

impl AppState {
    /// 短持有读锁，返回 Arc 克隆；不跨 await 持锁。
    pub fn app_meta(&self, id: &str) -> Result<Arc<AppMeta>, ApiError> {
        let guard = self
            .apps
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.get(id).ok_or_else(|| ApiError::app_not_found(id))
    }
}

/// 组装全部路由。静态与 API 分两组。
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(statics::routes())
        .merge(store_api::routes())
        .fallback(not_found_fallback)
        .with_state(state)
}

/// 未匹配路由：契约化 404（含 /api/* 下尚未提供的端点）。
/// 非法百分号编码等导致的 extractor 拒绝由 axum 返回 400。
async fn not_found_fallback() -> ApiError {
    ApiError::not_found("路由不存在")
}
