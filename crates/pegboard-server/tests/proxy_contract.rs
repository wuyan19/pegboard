//! Proxy 契约测试：/api/proxy 与 /api/asset（API 契约 §5）。
//! 真实转发经 raw-HTTP mock 上游；错误码断言对齐契约 §6。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use pegboard_server::ingress::{build_router, AppState};
use tower::ServiceExt;

struct TestEnv {
    _root: tempfile::TempDir,
    state: Arc<AppState>,
}

fn state_with(manifests: &[(&str, &str)]) -> TestEnv {
    let root = tempfile::TempDir::new().expect("tempdir");
    let apps = root.path().join("apps");
    for (id, manifest) in manifests {
        let dir = apps.join(id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("index.html"), "<h1>hi</h1>").expect("write");
        std::fs::write(dir.join("manifest.json"), manifest).expect("write manifest");
    }
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
        stores: pegboard_core::store::StoreManager::new(root.path().join("data/apps_data"), limits),
        files: pegboard_core::files::FilesManager::new(
            root.path().join("data/apps_data"),
            root.path().join("data/tmp"),
        ),
        signer: Arc::clone(&signer),
        host_db: Arc::clone(&host_db),
        disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
        proxy: Arc::new(proxy),
        config,
        apps: std::sync::RwLock::new(outcome.registry),
        auditor,
    });
    TestEnv { _root: root, state }
}

async fn proxy_call(
    env: &TestEnv,
    method: Method,
    uri: &str,
    body: Option<String>,
    headers: &[(&str, &str)],
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = match body {
        Some(text) => builder
            .header("content-type", "text/plain")
            .body(Body::from(text))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot")
}

async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body")
        .to_vec()
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = body_bytes(response).await;
    serde_json::from_slice(&bytes).expect("json")
}

/// raw-HTTP mock 上游：单连接，读请求后按 path 响应。
async fn spawn_upstream() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let seen = Arc::clone(&seen2);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let Ok(n) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                seen.lock().expect("lock").push(head.clone());
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .map(str::to_owned)
                    .unwrap_or_default();
                let cookie_sent = head.to_ascii_lowercase().contains("cookie:");
                let auth_sent = head.to_ascii_lowercase().contains("authorization:");
                let (status, body) = match path.as_str() {
                    "/json" => ("200 OK", r#"{"upstream":true}"#.to_owned()),
                    "/sse" => ("200 OK", "data: e1\n\ndata: e2\n\n".to_owned()),
                    "/redirect" => ("302 Found", String::new()),
                    _ => ("200 OK", "ok".to_owned()),
                };
                let mut response = format!("HTTP/1.1 {status}\r\n");
                if path == "/redirect" {
                    response.push_str("location: /json\r\n");
                }
                if path == "/sse" {
                    response.push_str("content-type: text/event-stream\r\n");
                    response.push_str(&format!("content-length: {}\r\n", body.len()));
                } else {
                    response.push_str("content-type: application/json\r\n");
                    response.push_str("set-cookie: leaked=1\r\n");
                    response.push_str(&format!("content-length: {}\r\n", body.len()));
                }
                if cookie_sent {
                    response.push_str("x-saw-cookie: yes\r\n");
                }
                if auth_sent {
                    response.push_str("x-saw-auth: yes\r\n");
                }
                response.push_str("\r\n");
                response.push_str(&body);
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (addr, seen)
}

fn manifest_with_net(id: &str, net: &str) -> String {
    format!(r#"{{"id":"{id}","name":"{id}","entry":"index.html","permissions":{{"net":[{net}]}}}}"#)
}

#[tokio::test]
async fn proxy_forwards_to_whitelisted_target() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["upstream"], true);
}

#[tokio::test]
async fn proxy_sanitizes_response_headers() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert!(
        r.headers().get("set-cookie").is_none(),
        "Set-Cookie 必须被清理"
    );
    assert_eq!(
        r.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}

#[tokio::test]
async fn proxy_strips_outbound_cookie_keeps_authorization() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r = proxy_call(
        &env,
        Method::GET,
        &uri,
        None,
        &[
            ("x-pegboard-app", "t"),
            ("cookie", "session=leak"),
            ("authorization", "Bearer app-key"),
            ("x-forwarded-for", "1.2.3.4"),
        ],
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    // mock 在看到 cookie/auth 时回显标记头
    assert!(r.headers().get("x-saw-cookie").is_none(), "cookie 不应转发");
    assert!(
        r.headers().get("x-saw-auth").is_some(),
        "authorization 应透传"
    );
}

#[tokio::test]
async fn proxy_target_denied_403() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[("t", &manifest_with_net("t", r#""http://127.0.0.1:1""#))]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "TARGET_DENIED");
}

#[tokio::test]
async fn proxy_permission_denied_403() {
    let env = state_with(&[("t", r#"{"id":"t","name":"T","entry":"index.html"}"#)]);
    let uri = format!("/api/proxy?url={}", urlencode("http://127.0.0.1:1/x"));
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

#[tokio::test]
async fn proxy_invalid_url_400() {
    let env = state_with(&[("t", &manifest_with_net("t", r#""http://127.0.0.1:1""#))]);
    let r = proxy_call(
        &env,
        Method::GET,
        "/api/proxy?url=notaurl",
        None,
        &[("x-pegboard-app", "t")],
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn proxy_missing_url_400() {
    let env = state_with(&[("t", &manifest_with_net("t", r#""http://127.0.0.1:1""#))]);
    let r = proxy_call(
        &env,
        Method::GET,
        "/api/proxy",
        None,
        &[("x-pegboard-app", "t")],
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn proxy_follows_redirect_within_whitelist() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/redirect"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["upstream"], true);
}

#[tokio::test]
async fn proxy_forwards_post_body() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/echo"))
    );
    let r = proxy_call(
        &env,
        Method::POST,
        &uri,
        Some("payload-123".into()),
        &[("x-pegboard-app", "t")],
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let seen = _seen.lock().expect("lock");
    let last = seen.last().expect("captured");
    assert!(last.contains("POST /echo"), "{last}");
}

#[tokio::test]
async fn asset_serves_resource() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/asset?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = body_bytes(r).await;
    assert!(String::from_utf8_lossy(&bytes).contains("upstream"));
}

#[tokio::test]
async fn sse_content_type_passthrough() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &manifest_with_net("t", &format!(r#""http://{upstream}""#)),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/sse"))
    );
    let r = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        r.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream")),
        "SSE content-type 应透传"
    );
    let bytes = body_bytes(r).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("data: e1") && text.contains("data: e2"),
        "{text}"
    );
}

#[tokio::test]
async fn proxy_rate_limited_429() {
    let (upstream, _seen) = spawn_upstream().await;
    let env = state_with(&[(
        "t",
        &format!(
            r#"{{"id":"t","name":"T","entry":"index.html","permissions":{{"net":["http://{upstream}"]}},"limits":{{"net_rps":2}}}}"#
        ),
    )]);
    let uri = format!(
        "/api/proxy?url={}",
        urlencode(&format!("http://{upstream}/json"))
    );
    let r1 = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r1.status(), StatusCode::OK);
    let r2 = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r2.status(), StatusCode::OK);
    let r3 = proxy_call(&env, Method::GET, &uri, None, &[("x-pegboard-app", "t")]).await;
    assert_eq!(r3.status(), StatusCode::TOO_MANY_REQUESTS);
    let v = body_json(r3).await;
    assert_eq!(v["error"]["code"], "LIMIT_EXCEEDED");
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
