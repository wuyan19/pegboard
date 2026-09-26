//! Store API handler：/api/store/kv/*。
//! 协议转换只在此层；配额与隔离语义在 core::store。
//!
//! 处理顺序（ingress 骨架）：RequestContext → guard.check_capability →
//! spawn_blocking(core 同步调用) → 审计 → 响应。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use pegboard_core::audit::ActionKind;
use pegboard_core::guard::Action;
use pegboard_core::store::KvOp;

use crate::ingress::context::RequestContext;
use crate::ingress::error::{code, ApiError};
use crate::ingress::{trace_audit, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/store/kv", get(list))
        .route("/api/store/kv/batch", post(batch))
        .route(
            "/api/store/kv/{*key}",
            get(get_one).put(put_one).delete(delete_one),
        )
}

/// key 是 URL 编码字符串；%2F 解码为 key 内容而非路径分隔符（API 契约 §4.1）。
/// axum Path 已做一次解码；此处仅兜底二次编码场景。
fn decode_key(raw: String) -> Result<String, ApiError> {
    percent_encoding::percent_decode_str(&raw)
        .decode_utf8()
        .map(|cow| cow.into_owned())
        .map_err(|_| ApiError::invalid_request("key 不是合法 UTF-8"))
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let key = decode_key(key)?;
    trace_audit(&state, &ctx, ActionKind::Store, key_prefix(&key), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Store)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        let value = tokio::task::spawn_blocking(move || {
            h.stores.with(&app.id, &app.limits, |s| s.get(&key))
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("store join: {e}")))?
        .map_err(ApiError::from)?;
        match value {
            Some(bytes) => {
                let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                    ApiError::new(code::UPSTREAM_ERROR, 502, format!("value 损坏: {e}"))
                })?;
                Ok((StatusCode::OK, Json(json!({ "value": value }))).into_response())
            }
            // 未命中返回 null 体，不抛错（SDK 约定：get 未命中为 null）
            None => Ok((StatusCode::OK, Json(json!({ "value": null }))).into_response()),
        }
    })
    .await
}

async fn put_one(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Path(key): Path<String>,
    Json(body): Json<PutBody>,
) -> Result<Response, ApiError> {
    let key = decode_key(key)?;
    let bytes = serde_json::to_vec(&body.value)
        .map_err(|e| ApiError::invalid_request(format!("value 序列化失败: {e}")))?;
    trace_audit(&state, &ctx, ActionKind::Store, key_prefix(&key), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Store)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        tokio::task::spawn_blocking(move || {
            h.stores.with(&app.id, &app.limits, |s| s.set(&key, &bytes))
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("store join: {e}")))?
        .map_err(ApiError::from)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

async fn delete_one(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let key = decode_key(key)?;
    trace_audit(&state, &ctx, ActionKind::Store, key_prefix(&key), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Store)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        tokio::task::spawn_blocking(move || {
            h.stores.with(&app.id, &app.limits, |s| s.delete(&key))
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("store join: {e}")))?
        .map_err(ApiError::from)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    prefix: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let prefix = query.prefix.unwrap_or_default();
    trace_audit(
        &state,
        &ctx,
        ActionKind::Store,
        key_prefix(&prefix),
        async {
            state
                .guard
                .check_capability(&ctx.app, Action::Store)
                .map_err(ApiError::from)?;
            let app = Arc::clone(&ctx.app);
            let h = Arc::clone(&state);
            let items = tokio::task::spawn_blocking(move || {
                h.stores
                    .with(&app.id, &app.limits, |s| s.list(Some(&prefix)))
            })
            .await
            .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("store join: {e}")))?
            .map_err(ApiError::from)?;
            let items: Vec<Value> = items
                .iter()
                .map(|entry| {
                    let value: Value = serde_json::from_slice(&entry.value).unwrap_or(Value::Null);
                    json!({ "key": entry.key, "value": value })
                })
                .collect();
            Ok((StatusCode::OK, Json(json!({ "items": items }))).into_response())
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
struct PutBody {
    value: Value,
}

#[derive(Debug, Deserialize)]
struct BatchBody {
    ops: Vec<BatchOp>,
}

#[derive(Debug, Deserialize)]
struct BatchOp {
    op: String,
    key: String,
    #[serde(default)]
    value: Option<Value>,
}

async fn batch(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Json(body): Json<BatchBody>,
) -> Result<Response, ApiError> {
    let mut ops = Vec::with_capacity(body.ops.len());
    for op in &body.ops {
        match op.op.as_str() {
            "set" => {
                let value = op.value.clone().unwrap_or(Value::Null);
                let bytes = serde_json::to_vec(&value)
                    .map_err(|e| ApiError::invalid_request(format!("value 序列化失败: {e}")))?;
                ops.push(KvOp::Set {
                    key: op.key.clone(),
                    value: bytes,
                });
            }
            "delete" => ops.push(KvOp::Delete {
                key: op.key.clone(),
            }),
            other => {
                return Err(ApiError::invalid_request(format!(
                    "未知批量操作 `{other}`（须为 set | delete）"
                )))
            }
        }
    }
    trace_audit(
        &state,
        &ctx,
        ActionKind::Store,
        Some("batch".into()),
        async {
            state
                .guard
                .check_capability(&ctx.app, Action::Store)
                .map_err(ApiError::from)?;
            let app = Arc::clone(&ctx.app);
            let h = Arc::clone(&state);
            tokio::task::spawn_blocking(move || {
                h.stores.with(&app.id, &app.limits, |s| s.batch(&ops))
            })
            .await
            .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("store join: {e}")))?
            .map_err(ApiError::from)?;
            Ok(StatusCode::NO_CONTENT.into_response())
        },
    )
    .await
}

/// 审计 target 脱敏：只记 key 前缀（前 64 字节）。
fn key_prefix(key: &str) -> Option<String> {
    if key.is_empty() {
        None
    } else {
        Some(key.chars().take(64).collect())
    }
}
