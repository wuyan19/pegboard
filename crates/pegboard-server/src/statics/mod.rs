//! 静态托管：应用产物直出、SPA 回退、Range/ETag/缓存、text/html 注入 /sdk.js。
//!
//! 手写而非 ServeDir：回退规则与自定义头（含 body 注入）更直接。
//! 不经 identity / guard / audit（模块划分 §5）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::ingress::error::ApiError;
use crate::ingress::AppState;

/// 挂载静态路由：GET /apps/{app_id}/{*path} 与 GET /sdk.js、/sdk.d.ts。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/apps/{app_id}/{*path}", get(serve_app).head(serve_app))
        .route("/apps/{app_id}", get(serve_app_bare).head(serve_app_bare))
        .route("/apps/{app_id}/", get(serve_app_bare).head(serve_app_bare))
        .route("/sdk.js", get(serve_sdk_js))
        .route("/sdk.d.ts", get(serve_sdk_dts))
}

/// 静态托管 handler。命中文件直出；未命中回退 entry（SPA）。
async fn serve_app(
    State(state): State<Arc<AppState>>,
    AxumPath((app_id, path)): AxumPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    serve_impl(&state, &app_id, &path, method, headers).await
}

/// /apps/{id}（无尾斜杠）：等价空路径，服务入口文件。
async fn serve_app_bare(
    State(state): State<Arc<AppState>>,
    AxumPath(app_id): AxumPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    serve_impl(&state, &app_id, "", method, headers).await
}

/// 固定身份模式下的 subject（注入页面供 host.user 使用）。
fn fixed_subject(state: &Arc<AppState>) -> Option<String> {
    use pegboard_core::config::IdentityConfig;
    match &state.config.identity {
        IdentityConfig::Fixed { subject } => subject.clone(),
        _ => None,
    }
}

async fn serve_impl(
    state: &Arc<AppState>,
    app_id: &str,
    path: &str,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let app = match state.app_meta_static(app_id) {
        Ok(app) => app,
        Err(e) => return e.into_response(),
    };
    let subject = fixed_subject(state).clone();
    // 目标相对路径：空路径用 entry；目录路径补 index.html
    let rel: String = if path.is_empty() {
        app.manifest.entry.trim_start_matches("./").to_owned()
    } else if path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_owned()
    };

    if rel.split('/').any(|seg| seg == "..") {
        return ApiError::not_found("路径非法").into_response();
    }
    match resolve_in_root(&app.root, &rel).await {
        Some(absolute) => {
            match serve_file(&absolute, &rel, &app, &method, &headers, subject.clone()).await {
                Ok(response) => response,
                Err(StaticError::Missing) => {
                    fallback_response(&app, &rel, &method, &headers, subject).await
                }
                Err(StaticError::Io(e)) => {
                    tracing::error!(error = %e, app_id = %app.id, "static io error");
                    ApiError::not_found("资源不可用").into_response()
                }
            }
        }
        // 文件不存在（canonicalize 失败）或符号链接越界 → 回退判定
        None => fallback_response(&app, &rel, &method, &headers, subject).await,
    }
}

enum StaticError {
    Missing,
    Io(std::io::Error),
}

fn http_err(e: axum::http::Error) -> StaticError {
    StaticError::Io(std::io::Error::other(e.to_string()))
}

/// canonicalize + 前缀校验：路径必须在应用根目录内。
/// async 版 canonicalize：不在 async 上下文做阻塞 IO。
async fn resolve_in_root(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let joined = root.join(rel);
    let canon = tokio::fs::canonicalize(&joined).await.ok()?;
    if !canon.starts_with(root) {
        return None;
    }
    if tokio::fs::metadata(&canon).await.ok()?.is_file() {
        Some(canon)
    } else {
        None
    }
}

