//! 管理 API：/api/admin/*（API 契约 §8）。无鉴权（v1 本地模式），访问控制交部署层。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use pegboard_core::audit::{ActionKind, Filter, Outcome};
use pegboard_core::files::FilesError;

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
        .route("/api/admin/apps/{id}", delete(delete_app))
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

/// 应用摘要（含生命周期状态与生效限额）。
#[derive(serde::Serialize)]
pub struct AppView {
    pub id: String,
    pub name: String,
    pub entry: String,
    pub enabled: bool,
    /// enabled | disabled | not_installed | invalid | missing（API 契约 §8）
    pub state: &'static str,
    pub permissions: serde_json::Value,
    pub limits: serde_json::Value,
    pub installed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn view_of(
    meta: &pegboard_core::app::AppMeta,
    state: &'static str,
    enabled: bool,
    installed_at: i64,
) -> AppView {
    AppView {
        id: meta.id.clone(),
        name: meta.name.clone(),
        entry: meta.manifest.entry.clone(),
        enabled,
        state,
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
        error: None,
    }
}

/// 未安装/无效候选视图（磁盘目录存在但未注册）。
fn candidate_view(
    id: &str,
    state: &'static str,
    meta: Option<pegboard_core::app::AppMeta>,
    error: Option<String>,
) -> AppView {
    let empty_limits = serde_json::json!({});
    match meta {
        Some(meta) => AppView {
            id: meta.id.clone(),
            name: meta.name.clone(),
            entry: meta.manifest.entry.clone(),
            enabled: false,
            state,
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
            installed_at: 0,
            error,
        },
        None => AppView {
            id: id.to_owned(),
            name: id.to_owned(),
            entry: String::new(),
            enabled: false,
            state,
            permissions: json!({}),
            limits: empty_limits,
            installed_at: 0,
            error,
        },
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
    // 磁盘扫描 + 候选校验是文件 IO：spawn_blocking（项目契约：async 上下文禁阻塞 IO）
    let installed_ids: std::collections::HashSet<String> =
        metas.iter().map(|m| m.id.clone()).collect();
    let apps_dir = state.config.storage.apps_dir.clone();
    let limits = state.config.limits;
    let candidates = tokio::task::spawn_blocking(move || {
        let mut out: Vec<(String, Option<pegboard_core::app::AppMeta>, Option<String>)> =
            Vec::new();
        let mut dir_names = std::collections::HashSet::new();
        let Ok(entries) = std::fs::read_dir(&apps_dir) else {
            return (out, dir_names);
        };
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            dir_names.insert(dir_name.clone());
            if installed_ids.contains(&dir_name) {
                continue;
            }
            match pegboard_core::app::load_candidate(&apps_dir, &dir_name, &limits) {
                Ok(meta) => out.push((meta.id.clone(), Some(meta), None)),
                Err(e) => out.push((dir_name, None, Some(e.to_string()))),
            }
        }
        (out, dir_names)
    })
    .await
    .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("候选扫描: {e}")))?;
    let (candidates, mut dir_names) = candidates;

    let mut views: Vec<AppView> = Vec::new();
    for meta in &metas {
        let (enabled, installed_at) = states.get(&meta.id).copied().unwrap_or((true, 0));
        // 已安装但产物目录被外部删除 → missing
        if !dir_names.contains(&meta.id) {
            let mut view = view_of(meta, "missing", enabled, installed_at);
            view.error = Some("产物目录不存在".into());
            views.push(view);
            continue;
        }
        let state_str = if enabled { "enabled" } else { "disabled" };
        views.push(view_of(meta, state_str, enabled, installed_at));
    }
    for (id, meta, error) in candidates {
        dir_names.insert(id.clone());
        if meta.is_some() {
            views.push(candidate_view(&id, "not_installed", meta, error));
        } else {
            views.push(candidate_view(&id, "invalid", None, error));
        }
    }
    views.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((StatusCode::OK, Json(views)).into_response())
}

