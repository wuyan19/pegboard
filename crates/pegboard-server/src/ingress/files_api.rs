//! Files API handler：/api/files/*（API 契约 §4.2）。
//!
//! 上传：multipart 流式落暂存（不整文件入内存），core 完成去重落位。
//! 下载：元数据 + 内容句柄，Range/ETag/Content-Disposition 在此层。
//! 签名访问：?token= 免 subject，校验在 files（经 host.db 接口），跳过 guard。

use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use pegboard_core::audit::ActionKind;
use pegboard_core::files::{FileMeta, FilesError};
use pegboard_core::guard::Action;

use crate::ingress::context::RequestContext;
use crate::ingress::error::{code, ApiError};
use crate::ingress::{trace_audit, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/files", get(list).post(upload))
        .route("/api/files/{id}", get(get_one).delete(delete_one))
        .route("/api/files/{id}/sign", post(sign))
}

fn meta_json(m: &FileMeta) -> serde_json::Value {
    json!({
        "id": m.id,
        "name": m.name,
        "size": m.size,
        "mime": m.mime,
        "created": m.created_at,
    })
}

/// 上传：multipart 字段 `file`，流式写暂存；超单文件上限即断。
async fn upload(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    trace_audit(&state, &ctx, ActionKind::Files, None, async {
        state
            .guard
            .check_capability(&ctx.app, Action::Files)
            .map_err(ApiError::from)?;

        // 暂存路径：data_root/tmp/<ulid>.part（与 apps_data 同盘，rename 原子）
        let staged = state
            .config
            .storage
            .data_root
            .join("tmp")
            .join(pegboard_core::files::new_staging_name());
        let mut name = None;
        let mut mime = None;
        let mut size: u64 = 0;
        while let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|e| ApiError::invalid_request(format!("multipart: {e}")))?
        {
            if field.name() != Some("file") {
                // 跳过非 file 字段
                continue;
            }
            name = Some(
                field
                    .file_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "unnamed".to_owned()),
            );
            mime = Some(
                field
                    .content_type()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
            );
            use tokio::io::AsyncWriteExt;
            if let Some(parent) = staged.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("暂存目录: {e}")))?;
            }
            let mut out = tokio::fs::File::create(&staged)
                .await
                .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("暂存失败: {e}")))?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| ApiError::invalid_request(format!("multipart chunk: {e}")))?
            {
                size += chunk.len() as u64;
                if size > ctx.app.limits.file_bytes {
                    let _ = tokio::fs::remove_file(&staged).await;
                    return Err(ApiError::new(
                        code::LIMIT_EXCEEDED,
                        413,
                        format!("文件超上限: {size} > {}", ctx.app.limits.file_bytes),
                    )
                    .with_detail(json!({"limit": "file_bytes", "max": ctx.app.limits.file_bytes, "size": size})));
                }
                out.write_all(&chunk)
                    .await
                    .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("暂存写入: {e}")))?;
            }
            out.flush()
                .await
                .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("暂存写入: {e}")))?;
            break; // 只取第一个 file 字段
        }
        let (Some(name), Some(mime)) = (name, mime) else {
            let _ = tokio::fs::remove_file(&staged).await;
            return Err(ApiError::invalid_request("缺少 multipart 字段 `file`"));
        };
        let app = Arc::clone(&ctx.app);
        let staged = Arc::new(staged);
        let upload_state = Arc::clone(&state);
        let staged_for_cleanup = Arc::clone(&staged);
        let upload_result = tokio::task::spawn_blocking(move || {
            upload_state.files.with(&app.id, &app.limits, |f| {
                let src = std::io::BufReader::new(std::fs::File::open(staged_for_cleanup.as_ref())?);
                f.upload(&name, &mime, src)
            })
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))
        .and_then(|r| r.map_err(ApiError::from));
        // 暂存文件使命完成：无论成败都清理
        let _ = tokio::fs::remove_file(staged.as_ref()).await;
        let meta = upload_result?;
        Ok((StatusCode::OK, Json(meta_json(&meta))).into_response())
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    prefix: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let prefix = query.prefix.clone();
    let cursor = query.cursor.clone();
    let limit = query.limit.unwrap_or(50);
    trace_audit(&state, &ctx, ActionKind::Files, prefix.clone(), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Files)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        let page = tokio::task::spawn_blocking(move || {
            h.files.with(&app.id, &app.limits, |f| {
                f.list(prefix.as_deref(), cursor.as_deref(), limit)
            })
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))?
        .map_err(ApiError::from)?;
        Ok((
            StatusCode::OK,
            Json(json!({
                "items": page.items.iter().map(meta_json).collect::<Vec<_>>(),
                "next": page.next,
            })),
        )
            .into_response())
    })
    .await
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    token: Option<String>,
}

