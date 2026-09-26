//! 契约测试：对 server 发请求，断言响应形状与错误码（与 docs/API契约.md 同步）。
//! M2 范围：静态托管、SDK 资源、404/400 错误体。能力 API 契约随 M3-M6 扩充。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use pegboard_server::ingress::{build_router, AppState};
use tower::ServiceExt;

struct TestEnv {
    _root: tempfile::TempDir,
    state: Arc<AppState>,
}

fn test_env() -> TestEnv {
    let root = tempfile::TempDir::new().expect("tempdir");
    let apps = root.path().join("apps");
    let app_dir = apps.join("t");
    std::fs::create_dir_all(&app_dir).expect("mkdir");
    std::fs::write(app_dir.join("index.html"), "<h1>hi</h1>").expect("write");
    std::fs::write(
        app_dir.join("manifest.json"),
        r#"{"id":"t","name":"T","entry":"index.html"}"#,
    )
    .expect("write manifest");
    let mut config = pegboard_core::config::Config::default();
    config.storage.apps_dir = apps.clone();
    config.storage.data_root = root.path().join("data");
    let outcome = pegboard_core::app::AppRegistry::scan(&apps, &config.limits).expect("scan");
    let auditor = pegboard_core::audit::Auditor::start(pegboard_core::audit::AuditConfig {
        log_dir: root.path().join("data/logs"),
        ..Default::default()
    })
    .expect("auditor")
    .handle();
    TestEnv {
        _root: root,
        state: Arc::new(AppState {
            config,
            apps: std::sync::RwLock::new(outcome.registry),
            auditor,
        }),
    }
}

async fn get(env: &TestEnv, uri: &str) -> axum::http::Response<Body> {
    let req = Request::get(uri).body(Body::empty()).expect("request");
    build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot")
}

async fn error_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("error json")
}

#[tokio::test]
async fn static_app_not_found_is_contract_404() {
    let env = test_env();
    let r = get(&env, "/apps/nope/index.html").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = error_json(r).await;
    assert_eq!(v["error"]["code"], "APP_NOT_FOUND");
    assert!(v["error"]["message"].is_string());
}

#[tokio::test]
async fn unknown_api_route_is_contract_404() {
    let env = test_env();
    let r = get(&env, "/api/store/kv/x").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = error_json(r).await;
    assert_eq!(v["error"]["code"], "NOT_FOUND");
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
    let v = error_json(r).await;
    // {"error":{"code":STRING,"message":STRING,"detail":...}}
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
