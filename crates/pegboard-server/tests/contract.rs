//! 契约测试：对 server 发请求，断言响应形状与错误码（与 docs/API契约.md 同步）。
//! M2：静态托管、SDK 资源、404/400 错误体。M3：Store API。
//! 能力 API 契约随 M4-M6 扩充。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use pegboard_server::ingress::{build_router, AppState};
use tower::ServiceExt;

struct TestEnv {
    _root: tempfile::TempDir,
    state: Arc<AppState>,
}

/// 构造测试环境：apps 下若干应用（id → manifest 内容）。
fn test_env_with(manifests: &[(&str, &str)]) -> TestEnv {
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
        started: std::time::Instant::now(),
        admin_token: None,
        disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
        proxy: Arc::new(proxy),
        config,
        apps: std::sync::RwLock::new(outcome.registry),
        auditor,
    });
    TestEnv { _root: root, state }
}

fn test_env() -> TestEnv {
    test_env_with(&[(
        "t",
        r#"{"id":"t","name":"T","entry":"index.html","permissions":{"store":true}}"#,
    )])
}

fn router_of(env: &TestEnv) -> Router {
    build_router(Arc::clone(&env.state))
}

async fn get(env: &TestEnv, uri: &str) -> axum::http::Response<Body> {
    let req = Request::get(uri).body(Body::empty()).expect("request");
    router_of(env).oneshot(req).await.expect("oneshot")
}

/// 能力 API 请求：带 X-Pegboard-App 头。
async fn api(
    env: &TestEnv,
    method: Method,
    uri: &str,
    body: Option<String>,
) -> axum::http::Response<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-pegboard-app", "t");
    let req = match body {
        Some(text) => builder
            .header("content-type", "application/json")
            .body(Body::from(text))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    router_of(env).oneshot(req).await.expect("oneshot")
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json body")
}

