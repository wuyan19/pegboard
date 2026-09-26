//! 管理 API：/api/admin/*（API 契约 §8）。无鉴权（v1 本地模式），访问控制交部署层。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use pegboard_core::audit::{ActionKind, Filter, Outcome};

use crate::ingress::error::{code, ApiError};
use crate::ingress::AppState;

/// 管理 API 路由，挂 /api/admin/*。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/status", get(status))
        .route("/api/admin/apps", get(list_apps))
        .route("/api/admin/apps/{id}", get(get_app))
        .route("/api/admin/apps/{id}/install", post(install_app))
        .route("/api/admin/apps/{id}/uninstall", post(uninstall_app))
        .route("/api/admin/apps/{id}/enable", post(enable_app))
        .route("/api/admin/apps/{id}/disable", post(disable_app))
        .route("/api/admin/logs", get(list_logs))
}

/// 宿主自身状态（只读元数据，不含业务数据）。
#[derive(serde::Serialize)]
struct StatusView {
    version: &'static str,
    mode: &'static str,
    identity: &'static str,
    listen: String,
    data_root: String,
    apps_dir: String,
    uptime_secs: u64,
    audit_dropped: u64,
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusView> {
    let cfg = &state.config;
    let mode = match cfg.server.mode {
        pegboard_core::config::Mode::Local => "local",
        pegboard_core::config::Mode::Lan => "lan",
    };
    let identity = match &cfg.identity {
        pegboard_core::config::IdentityConfig::Fixed { .. } => "fixed",
        pegboard_core::config::IdentityConfig::Forwarded { .. } => "forwarded",
        pegboard_core::config::IdentityConfig::Tokens { .. } => "tokens",
    };
    Json(StatusView {
        version: env!("CARGO_PKG_VERSION"),
        mode,
        identity,
        listen: cfg.server.listen.to_string(),
        data_root: cfg.storage.data_root.display().to_string(),
        apps_dir: cfg.storage.apps_dir.display().to_string(),
        uptime_secs: state.started.elapsed().as_secs(),
        audit_dropped: state.auditor.dropped(),
    })
}

/// 应用摘要（含启用状态与生效限额）。
#[derive(serde::Serialize)]
pub struct AppView {
    pub id: String,
    pub name: String,
    pub entry: String,
    pub enabled: bool,
    pub permissions: serde_json::Value,
    pub limits: serde_json::Value,
    pub installed_at: i64,
}

fn view_of(meta: &pegboard_core::app::AppMeta, enabled: bool, installed_at: i64) -> AppView {
    AppView {
        id: meta.id.clone(),
        name: meta.name.clone(),
        entry: meta.manifest.entry.clone(),
        enabled,
        permissions: json!({
            "store": meta.manifest.permissions.store,
            "files": meta.manifest.permissions.files,
            "net": meta.manifest.permissions.net,
            "ws": meta.manifest.permissions.ws,
            "shim": meta.manifest.permissions.shim,
        }),
        limits: json!({
            "kv_value_bytes": meta.limits.kv_value_bytes,
            "kv_total_bytes": meta.limits.kv_total_bytes,
            "file_bytes": meta.limits.file_bytes,
            "file_total_bytes": meta.limits.file_total_bytes,
            "net_rps": meta.limits.net_rps,
            "sign_ttl_max": meta.limits.sign_ttl_max,
        }),
        installed_at,
    }
}

async fn list_apps(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let states = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    let metas = {
        let registry = state
            .apps
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.list()
    };
    let views: Vec<AppView> = metas
        .iter()
        .map(|meta| {
            let (enabled, installed_at) = states.get(&meta.id).copied().unwrap_or((true, 0));
            view_of(meta, enabled, installed_at)
        })
        .collect();
    Ok((StatusCode::OK, Json(views)).into_response())
}

async fn get_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let meta = state.app_meta_static(&id)?;
    let states = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    let (enabled, installed_at) = states.get(&id).copied().unwrap_or((true, 0));
    let mut view = serde_json::to_value(view_of(&meta, enabled, installed_at))
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("序列化: {e}")))?;
    view["usage"] = app_usage(&state, &meta).await;
    Ok((StatusCode::OK, Json(view)).into_response())
}

/// 实际用量 vs 限额（架构设计「管理层」）：KV 字节/键数、文件字节/个数。
/// 未使用过的应用没有落库文件，直接报 0（避免惰性打开产生副作用）。
async fn app_usage(state: &Arc<AppState>, meta: &pegboard_core::app::AppMeta) -> serde_json::Value {
    let app_dir = state
        .config
        .storage
        .data_root
        .join("apps_data")
        .join(&meta.id);
    let has_kv = app_dir.join("app.db").is_file();
    let has_files = app_dir.join("files").is_dir();
    let limits = meta.limits;
    let app_id = meta.id.clone();
    let state = Arc::clone(state);
    let kv_files = tokio::task::spawn_blocking(move || {
        let kv = if has_kv {
            state.stores.with(&app_id, &limits, |s| s.usage()).ok()
        } else {
            None
        };
        let files = if has_files {
            state.files.with(&app_id, &limits, |f| f.usage()).ok()
        } else {
            None
        };
        (kv, files)
    })
    .await
    .unwrap_or((None, None));
    let (kv, files) = kv_files;
    json!({
        "kv": {
            "bytes": kv.as_ref().map_or(0, |u| u.bytes),
            "keys": kv.as_ref().map_or(0, |u| u.keys),
        },
        "files": {
            "bytes": files.as_ref().map_or(0, |u| u.0),
            "count": files.as_ref().map_or(0, |u| u.1),
        },
    })
}