async fn get_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // 已注册 → 注册表视图；未注册但磁盘候选有效 → 未安装视图
    let (meta, state_str, enabled, installed_at) =
        match state.app_meta_static(&id) {
            Ok(meta) => {
                let states = state.host_db.app_states().map_err(|e| {
                    ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}"))
                })?;
                let (enabled, installed_at) = states.get(&id).copied().unwrap_or((true, 0));
                let dir_gone = !state.config.storage.apps_dir.join(&id).is_dir();
                let state_str = if dir_gone {
                    "missing"
                } else if enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                (meta, state_str, enabled, installed_at)
            }
            Err(_) => {
                let apps_dir = state.config.storage.apps_dir.clone();
                let limits = state.config.limits;
                let id2 = id.clone();
                let candidate = tokio::task::spawn_blocking(move || {
                    pegboard_core::app::load_candidate(&apps_dir, &id2, &limits).ok()
                })
                .await
                .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("候选加载: {e}")))?;
                let Some(meta) = candidate else {
                    return Err(ApiError::app_not_found(&id));
                };
                let states = state.host_db.app_states().map_err(|e| {
                    ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}"))
                })?;
                let (enabled, installed_at) = states.get(&id).copied().unwrap_or((false, 0));
                let state_str = if states.contains_key(&id) {
                    if enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                } else {
                    "not_installed"
                };
                (Arc::new(meta), state_str, enabled, installed_at)
            }
        };
    let mut view = serde_json::to_value(view_of(&meta, state_str, enabled, installed_at))
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("序列化: {e}")))?;
    view["usage"] = app_usage(&state, &meta).await;
    view["root"] = json!(meta.root.display().to_string());
    view["data_dir"] = json!(state
        .config
        .storage
        .data_root
        .join("apps_data")
        .join(&meta.id)
        .display()
        .to_string());
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
    // 应用须已安装（注册行为安装时写入；不再以 b"{}" 兜底覆盖缓存）
    let meta = state.app_meta_static(id)?;
    let states = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    if !states.contains_key(id) {
        return Err(ApiError::invalid_request("应用未安装，无法启停"));
    }
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
    let installed_at = states.get(id).map_or(0, |s| s.1);
    Ok((
        StatusCode::OK,
        Json(view_of(
            &meta,
            if enabled { "enabled" } else { "disabled" },
            enabled,
            installed_at,
        )),
    )
        .into_response())
}

/// 安装：校验清单并注册磁盘上的应用（清单校验通过才成功）。
/// 文件 IO 在 spawn_blocking 内完成，注册表写锁不跨 await。
async fn install_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let apps_dir = state.config.storage.apps_dir.clone();
    let limits = state.config.limits;
    let state2 = Arc::clone(&state);
    let id2 = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut registry = state2
            .apps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.reload_one(&apps_dir, &id2, &limits)
    })
    .await
    .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("install join: {e}")))?;
    let meta = result.map_err(ApiError::from)?;
    state
        .host_db
        .upsert_app(
            &meta.id,
            &meta.name,
            &meta.root.display().to_string(),
            meta.manifest_json().as_slice(),
        )
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    audit_admin(&state, &id, Outcome::Ok, None);
    get_app(State(state), Path(id)).await
}

/// 卸载：注销注册表、失效签名 token；产物与数据目录保留，重装即恢复（数据模型 §9）。
async fn uninstall_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let registered = state.app_meta_static(&id).is_ok();
    let has_row = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?
        .contains_key(&id);
    if !registered && !has_row {
        return Err(ApiError::app_not_found(&id));
    }
    {
        let mut registry = state
            .apps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.remove(&id);
    }
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
        .remove(&id);
    audit_admin(&state, &id, Outcome::Ok, None);
    tracing::info!(app_id = %id, "app uninstalled");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 删除：卸载之外，进一步删除产物目录与数据目录；不可逆（数据模型 §9）。
