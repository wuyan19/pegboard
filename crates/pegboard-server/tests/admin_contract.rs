//! Admin 契约测试：/api/admin/* 与 /admin 页面（API 契约 §8）。

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
    .expect("auditor");
    // 造两条审计事件并落盘
    let handle = auditor.handle();
    handle
        .record(pegboard_core::audit::AuditEvent {
            ts: 1_000,
            seq: 0,
            app_id: Some("a".into()),
            subject: Some("u".into()),
            action: pegboard_core::audit::ActionKind::Net,
            target: Some("api.example.com".into()),
            outcome: pegboard_core::audit::Outcome::Ok,
            error_code: None,
            duration_ms: 5,
        })
        .expect("record");
    handle
        .record(pegboard_core::audit::AuditEvent {
            ts: 2_000,
            seq: 0,
            app_id: Some("b".into()),
            subject: None,
            action: pegboard_core::audit::ActionKind::Store,
            target: Some("k".into()),
            outcome: pegboard_core::audit::Outcome::Denied,
            error_code: Some("PERMISSION_DENIED".into()),
            duration_ms: 0,
        })
        .expect("record");
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
        disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
        proxy: Arc::new(proxy),
        config,
        apps: std::sync::RwLock::new(outcome.registry),
        auditor: handle,
    });
    TestEnv { _root: root, state }
}

fn env() -> TestEnv {
    state_with(&[
        (
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"store":true}}"#,
        ),
        (
            "b",
            r#"{"id":"b","name":"B","entry":"index.html","permissions":{"net":["http://127.0.0.1:1"]}}"#,
        ),
    ])
}

async fn call(env: &TestEnv, method: Method, uri: &str) -> axum::http::Response<Body> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot")
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn list_apps_returns_all_sorted() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let items = v.as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "a");
    assert_eq!(items[1]["id"], "b");
    assert_eq!(items[0]["enabled"], true);
    assert!(items[0]["permissions"]["store"].is_boolean());
    assert!(items[0]["limits"]["kv_value_bytes"].is_u64());
}

#[tokio::test]
async fn get_app_returns_view() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/apps/b").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["id"], "b");
    assert_eq!(v["permissions"]["net"][0], "http://127.0.0.1:1");
}

#[tokio::test]
async fn get_unknown_app_404() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/apps/nope").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "APP_NOT_FOUND");
}

#[tokio::test]
async fn disable_then_capability_denied_static_kept() {
    let env = env();
    let r = call(&env, Method::POST, "/api/admin/apps/a/disable").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["enabled"], false);

    // 能力 API → APP_NOT_FOUND
    let req = Request::get("/api/store/kv/x")
        .header("x-pegboard-app", "a")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // 静态仍可访问（产物公开语义）
    let r = call(&env, Method::GET, "/apps/a/index.html").await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn enable_restores_capability() {
    let env = env();
    call(&env, Method::POST, "/api/admin/apps/a/disable").await;
    let r = call(&env, Method::POST, "/api/admin/apps/a/enable").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["enabled"], true);
    let req = Request::get("/api/store/kv/x")
        .header("x-pegboard-app", "a")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK); // store 权限在，未命中返回 200 null
}

#[tokio::test]
async fn disable_is_idempotent() {
    let env = env();
    for _ in 0..2 {
        let r = call(&env, Method::POST, "/api/admin/apps/a/disable").await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn uninstall_removes_app_and_data() {
    let env = env();
    // 先写一条 KV 建库
    let req = Request::put("/api/store/kv/k")
        .header("x-pegboard-app", "a")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":1}"#))
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    let r = call(&env, Method::POST, "/api/admin/apps/a/uninstall").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    // 能力 → 404
    let r = call(&env, Method::GET, "/api/admin/apps/a").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // 数据目录删除、产物保留（磁盘文件在，但应用已注销 → 不再服务）
    assert!(!env._root.path().join("data/apps_data/a").exists());
    assert!(env._root.path().join("apps/a/index.html").exists());
    let r = call(&env, Method::GET, "/apps/a/index.html").await;
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "注销后不再服务（产物留在磁盘由操作者处理）"
    );
}

#[tokio::test]
async fn install_registers_app() {
    // 手工放置一个未注册应用目录（注册表不含 c）
    let env = env();
    let dir = env._root.path().join("apps/c");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("index.html"), "c").expect("write");
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"id":"c","name":"C","entry":"index.html","permissions":{"store":true}}"#,
    )
    .expect("write");

    let r = call(&env, Method::POST, "/api/admin/apps/c/install").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["id"], "c");
    // 能力恢复可用
    let req = Request::get("/api/store/kv/x")
        .header("x-pegboard-app", "c")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn install_invalid_manifest_400() {
    let env = env();
    let dir = env._root.path().join("apps/bad");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("manifest.json"), "{ nope").expect("write");
    let r = call(&env, Method::POST, "/api/admin/apps/bad/install").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logs_returned_desc_with_filters() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/logs").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert!(items[0]["ts"].as_i64() >= items[1]["ts"].as_i64(), "倒序");
    assert_eq!(items[0]["app_id"], "b");

    // 过滤
    let r = call(&env, Method::GET, "/api/admin/logs?app=a").await;
    let v = body_json(r).await;
    assert_eq!(v["items"].as_array().expect("items").len(), 1);

    let r = call(&env, Method::GET, "/api/admin/logs?outcome=Denied").await;
    let v = body_json(r).await;
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["error_code"], "PERMISSION_DENIED");

    let r = call(&env, Method::GET, "/api/admin/logs?action=Net").await;
    let v = body_json(r).await;
    assert_eq!(v["items"].as_array().expect("items").len(), 1);

    let r = call(&env, Method::GET, "/api/admin/logs?action=Nope").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn app_detail_includes_usage() {
    // 写一条 KV + 上传一个文件，详情 usage 应反映实际用量
    let env = state_with(&[(
        "a",
        r#"{"id":"a","name":"A","entry":"index.html","permissions":{"store":true,"files":true}}"#,
    )]);
    let req = Request::put("/api/store/kv/k")
        .header("x-pegboard-app", "a")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":"x"}"#))
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    let boundary = "pegboardtestboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\n12345\r\n--{boundary}--\r\n"
    );
    let req = Request::post("/api/files")
        .header("x-pegboard-app", "a")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK, "文件上传应成功");

    let r = call(&env, Method::GET, "/api/admin/apps/a").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["usage"]["kv"]["keys"], 1);
    assert_eq!(v["usage"]["kv"]["bytes"], 3); // JSON 字符串 "x" 含引号 3 字节
    assert_eq!(v["usage"]["files"]["count"], 1);
    assert_eq!(v["usage"]["files"]["bytes"], 5);
}