/// 未命中回退：请求含扩展名（资源）→ 404；否则视为导航，回退 entry（SPA）。
async fn fallback_response(
    app: &Arc<pegboard_core::app::AppMeta>,
    rel: &str,
    method: &Method,
    headers: &HeaderMap,
    subject: Option<String>,
) -> Response {
    let looks_like_asset = rel
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'));
    if looks_like_asset {
        return ApiError::not_found("资源不存在").into_response();
    }
    let entry_rel = app.manifest.entry.trim_start_matches("./");
    match resolve_in_root(&app.root, entry_rel).await {
        Some(entry_abs) => {
            match serve_file(&entry_abs, entry_rel, app, method, headers, subject).await {
                Ok(response) => response,
                Err(_) => ApiError::not_found("入口文件不存在").into_response(),
            }
        }
        None => ApiError::not_found("入口文件不存在").into_response(),
    }
}

/// HTTP 日期（IMF-fixdate）格式化，无外部依赖。
fn http_date(t: SystemTime) -> String {
    const WEEK: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTH: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // civil_from_days（Howard Hinnant 算法）
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let weekday = WEEK[days.rem_euclid(7) as usize];
    let month = MONTH[(m - 1) as usize];
    format!("{weekday}, {d:02} {month} {y} {h:02}:{mi:02}:{s:02} GMT")
}

/// 弱 ETag：mtime + size。
fn weak_etag(mtime: SystemTime, size: u64) -> String {
    let ms = mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis();
    format!("W/\"{ms:x}-{size:x}\"")
}

/// 带内容哈希风格的路径段（8+ 连续十六进制字符）→ immutable 长缓存；其余 no-cache。
fn cache_control_for(rel: &str) -> &'static str {
    let hashed = rel.split('/').any(|seg| {
        let bytes = seg.as_bytes();
        let mut run = 0usize;
        let mut max_run = 0usize;
        for &b in bytes {
            if b.is_ascii_hexdigit() {
                run += 1;
                max_run = max_run.max(run);
            } else {
                run = 0;
            }
        }
        max_run >= 8
    });
    if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// 单范围解析。返回 (start, len)；None 表示忽略 Range（整文件）。