async fn enable_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    set_enabled(&state, &id, true)
}

async fn disable_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    set_enabled(&state, &id, false)
}

fn set_enabled(state: &Arc<AppState>, id: &str, enabled: bool) -> Result<Response, ApiError> {
    // 应用须存在（注册表为准）
    let meta = state.app_meta_static(id)?;
    state
        .host_db
        .upsert_app(
            &meta.id,
            &meta.name,
            &meta.root.display().to_string(),
            b"{}",
        )
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    state
        .host_db
        .set_enabled(id, enabled)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    {
        let mut disabled = state
            .disabled
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if enabled {
            disabled.remove(id);
        } else {
            disabled.insert(id.to_owned());
        }
    }
    let states = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    let installed_at = states.get(id).map_or(0, |s| s.1);
    Ok((StatusCode::OK, Json(view_of(&meta, enabled, installed_at))).into_response())
}

/// 安装：重新扫描该应用并注册（清单校验通过才成功）。
async fn install_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let meta = {
        let mut registry = state
            .apps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .reload_one(&state.config.storage.apps_dir, &id, &state.config.limits)
            .map_err(ApiError::from)?
    };
    state
        .host_db
        .upsert_app(
            &meta.id,
            &meta.name,
            &meta.root.display().to_string(),
            meta.manifest_json().as_slice(),
        )
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    get_app(State(state), Path(id)).await
}

/// 卸载：注销元数据、删除数据目录与签名 token；产物目录保留（数据模型 §9）。
async fn uninstall_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.app_meta_static(&id)?;
    {
        let mut registry = state
            .apps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.remove(&id);
    }
    let files_result = state.files.drop_app(&id);
    let stores_result = state.stores.drop_app(&id);
    state
        .host_db
        .delete_app_tokens(&id)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    state
        .host_db
        .remove_app(&id)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    state
        .disabled
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|k| k != &id);
    files_result.map_err(ApiError::from)?;
    stores_result.map_err(ApiError::from)?;
    tracing::info!(app_id = %id, "app uninstalled");
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    app: Option<String>,
    subject: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    /// 倒序分页游标 "ts:seq"（上一页 next 原样传回）
    cursor: Option<String>,
    limit: Option<u32>,
}

fn parse_action(s: &str) -> Option<ActionKind> {
    match s {
        "Store" => Some(ActionKind::Store),
        "Files" => Some(ActionKind::Files),
        "Net" => Some(ActionKind::Net),
        "Ws" => Some(ActionKind::Ws),
        "Static" => Some(ActionKind::Static),
        "Admin" => Some(ActionKind::Admin),
        "Identity" => Some(ActionKind::Identity),
        _ => None,
    }
}

fn parse_outcome(s: &str) -> Option<Outcome> {
    match s {
        "Ok" => Some(Outcome::Ok),
        "Denied" => Some(Outcome::Denied),
        "Error" => Some(Outcome::Error),
        _ => None,
    }
}

async fn list_logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogQuery>,
) -> Result<Response, ApiError> {
    let action = match q.action.as_deref() {
        None => None,
        Some(s) => Some(
            parse_action(s)
                .ok_or_else(|| ApiError::invalid_request(format!("未知 action: {s}")))?,
        ),
    };
    let outcome = match q.outcome.as_deref() {
        None => None,
        Some(s) => Some(
            parse_outcome(s)
                .ok_or_else(|| ApiError::invalid_request(format!("未知 outcome: {s}")))?,
        ),
    };
    let cursor = match q.cursor.as_deref() {
        None => None,
        Some(raw) => {
            let (ts, seq) = raw
                .split_once(':')
                .and_then(|(t, s)| Some((t.parse::<i64>().ok()?, s.parse::<u64>().ok()?)))
                .ok_or_else(|| {
                    ApiError::invalid_request(format!("游标非法: {raw}（须为 ts:seq）"))
                })?;
            Some((ts, seq))
        }
    };
    let filter = Filter {
        app_id: q.app.clone(),
        subject: q.subject.clone(),
        action,
        outcome,
        since: q.since,
        until: q.until,
        cursor,
        limit: q.limit.map(|l| l.min(1000)),
    };
    let events = state
        .auditor
        .query(&filter)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("审计查询: {e}")))?;
    let items: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            json!({
                "ts": e.ts,
                "seq": e.seq,
                "app_id": e.app_id,
                "subject": e.subject,
                "action": format!("{:?}", e.action),
                "target": e.target,
                "outcome": format!("{:?}", e.outcome),
                "error_code": e.error_code,
                "duration_ms": e.duration_ms,
            })
        })
        .collect();
    // 满页才可能有下一页；游标取最后一项的 (ts, seq)
    let effective_limit = q.limit.map(|l| l.min(1000)).unwrap_or(200) as usize;
    let next = if events.len() == effective_limit && effective_limit > 0 {
        events.last().map(|e| format!("{}:{}", e.ts, e.seq))
    } else {
        None
    };
    Ok((
        StatusCode::OK,
        Json(json!({ "items": items, "next": next })),
    )
        .into_response())
}