#[tokio::test]
async fn app_detail_usage_zero_when_unused() {
    // 未使用过的应用没有落库文件，usage 为 0 且不产生副作用
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/apps/b").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["usage"]["kv"]["keys"], 0);
    assert_eq!(v["usage"]["files"]["count"], 0);
    assert!(
        !env._root.path().join("data/apps_data/b").exists(),
        "查询用量不应创建应用数据目录"
    );
}

#[tokio::test]
async fn status_endpoint() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/status").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert!(v["version"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(v["mode"], "local");
    assert_eq!(v["identity"], "fixed");
    assert!(v["listen"].as_str().is_some_and(|s| s.contains(':')));
    assert!(v["uptime_secs"].is_u64());
    assert_eq!(v["audit_dropped"], 0);
}

#[tokio::test]
async fn logs_cursor_pagination_no_overlap() {
    let env = env();
    // 造 6 条同 app 事件（ts 各异），limit=2 逐页取
    let handle = env.state.auditor.clone();
    for i in 0..6i64 {
        handle
            .record(pegboard_core::audit::AuditEvent {
                ts: 10_000 + i,
                seq: 0,
                app_id: Some("paging".into()),
                subject: None,
                action: pegboard_core::audit::ActionKind::Net,
                target: Some(format!("t{i}")),
                outcome: pegboard_core::audit::Outcome::Ok,
                error_code: None,
                duration_ms: 1,
            })
            .expect("record");
    }
    // 审计写入是后台线程异步落盘：轮询首页直到 2 条可见（2s 上限）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let r = call(&env, Method::GET, "/api/admin/logs?app=paging&limit=2").await;
        let v = body_json(r).await;
        if v["items"].as_array().is_some_and(|a| a.len() == 2) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "审计事件 2s 内未全部落盘"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut cursor: Option<String> = None;
    let mut seen: Vec<(i64, Option<String>)> = Vec::new();
    for _ in 0..5 {
        let mut uri = "/api/admin/logs?app=paging&limit=2".to_owned();
        if let Some(c) = &cursor {
            uri.push_str(&format!("&cursor={c}"));
        }
        let r = call(&env, Method::GET, &uri).await;
        assert_eq!(r.status(), StatusCode::OK);
        let v = body_json(r).await;
        let items = v["items"].as_array().expect("items").to_vec();
        for it in &items {
            seen.push((
                it["ts"].as_i64().expect("ts"),
                it["target"].as_str().map(str::to_owned),
            ));
        }
        match v["next"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    // 6 条全部取到、无重复、倒序
    assert_eq!(seen.len(), 6, "{seen:?}");
    let ts_list: Vec<i64> = seen.iter().map(|(ts, _)| *ts).collect();
    let mut sorted = ts_list.clone();
    sorted.sort();
    sorted.reverse();
    assert_eq!(ts_list, sorted, "应按时间倒序");
}

#[tokio::test]
async fn logs_invalid_cursor_400() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/logs?cursor=garbage").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn admin_page_served() {
    let env = env();
    let r = call(&env, Method::GET, "/admin").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html")));
    let bytes = axum::body::to_bytes(r.into_body(), 64 * 1024)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("Pegboard"), "{text}");
}

#[tokio::test]
async fn admin_spa_fallback_and_asset_404() {
    let env = env();
    let r = call(&env, Method::GET, "/admin/some/view").await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = call(&env, Method::GET, "/admin/missing.js").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