/// Err(()) 表示不可满足（416）。
fn parse_range(value: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = value.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None); // 多范围降级为整文件
    }
    let (start_s, end_s) = match spec.split_once('-') {
        Some(pair) => pair,
        None => return Err(()),
    };
    if start_s.is_empty() {
        // 后缀范围 bytes=-N
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

/// text/html 注入上限：超过则原样返回（异常大的 HTML 不值得缓冲）。
const MAX_INJECT_BYTES: u64 = 4 * 1024 * 1024;

/// 注入引导与 SDK script：紧跟 <head ...> 之后；无 head 则前置。
fn inject_sdk(html: &str, app_id: &str, shim: bool, boot_subject: Option<String>) -> String {
    let boot = format!(
        "<script>window.__PEGBOARD__={}</script>\n<script src=\"/sdk.js\"></script>",
        serde_json::json!({ "appId": app_id, "shim": shim, "subject": boot_subject })
    );
    let lower = html.to_ascii_lowercase();
    if let Some(pos) = lower.find("<head") {
        if let Some(close) = lower[pos..].find('>') {
            let at = pos + close + 1;
            let mut out = String::with_capacity(html.len() + boot.len() + 2);
            out.push_str(&html[..at]);
            out.push('\n');
            out.push_str(&boot);
            out.push_str(&html[at..]);
            return out;
        }
    }
    format!("{boot}\n{html}")
}

async fn serve_file(
    absolute: &Path,
    rel: &str,
    app: &Arc<pegboard_core::app::AppMeta>,
    method: &Method,
    headers: &HeaderMap,
    subject: Option<String>,
) -> Result<Response, StaticError> {
    let metadata = tokio::fs::metadata(absolute).await.map_err(map_missing)?;
    if !metadata.is_file() {
        return Err(StaticError::Missing);
    }
    let size = metadata.len();
    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let mime = mime_guess::from_path(rel)
        .first_or_octet_stream()
        .to_string();

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime.clone())
        .header(header::ETAG, weak_etag(mtime, size))
        .header(header::LAST_MODIFIED, http_date(mtime))
        .header(header::CACHE_CONTROL, cache_control_for(rel))
        .header(header::ACCEPT_RANGES, "bytes");

    // 条件请求：If-None-Match 命中 → 304
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(&weak_etag(mtime, size)))
    {
        let response = builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .map_err(http_err)?;
        return Ok(response.into_response());
    }

    let is_html = mime.starts_with("text/html");
    let is_head = *method == Method::HEAD;

    // HTML：读入注入（文件级缓冲；HTML 是应用外壳，通常很小）。
    // 带 Range 的请求按字节范围流式返回，不做注入（范围请求是下载/媒体语义，
    // 注入会破坏字节偏移）。
    let has_range = headers.contains_key(header::RANGE);
    if is_html && !has_range && size <= MAX_INJECT_BYTES {
        let mut buf = Vec::with_capacity(size as usize);
        let mut file = tokio::fs::File::open(absolute)
            .await
            .map_err(StaticError::Io)?;
        file.read_to_end(&mut buf).await.map_err(StaticError::Io)?;
        let html = String::from_utf8_lossy(&buf).into_owned();
        let injected = inject_sdk(&html, &app.id, app.manifest.permissions.shim, subject);
        let bytes = injected.into_bytes();
        let response = builder
            .header(header::CONTENT_LENGTH, bytes.len().to_string())
            .body(if is_head {
                Body::empty()
            } else {
                Body::from(bytes)
            })
            .map_err(http_err)?;
        return Ok(response.into_response());
    }
    if is_html {
        tracing::warn!(size, app_id = %app.id, "html 超过注入上限，原样返回");
    }

    // Range 处理
    let mut start = 0u64;
    let mut len = size;
    if let Some(range_value) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        match parse_range(range_value, size) {
            Ok(Some((s, l))) => {
                start = s;
                len = l;
                builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
                    header::CONTENT_RANGE,
                    format!("bytes {s}-{}/{size}", s + l - 1),
                );
            }
            Ok(None) => {}
            Err(()) => {
                let response = builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .map_err(http_err)?;
                return Ok(response.into_response());
            }
        }
    }

    builder = builder.header(header::CONTENT_LENGTH, len.to_string());
    if is_head || len == 0 {
        let response = builder.body(Body::empty()).map_err(http_err)?;
        return Ok(response.into_response());
    }

    let file = tokio::fs::File::open(absolute)
        .await
        .map_err(StaticError::Io)?;
    let mut file = file;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(StaticError::Io)?;
    }
    let stream = ReaderStream::with_capacity(file.take(len), 64 * 1024);
    let response = builder.body(Body::from_stream(stream)).map_err(http_err)?;
    Ok(response.into_response())
}

fn map_missing(e: std::io::Error) -> StaticError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StaticError::Missing
    } else {
        StaticError::Io(e)
    }
}

/// FNV-1a 64 位：SDK 静态资源的弱内容哈希（ETag 用）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn sdk_response(bytes: &'static [u8], content_type: &str) -> Response {
    let etag = format!("W/\"{:x}\"", fnv1a(bytes));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .unwrap_or_else(|e| ApiError::invalid_request(e.to_string()).into_response())
}