/// 下载：无 token 走完整身份+治理；带 token 免 subject（签名路径）。
async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    match query.token.as_deref() {
        Some(token) => {
            // 签名访问：token → app_id（校验在 files，经 host.db 接口），跳过 subject 与 guard
            let resolved = state.signer.resolve(token, &id).map_err(ApiError::from)?;
            if let Some(app_id) = resolved {
                let app = state.app_meta(&app_id)?;
                let h = Arc::clone(&state);
                let file_id = id.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    h.files
                        .with(&app.id, &app.limits, |f| f.open_content(&file_id))
                })
                .await
                .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))?
                .map_err(ApiError::from)?;
                serve_file_response(outcome, &method, &headers).await
            } else {
                Err(ApiError::new(code::TOKEN_INVALID, 401, "签名 token 无效"))
            }
        }
        None => {
            let ctx = crate::ingress::context::manual_context(&state, &headers)?;
            trace_audit(&state, &ctx, ActionKind::Files, Some(id.clone()), async {
                state
                    .guard
                    .check_capability(&ctx.app, Action::Files)
                    .map_err(ApiError::from)?;
                let app = Arc::clone(&ctx.app);
                let h = Arc::clone(&state);
                let file_id = id.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    h.files
                        .with(&app.id, &app.limits, |f| f.open_content(&file_id))
                })
                .await
                .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))?
                .map_err(ApiError::from)?;
                serve_file_response(outcome, &method, &headers).await
            })
            .await
        }
    }
}

/// 文件内容响应：Content-Disposition、ETag(sha256)、Range 单范围。
async fn serve_file_response(
    outcome: Option<(FileMeta, std::fs::File)>,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let Some((meta, std_file)) = outcome else {
        return Err(ApiError::new(code::NOT_FOUND, 404, "文件不存在"));
    };
    let mut file = tokio::fs::File::from(std_file);
    let etag = format!("\"{}\"", meta.sha256);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(&etag))
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response()));
    }
    let disposition = format!(
        "attachment; filename=\"{}\"",
        meta.name.replace(['"', '\\'], "_")
    );
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, meta.mime.clone())
        .header(header::ETAG, etag)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::ACCEPT_RANGES, "bytes");

    let mut start = 0u64;
    let mut len = meta.size;
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        match parse_range(range, meta.size) {
            Ok(Some((s, l))) => {
                start = s;
                len = l;
                builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
                    header::CONTENT_RANGE,
                    format!("bytes {s}-{}/{}", s + l - 1, meta.size),
                );
            }
            Ok(None) => {}
            Err(()) => {
                return Ok(builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", meta.size))
                    .body(Body::empty())
                    .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response()));
            }
        }
    }
    builder = builder.header(header::CONTENT_LENGTH, len.to_string());
    if *method == Method::HEAD || len == 0 {
        return Ok(builder
            .body(Body::empty())
            .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response()));
    }
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("seek: {e}")))?;
    }
    // 流式读取（64KiB 块），绝不整文件入内存
    let stream = async_stream_file(file, len);
    Ok(builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response()))
}

