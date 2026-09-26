//! 路由装配与全局状态。静态与 SDK 不过 guard、不要求 subject；
//! 能力 API 经 RequestContext extractor（应用识别 + 身份解析）。

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use axum::Router;
use pegboard_core::app::{AppMeta, AppRegistry, HostDb};
use pegboard_core::audit::AuditorHandle;
use pegboard_core::config::Config;
use pegboard_core::guard::Guard;
use pegboard_core::identity::Identity;
use pegboard_core::proxy::Proxy;
use pegboard_core::store::StoreManager;

use crate::admin;
use crate::ingress::error::ApiError;
use crate::ingress::files_api;
use crate::ingress::proxy_api;
use crate::ingress::store_api;
use crate::ingress::ws_api;
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
    pub proxy: Arc<Proxy>,
    pub files: pegboard_core::files::FilesManager,
    pub signer: Arc<pegboard_core::files::Signer>,
    /// host.db 句柄（admin 元数据操作）
    pub host_db: Arc<HostDb>,
    /// 进程启动时刻（admin 状态页的 uptime）
    pub started: std::time::Instant,
    /// 管理鉴权 token（env PEGBOARD_ADMIN_TOKEN；None = 不鉴权，v1 本地模式）
    pub admin_token: Option<String>,
    /// 禁用应用集合（能力 API 对禁用应用返回 APP_NOT_FOUND；静态仍可访问）
    pub disabled: RwLock<HashSet<String>>,
}

impl AppState {
    /// 能力路径应用解析：禁用即 APP_NOT_FOUND（admin 骨架语义）。
    pub fn app_meta(&self, id: &str) -> Result<Arc<AppMeta>, ApiError> {
        self.app_meta_static(id)?;
        let disabled = self
            .disabled
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if disabled.contains(id) {
            return Err(ApiError::app_not_found(id));
        }
        self.app_meta_unlocked(id)
    }

    /// 静态路径应用解析（不检查禁用状态：产物公开）。
    pub fn app_meta_static(&self, id: &str) -> Result<Arc<AppMeta>, ApiError> {
        self.app_meta_unlocked(id)
    }

    fn app_meta_unlocked(&self, id: &str) -> Result<Arc<AppMeta>, ApiError> {
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
        .merge(proxy_api::routes())
        .merge(files_api::routes())
        .merge(ws_api::routes())
        .merge(
            admin::api_routes().route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                crate::admin::api::auth_middleware,
            )),
        )
        .merge(admin::page_routes())
        .fallback(not_found_fallback)
        .with_state(state)
}

/// 未匹配路由：契约化 404（含 /api/* 下尚未提供的端点）。
/// 非法百分号编码等导致的 extractor 拒绝由 axum 返回 400。
async fn not_found_fallback() -> ApiError {
    ApiError::not_found("路由不存在")
}
