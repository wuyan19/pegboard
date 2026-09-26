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
        .route("/api/admin/apps", get(list_apps))
        .route("/api/admin/apps/{id}", get(get_app))
        .route("/api/admin/apps/{id}/install", post(install_app))
        .route("/api/admin/apps/{id}/uninstall", post(uninstall_app))
        .route("/api/admin/apps/{id}/enable", post(enable_app))
        .route("/api/admin/apps/{id}/disable", post(disable_app))
        .route("/api/admin/logs", get(list_logs))
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
    Ok((StatusCode::OK, Json(view_of(&meta, enabled, installed_at))).into_response())
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
    let filter = Filter {
        app_id: q.app.clone(),
        subject: q.subject.clone(),
        action,
        outcome,
        since: q.since,
        until: q.until,
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
    Ok((
        StatusCode::OK,
        Json(json!({ "items": items, "next": null })),
    )
        .into_response())
}
