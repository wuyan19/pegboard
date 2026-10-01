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
/// 除 AppRegistry / config 外全部只读；注册表变更走显式接口，
/// config 由 admin 配置接口整快照替换（limits 热生效）。
pub struct AppState {
    pub config: RwLock<Arc<Config>>,
    /// 配置文件路径（resolve_config 结果；None = 全默认启动，无文件可写回）。
    pub config_path: Option<std::path::PathBuf>,
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
    /// 禁用应用集合（能力 API 对禁用应用返回 APP_NOT_FOUND；静态仍可访问）
    pub disabled: RwLock<HashSet<String>>,
    /// 进程控制（重启宿主）；实现属进程层（cli），单测用 NoControl
    pub control: Arc<dyn crate::host::ProcessControl>,
    /// 在线升级状态机（更新源未配置时保持 Idle）
    pub update: Arc<crate::host::update::Updater>,
}

impl AppState {
    /// 当前生效配置快照（admin 在线修改后整体替换，读侧取快照保证一致）。
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

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
        // auth 端点豁免鉴权（登录/设密本身不能要凭证）
        .merge(admin::auth_routes())
        .merge(
            admin::api_routes().route_layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                crate::admin::auth::auth_middleware,
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