async fn delete_app(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let registered = state.app_meta_static(&id).is_ok();
    let artifacts = state.config.storage.apps_dir.join(&id);
    // 删除对三种形态生效：已注册、有残留行（缺失）、磁盘候选（未安装）
    let has_row = state
        .host_db
        .app_states()
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?
        .contains_key(&id);
    if !registered && !has_row && !artifacts.is_dir() {
        return Err(ApiError::app_not_found(&id));
    }
    let state2 = Arc::clone(&state);
    let id2 = id.clone();
    let removal = tokio::task::spawn_blocking(move || {
        let stores_result = state2.stores.drop_app(&id2);
        let files_result = state2.files.drop_app(&id2);
        // 产物目录在数据目录之外，可能不存在
        let artifacts_result = if artifacts.is_dir() {
            std::fs::remove_dir_all(&artifacts).map_err(FilesError::from)
        } else {
            Ok(())
        };
        (stores_result, files_result, artifacts_result)
    })
    .await
    .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("delete join: {e}")))?;
    let (stores_result, files_result, artifacts_result) = removal;
    stores_result.map_err(ApiError::from)?;
    files_result.map_err(ApiError::from)?;
    artifacts_result.map_err(ApiError::from)?;
    state
        .host_db
        .delete_app_tokens(&id)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    state
        .host_db
        .remove_app(&id)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("host.db: {e}")))?;
    {
        let mut registry = state
            .apps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.remove(&id);
    }
    state
        .disabled
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id);
    audit_admin(&state, &id, Outcome::Ok, None);
    tracing::info!(app_id = %id, "app deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Admin 操作审计事件（target = 应用 id）。
fn audit_admin(state: &Arc<AppState>, app_id: &str, outcome: Outcome, error_code: Option<&str>) {
    let event = pegboard_core::audit::AuditEvent {
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        seq: 0, // record 时由 AuditorShared 分配
        app_id: Some(app_id.to_owned()),
        subject: None,
        action: ActionKind::Admin,
        target: Some(app_id.to_owned()),
        outcome,
        error_code: error_code.map(str::to_owned),
        duration_ms: 0,
    };
    if let Err(e) = state.auditor.record(event) {
        tracing::error!(error = %e, "admin audit record failed");
    }
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    app: Option<String>,
    subject: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    /// 倒向游标 "ts:seq"：取更早一页
    before: Option<String>,
    /// 正向游标 "ts:seq"：取更新一页
    after: Option<String>,
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
    let parse_cursor = |raw: &str| -> Result<(i64, u64), ApiError> {
        raw.split_once(':')
            .and_then(|(t, s)| Some((t.parse::<i64>().ok()?, s.parse::<u64>().ok()?)))
            .ok_or_else(|| ApiError::invalid_request(format!("游标非法: {raw}（须为 ts:seq）")))
    };
    let before = match q.before.as_deref() {
        None => None,
        Some(raw) => Some(parse_cursor(raw)?),
    };
    let after = match q.after.as_deref() {
        None => None,
        Some(raw) => Some(parse_cursor(raw)?),
    };
    if before.is_some() && after.is_some() {
        return Err(ApiError::invalid_request("before 与 after 不可同时使用"));
    }
    let filter = Filter {
        app_id: q.app.clone(),
        subject: q.subject.clone(),
        action,
        outcome,
        since: q.since,
        until: q.until,
        before,
        after,
        limit: q.limit.map(|l| l.min(1000)),
    };
    let page = state
        .auditor
        .query_page(&filter)
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("审计查询: {e}")))?;
    let events = &page.items;
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
    // 双向游标：next 取更早（末项），prev 取更新（首项）
    let cursor_of = |e: &pegboard_core::audit::AuditEvent| format!("{}:{}", e.ts, e.seq);
    let next = if page.has_older {
        events.last().map(cursor_of)
    } else {
        None
    };
    let prev = if page.has_newer {
        events.first().map(cursor_of)
    } else {
        None
    };
    Ok((
        StatusCode::OK,
        Json(json!({ "items": items, "next": next, "prev": prev })),
    )
        .into_response())
}
