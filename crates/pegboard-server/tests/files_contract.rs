//! Files 契约测试：/api/files/*（API 契约 §4.2）。
//! 上传/下载/删除/列表/签名全链路；签名访问免 subject。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
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
    let host_db =
        pegboard_core::app::HostDb::open(&root.path().join("data/host.db")).expect("host db");
    let signer = Arc::new(pegboard_core::files::Signer::new(Arc::new(host_db)));
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
        proxy: Arc::new(proxy),
        config,
        apps: std::sync::RwLock::new(outcome.registry),
        auditor,
    });
    TestEnv { _root: root, state }
}

fn files_env() -> TestEnv {
    state_with(&[(
        "t",
        r#"{"id":"t","name":"T","entry":"index.html","permissions":{"files":true}}"#,
    )])
}

async fn call(env: &TestEnv, req: Request<Body>) -> axum::http::Response<Body> {
    build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot")
}

fn multipart_body(boundary: &str, filename: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    out.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

async fn upload(env: &TestEnv, filename: &str, data: &[u8]) -> axum::http::Response<Body> {
    let boundary = "pegboardtestboundary";
    let body = multipart_body(boundary, filename, "text/plain", data);
    let req = Request::post("/api/files")
        .header("x-pegboard-app", "t")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("request");
    call(env, req).await
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body")
        .to_vec()
}

#[tokio::test]
async fn upload_returns_meta() {
    let env = files_env();
    let r = upload(&env, "hello.txt", b"hello world").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["name"], "hello.txt");
    assert_eq!(v["size"], 11);
    assert_eq!(v["mime"], "text/plain");
    assert!(v["id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn download_roundtrip_with_range() {
    let env = files_env();
    let r = upload(&env, "data.bin", &vec![7u8; 1024]).await;
    let v = body_json(r).await;
    let id = v["id"].as_str().expect("id").to_owned();

    // 完整下载
    let req = Request::get(format!("/api/files/{id}"))
        .header("x-pegboard-app", "t")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers().get(header::CONTENT_DISPOSITION).is_some());
    assert!(r.headers().get(header::ETAG).is_some());
    let bytes = body_bytes(r).await;
    assert_eq!(bytes.len(), 1024);

    // Range
    let req = Request::get(format!("/api/files/{id}"))
        .header("x-pegboard-app", "t")
        .header(header::RANGE, "bytes=0-99")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        r.headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 0-99/1024")
    );
    assert_eq!(body_bytes(r).await.len(), 100);
}

#[tokio::test]
async fn download_missing_404() {
    let env = files_env();
    let req = Request::get("/api/files/NOPE")
        .header("x-pegboard-app", "t")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn delete_then_missing() {
    let env = files_env();
    let r = upload(&env, "x.txt", b"12345").await;
    let v = body_json(r).await;
    let id = v["id"].as_str().expect("id").to_owned();
    let req = Request::delete(format!("/api/files/{id}"))
        .header("x-pegboard-app", "t")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let req = Request::get(format!("/api/files/{id}"))
        .header("x-pegboard-app", "t")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_and_prefix() {
    let env = files_env();
    upload(&env, "img-1.png", b"1").await;
    upload(&env, "img-2.png", b"2").await;
    upload(&env, "doc.txt", b"3").await;
    let req = Request::get("/api/files?prefix=img-&limit=10")
        .header("x-pegboard-app", "t")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert!(v["next"].is_null());
}

#[tokio::test]
async fn sign_and_token_access() {
    let env = files_env();
    let r = upload(&env, "share.txt", b"shared content").await;
    let v = body_json(r).await;
    let id = v["id"].as_str().expect("id").to_owned();

    // 签发
    let req = Request::post(format!("/api/files/{id}/sign"))
        .header("x-pegboard-app", "t")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ttl":600}"#))
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let url = v["url"].as_str().expect("url").to_owned();
    assert!(url.starts_with(&format!("/api/files/{id}?token=")), "{url}");

    // 带 token 访问：无 X-Pegboard-App、无身份 → 200
    let req = Request::get(url).body(Body::empty()).expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::OK, "签名 URL 应免 subject 访问");
    let bytes = body_bytes(r).await;
    assert_eq!(bytes, b"shared content");

    // 伪造 token → 401 TOKEN_INVALID
    let req = Request::get(format!(
        "/api/files/{id}?token=forged0000000000000000000000000000000000000000000000000000000000"
    ))
    .body(Body::empty())
    .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "TOKEN_INVALID");
}

#[tokio::test]
async fn sign_ttl_over_max_rejected() {
    let env = files_env();
    let r = upload(&env, "x.txt", b"1").await;
    let v = body_json(r).await;
    let id = v["id"].as_str().expect("id").to_owned();
    let req = Request::post(format!("/api/files/{id}/sign"))
        .header("x-pegboard-app", "t")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"ttl":99999999}"#))
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn files_permission_denied_403() {
    let env = state_with(&[(
        "t",
        r#"{"id":"t","name":"T","entry":"index.html","permissions":{"files":false}}"#,
    )]);
    let r = upload(&env, "nope.txt", b"x").await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

#[tokio::test]
async fn upload_too_large_413() {
    let env = state_with(&[(
        "t",
        r#"{"id":"t","name":"T","entry":"index.html","permissions":{"files":true},"limits":{"file_bytes":64}}"#,
    )]);
    let r = upload(&env, "big.bin", &[0u8; 128]).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "LIMIT_EXCEEDED");
}

#[tokio::test]
async fn files_isolated_between_apps() {
    let env = state_with(&[
        (
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"files":true}}"#,
        ),
        (
            "b",
            r#"{"id":"b","name":"B","entry":"index.html","permissions":{"files":true}}"#,
        ),
    ]);
    let boundary = "pegboardtestboundary";
    let body = multipart_body(boundary, "secret.txt", "text/plain", b"only-a");
    let req = Request::post("/api/files")
        .header("x-pegboard-app", "a")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let id = v["id"].as_str().expect("id").to_owned();
    // app b 看不到 a 的文件
    let req = Request::get(format!("/api/files/{id}"))
        .header("x-pegboard-app", "b")
        .body(Body::empty())
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "应用间文件不可互访");
}

#[tokio::test]
async fn upload_requires_multipart_file_field() {
    let env = files_env();
    let req = Request::post("/api/files")
        .header("x-pegboard-app", "t")
        .header("content-type", "multipart/form-data; boundary=b")
        .body(Body::from("--b--\r\n"))
        .expect("request");
    let r = call(&env, req).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}