fn async_stream_file(
    mut file: tokio::fs::File,
    len: u64,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    futures_util::stream::unfold((file, len), |(mut file, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let chunk_len = remaining.min(64 * 1024) as usize;
        let mut buf = vec![0u8; chunk_len];
        match file.read_exact(&mut buf).await {
            Ok(_n) => {
                let remaining = remaining - chunk_len as u64;
                Some((Ok(bytes::Bytes::from(buf)), (file, remaining)))
            }
            Err(e) => Some((Err(e), (file, 0))),
        }
    })
}

fn parse_range(value: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let (start_s, end_s) = spec.split_once('-').ok_or(())?;
    if start_s.is_empty() {
        let n: u64 = end_s.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        let len = n.min(size);
        Ok(Some((size - len, len)))
    } else {
        let start: u64 = start_s.parse().map_err(|_| ())?;
        if start >= size {
            return Err(());
        }
        let end = if end_s.is_empty() {
            size - 1
        } else {
            end_s.parse::<u64>().map_err(|_| ())?.min(size - 1)
        };
        if end < start {
            return Err(());
        }
        Ok(Some((start, end - start + 1)))
    }
}

async fn delete_one(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    trace_audit(&state, &ctx, ActionKind::Files, Some(id.clone()), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Files)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        let file_id = id.clone();
        tokio::task::spawn_blocking(move || {
            h.files.with(&app.id, &app.limits, |f| f.delete(&file_id))
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))?
        .map_err(ApiError::from)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

#[derive(Debug, Deserialize)]
struct SignBody {
    ttl: u64,
}

async fn sign(
    State(state): State<Arc<AppState>>,
    ctx: RequestContext,
    Path(id): Path<String>,
    Json(body): Json<SignBody>,
) -> Result<Response, ApiError> {
    trace_audit(&state, &ctx, ActionKind::Files, Some(id.clone()), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Files)
            .map_err(ApiError::from)?;
        let ttl_max = ctx.app.limits.sign_ttl_max;
        let app = Arc::clone(&ctx.app);
        let h = Arc::clone(&state);
        let file_id = id.clone();
        // 文件须存在
        let exists = tokio::task::spawn_blocking(move || {
            h.files.with(&app.id, &app.limits, |f| f.meta(&file_id))
        })
        .await
        .map_err(|e| ApiError::new(code::UPSTREAM_ERROR, 502, format!("files join: {e}")))?
        .map_err(ApiError::from)?;
        if exists.is_none() {
            return Err(ApiError::new(code::NOT_FOUND, 404, "文件不存在"));
        }
        let token = state
            .signer
            .sign(&ctx.app.id, &id, body.ttl, ttl_max)
            .map_err(ApiError::from)?;
        Ok((
            StatusCode::OK,
            Json(json!({ "url": format!("/api/files/{id}?token={token}") })),
        )
            .into_response())
    })
    .await
}

/// core FilesError → ApiError（API 契约 §6）。
impl From<FilesError> for ApiError {
    fn from(e: FilesError) -> Self {
        match &e {
            FilesError::NotFound(msg) => Self::new(code::NOT_FOUND, 404, msg.clone()),
            FilesError::TooLarge { size, max } => Self::new(
                code::LIMIT_EXCEEDED,
                413,
                format!("文件超上限: {size} > {max}"),
            )
            .with_detail(json!({ "limit": "file_bytes", "max": max, "size": size })),
            FilesError::QuotaExceeded { used, add, max } => Self::new(
                code::LIMIT_EXCEEDED,
                413,
                format!("超出文件总量配额: used {used} + {add} > {max}"),
            )
            .with_detail(
                json!({ "limit": "file_total_bytes", "used": used, "add": add, "max": max }),
            ),
            FilesError::InvalidName(msg) => Self::invalid_request(msg.clone()),
            FilesError::TokenInvalid => {
                Self::new(code::TOKEN_INVALID, 401, "签名 token 无效或过期")
            }
            FilesError::HostDb(msg) | FilesError::Entropy(msg) => {
                Self::new(code::UPSTREAM_ERROR, 502, msg.clone())
            }
            FilesError::Io(_) | FilesError::Db(_) => {
                Self::new(code::UPSTREAM_ERROR, 502, format!("文件存储错误: {e}"))
            }
        }
    }
}