async fn kv_set(env: &TestEnv, key: &str, value_json: &str) {
    let r = api(
        env,
        Method::PUT,
        &format!("/api/store/kv/{key}"),
        Some(format!(r#"{{"value":{value_json}}}"#)),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

// ---------- 静态与接入（M2 回归）----------

#[tokio::test]
async fn static_app_not_found_is_contract_404() {
    let env = test_env();
    let r = get(&env, "/apps/nope/index.html").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "APP_NOT_FOUND");
    assert!(v["error"]["message"].is_string());
}

#[tokio::test]
async fn unknown_api_route_is_contract_404() {
    let env = test_env();
    let r = get(&env, "/api/nonexistent").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn api_missing_app_header_rejected() {
    let env = test_env();
    let r = get(&env, "/api/store/kv/x").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn api_unknown_app_header_rejected() {
    let env = test_env();
    let req = Request::get("/api/store/kv/x")
        .header("x-pegboard-app", "nope")
        .body(Body::empty())
        .expect("request");
    let r = router_of(&env).oneshot(req).await.expect("oneshot");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "APP_NOT_FOUND");
}

#[tokio::test]
async fn invalid_percent_encoding_is_400() {
    let env = test_env();
    let r = get(&env, "/apps/t/%FF%FE.js").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn static_file_200_with_mime() {
    let env = test_env();
    let r = get(&env, "/apps/t/index.html").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html")));
}

#[tokio::test]
async fn sdk_js_returns_200() {
    let env = test_env();
    let r = get(&env, "/sdk.js").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("javascript")));
}

#[tokio::test]
async fn sdk_dts_returns_200() {
    let env = test_env();
    let r = get(&env, "/sdk.d.ts").await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn error_body_shape_matches_contract() {
    let env = test_env();
    let r = get(&env, "/apps/nope/x").await;
    let v = body_json(r).await;
    assert!(v["error"]["code"].is_string());
    assert!(v["error"]["message"].is_string());
}

#[tokio::test]
async fn html_response_injects_sdk_script() {
    let env = test_env();
    let r = get(&env, "/apps/t/").await;
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains(r#"<script src="/sdk.js"></script>"#),
        "{text}"
    );
}

// ---------- Store API（API 契约 §4.1）----------

#[tokio::test]
async fn store_kv_roundtrip() {
    let env = test_env();
    kv_set(&env, "greeting", r#"{"msg":"hi"}"#).await;
    let r = api(&env, Method::GET, "/api/store/kv/greeting", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["value"]["msg"], "hi");
}

#[tokio::test]
async fn store_kv_get_missing_returns_null() {
    let env = test_env();
    let r = api(&env, Method::GET, "/api/store/kv/absent", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert!(v["value"].is_null());
}

#[tokio::test]
async fn store_kv_delete_then_missing() {
    let env = test_env();
    kv_set(&env, "k", "1").await;
    let r = api(&env, Method::DELETE, "/api/store/kv/k", None).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = api(&env, Method::GET, "/api/store/kv/k", None).await;
    let v = body_json(r).await;
    assert!(v["value"].is_null());
}

#[tokio::test]
async fn store_kv_list_by_prefix() {
    let env = test_env();
    kv_set(&env, "u/a/1", "1").await;
    kv_set(&env, "u/a/2", "2").await;
    kv_set(&env, "u/b/1", "3").await;
    let r = api(&env, Method::GET, "/api/store/kv?prefix=u%2Fa%2F", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["key"], "u/a/1");
    assert_eq!(items[1]["key"], "u/a/2");
}

#[tokio::test]
async fn store_kv_batch_set_delete() {
    let env = test_env();
    let r = api(
        &env,
        Method::POST,
        "/api/store/kv/batch",
        Some(
            r#"{"ops":[
                {"op":"set","key":"b1","value":"x"},
                {"op":"set","key":"b2","value":[1,2]},
                {"op":"delete","key":"b3"}
            ]}"#
            .to_owned(),
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = api(&env, Method::GET, "/api/store/kv/b2", None).await;
    let v = body_json(r).await;
    assert_eq!(v["value"], serde_json::json!([1, 2]));
}

#[tokio::test]
async fn store_kv_batch_unknown_op_rejected() {
    let env = test_env();
    let r = api(
        &env,
        Method::POST,
        "/api/store/kv/batch",
        Some(r#"{"ops":[{"op":"frobnicate","key":"k"}]}"#.to_owned()),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn store_kv_encoded_slash_key() {
    let env = test_env();
    // key 含 / 时编码为 %2F，服务端不得解码为路径分隔符
    kv_set(&env, "ns%2Fa", "\"v1\"").await;
    let r = api(&env, Method::GET, "/api/store/kv/ns%2Fa", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["value"], "v1");
}

#[tokio::test]
async fn store_permission_denied_403() {
    let env = test_env_with(&[(
        "noperm",
        r#"{"id":"noperm","name":"N","entry":"index.html","permissions":{"store":false}}"#,
    )]);
    let req = Request::put("/api/store/kv/k")
        .header("x-pegboard-app", "noperm")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":1}"#))
        .expect("request");
    let r = build_router(env.state).oneshot(req).await.expect("oneshot");
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "PERMISSION_DENIED");
}

#[tokio::test]
async fn store_kv_value_too_large_413() {
    let env = test_env_with(&[(
        "t",
        r#"{"id":"t","name":"T","entry":"index.html","permissions":{"store":true},"limits":{"kv_value_bytes":64}}"#,
    )]);
    let big = "x".repeat(128);
    let r = api(
        &env,
        Method::PUT,
        "/api/store/kv/big",
        Some(format!(r#"{{"value":"{big}"}}"#)),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "LIMIT_EXCEEDED");
    assert_eq!(v["error"]["detail"]["limit"], "kv_value_bytes");
}

#[tokio::test]
async fn store_isolated_between_apps() {
    let env = test_env_with(&[
        (
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"store":true}}"#,
        ),
        (
            "b",
            r#"{"id":"b","name":"B","entry":"index.html","permissions":{"store":true}}"#,
        ),
    ]);
    let req = Request::put("/api/store/kv/secret")
        .header("x-pegboard-app", "a")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":"only-a"}"#))
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let req = Request::get("/api/store/kv/secret")
        .header("x-pegboard-app", "b")
        .body(Body::empty())
        .expect("request");
    let r = build_router(env.state).oneshot(req).await.expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert!(v["value"].is_null(), "app b 不应看到 app a 的数据");
}