async fn serve_sdk_js() -> Response {
    sdk_response(
        pegboard_sdk::SDK_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

async fn serve_sdk_dts() -> Response {
    sdk_response(
        pegboard_sdk::SDK_DTS.as_bytes(),
        "application/typescript; charset=utf-8",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    struct TestEnv {
        _root: tempfile::TempDir,
        state: Arc<AppState>,
    }

    fn state_with_app(files: &[(&str, &[u8])]) -> TestEnv {
        let root = tempfile::TempDir::new().expect("tempdir");
        let apps = root.path().join("apps");
        let app_dir = apps.join("t");
        std::fs::create_dir_all(&app_dir).expect("mkdir");
        for (name, content) in files {
            let path = app_dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(path, content).expect("write");
        }
        std::fs::write(
            app_dir.join("manifest.json"),
            r#"{"id":"t","name":"T","entry":"index.html"}"#,
        )
        .expect("write manifest");
        let mut config = pegboard_core::config::Config::default();
        config.storage.apps_dir = apps.clone();
        config.storage.data_root = root.path().join("data");
        let outcome = pegboard_core::app::AppRegistry::scan(&apps, &config.limits).expect("scan");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        let auditor = pegboard_core::audit::Auditor::start(pegboard_core::audit::AuditConfig {
            log_dir: root.path().join("data/logs"),
            ..Default::default()
        })
        .expect("auditor")
        .handle();
        let host_db = Arc::new(
            pegboard_core::app::HostDb::open(&root.path().join("data/host.db")).expect("host db"),
        );
        let signer = Arc::new(pegboard_core::files::Signer::new(Arc::clone(&host_db)));
        let limits = config.limits;
        let proxy = pegboard_core::proxy::Proxy::new(
            Arc::new(pegboard_core::guard::Guard::new()),
            pegboard_core::proxy::ProxyConfig::default(),
        )
        .expect("proxy");
        let state = Arc::new(AppState {
            identity: pegboard_core::identity::Identity::new(&config.identity),
            guard: Arc::new(pegboard_core::guard::Guard::new()),
            stores: pegboard_core::store::StoreManager::new(
                root.path().join("data/apps_data"),
                limits,
            ),
            files: pegboard_core::files::FilesManager::new(
                root.path().join("data/apps_data"),
                root.path().join("data/tmp"),
            ),
            signer: Arc::clone(&signer),
            host_db: Arc::clone(&host_db),
            started: std::time::Instant::now(),
            admin_token: None,
            disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
            proxy: Arc::new(proxy),
            config,
            apps: std::sync::RwLock::new(outcome.registry),
            auditor,
            control: Arc::new(crate::host::NoControl),
            update: Arc::new(crate::host::update::Updater::new("", std::env::temp_dir())),
        });
        TestEnv { _root: root, state }
    }

    async fn get(env: &TestEnv, uri: &str) -> axum::http::Response<Body> {
        let req = Request::get(uri).body(Body::empty()).expect("request");
        build_router_for_test(env)
            .oneshot(req)
            .await
            .expect("oneshot")
    }

    fn build_router_for_test(env: &TestEnv) -> axum::Router {
        crate::ingress::build_router(Arc::clone(&env.state))
    }

    async fn body_string(response: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn serves_index() {
        let env = state_with_app(&[("index.html", b"<h1>hi</h1>")]);
        let r = get(&env, "/apps/t/index.html").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html")));
    }

    #[tokio::test]
    async fn html_injects_sdk() {
        let env = state_with_app(&[(
            "index.html",
            b"<html><head><title>t</title></head><body></body></html>",
        )]);
        let r = get(&env, "/apps/t/index.html").await;
        let text = body_string(r).await;
        assert!(
            text.contains(r#"<script src="/sdk.js"></script>"#),
            "{text}"
        );
        assert!(
            text.contains(r#"window.__PEGBOARD__={"appId":"t","shim":true,"subject":null}"#),
            "{text}"
        );
        // 注入在 <head> 之后、应用内容之前
        let head_pos = text.find("<title>").expect("title");
        let boot_pos = text.find("__PEGBOARD__").expect("boot");
        assert!(boot_pos < head_pos, "{text}");
    }

    #[tokio::test]
    async fn serves_nested_asset() {
        let env = state_with_app(&[("index.html", b"x"), ("assets/a.js", b"console.log(1)")]);
        let r = get(&env, "/apps/t/assets/a.js").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("javascript")));
    }

    #[tokio::test]
    async fn empty_path_serves_entry() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t/").await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bare_app_path_serves_entry() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t").await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn spa_fallback_on_navigation() {
        let env = state_with_app(&[("index.html", b"<html>app</html>")]);
        let r = get(&env, "/apps/t/some/route").await;
        assert_eq!(r.status(), StatusCode::OK);
        let text = body_string(r).await;
        assert!(text.contains("app"), "{text}");
    }

    #[tokio::test]
    async fn missing_asset_returns_404() {
        let env = state_with_app(&[("index.html", b"app")]);
        let r = get(&env, "/apps/t/missing.js").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn traversal_rejected() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t/../../secret.txt").await;
        assert!(
            r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::BAD_REQUEST,
            "status: {}",
            r.status()
        );
    }

    #[tokio::test]
    async fn invalid_app_id_returns_404() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/INVALID/x").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_app_returns_404() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/nope/index.html").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn range_returns_206() {
        let env = state_with_app(&[("index.html", b"x"), ("f.bin", &[7u8; 1024])]);
        let req = Request::get("/apps/t/f.bin")
            .header(header::RANGE, "bytes=0-99")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some("bytes 0-99/1024")
        );
        assert_eq!(
            r.headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("100")
        );
    }

    #[tokio::test]
    async fn range_open_end() {
        let env = state_with_app(&[("index.html", b"x"), ("f.bin", &[7u8; 100])]);
        let req = Request::get("/apps/t/f.bin")
            .header(header::RANGE, "bytes=50-")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some("bytes 50-99/100")
        );
    }

    #[tokio::test]
    async fn range_suffix() {
        let env = state_with_app(&[("index.html", b"x"), ("f.bin", &[7u8; 100])]);
        let req = Request::get("/apps/t/f.bin")
            .header(header::RANGE, "bytes=-10")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some("10")
        );
    }

    #[tokio::test]
    async fn invalid_range_returns_416() {
        let env = state_with_app(&[("index.html", b"x"), ("f.bin", &[0u8; 100])]);
        let req = Request::get("/apps/t/f.bin")
            .header(header::RANGE, "bytes=200-300")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some("bytes */100")
        );
    }

    #[tokio::test]
    async fn multi_range_downgrades_to_200() {
        let env = state_with_app(&[("index.html", b"x"), ("f.bin", &[0u8; 100])]);
        let req = Request::get("/apps/t/f.bin")
            .header(header::RANGE, "bytes=0-10,20-30")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_rejected() {
        let env = state_with_app(&[("index.html", b"x")]);
        let req = Request::post("/apps/t/index.html")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn head_returns_headers_only() {
        let env = state_with_app(&[("index.html", b"hello")]);
        let req = Request::head("/apps/t/index.html")
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(r.into_body(), 1024)
            .await
            .expect("body");
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn etag_and_last_modified_present() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t/index.html").await;
        assert!(r.headers().get(header::ETAG).is_some());
        assert!(r.headers().get(header::LAST_MODIFIED).is_some());
    }

    #[tokio::test]
    async fn etag_304() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t/index.html").await;
        let etag = r
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let etag = etag.expect("etag");
        let req = Request::get("/apps/t/index.html")
            .header(header::IF_NONE_MATCH, etag)
            .body(Body::empty())
            .expect("request");
        let r = build_router_for_test(&env)
            .oneshot(req)
            .await
            .expect("oneshot");
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn html_not_immutable() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/apps/t/index.html").await;
        let cc = r
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert!(
            cc.as_deref().is_some_and(|c| c.contains("no-cache")),
            "{cc:?}"
        );
    }

    #[tokio::test]
    async fn hashed_asset_immutable() {
        let env = state_with_app(&[("index.html", b"x"), ("assets/app-a1b2c3d4e5f6.js", b"y")]);
        let r = get(&env, "/apps/t/assets/app-a1b2c3d4e5f6.js").await;
        let cc = r
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        assert!(
            cc.as_deref().is_some_and(|c| c.contains("immutable")),
            "{cc:?}"
        );
    }

    #[tokio::test]
    async fn sdk_js_served() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/sdk.js").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("javascript")));
    }

    #[tokio::test]
    async fn sdk_dts_served() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/sdk.d.ts").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("typescript")));
    }

    #[tokio::test]
    async fn unknown_route_contract_404() {
        let env = state_with_app(&[("index.html", b"x")]);
        let r = get(&env, "/definitely/not/here").await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let text = body_string(r).await;
        assert!(text.contains("NOT_FOUND"), "{text}");
    }
}
