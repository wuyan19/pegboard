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
    state_with_opts(manifests, None)
}

fn state_with_opts(manifests: &[(&str, &str)], admin_token: Option<&str>) -> TestEnv {
    state_full(
        manifests,
        admin_token,
        Arc::new(pegboard_server::host::NoControl),
    )
}

fn state_full(
    manifests: &[(&str, &str)],
    admin_token: Option<&str>,
    control: Arc<dyn pegboard_server::host::ProcessControl>,
) -> TestEnv {
    let root = tempfile::TempDir::new().expect("tempdir");
    let apps = root.path().join("apps");
    std::fs::create_dir_all(&apps).expect("mkdir apps");
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
    // 模拟 runtime 首启 seed：注册表写入 host.db 行（真实启动流程见 cli runtime）
    for meta in outcome.registry.list() {
        host_db
            .upsert_app(
                &meta.id,
                &meta.name,
                &meta.root.display().to_string(),
                &meta.manifest_json(),
            )
            .expect("seed host.db");
    }
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
        admin_token: admin_token.map(str::to_owned),
        disabled: std::sync::RwLock::new(std::collections::HashSet::new()),
        proxy: Arc::new(proxy),
        config,
        apps: std::sync::RwLock::new(outcome.registry),
        auditor: handle,
        control,
        update: Arc::new(pegboard_server::host::update::Updater::new(
            "",
            std::env::temp_dir(),
        )),
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

/// 记录重启调用的控制实现（共享标志供断言）。
struct RecordingControl {
    called: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl pegboard_server::host::ProcessControl for RecordingControl {
    fn request_restart(&self) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        self.called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn host_restart_invokes_process_control() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let env = state_full(
        &[],
        None,
        Arc::new(RecordingControl {
            called: Arc::clone(&called),
        }),
    );
    let r = call(&env, Method::POST, "/api/admin/host/restart").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["ok"], true);
    assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    // 重启动作应留下 Admin 审计事件
    let r = call(&env, Method::GET, "/api/admin/logs?action=Admin").await;
    let v = body_json(r).await;
    let items = v["items"].as_array().expect("items");
    assert!(
        items.iter().any(|e| e["target"] == "host"),
        "restart 应产生 Admin 审计事件: {items:?}"
    );
}

#[tokio::test]
async fn host_restart_unsupported_maps_to_error() {
    let env = env();
    let r = call(&env, Method::POST, "/api/admin/host/restart").await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "UPSTREAM_ERROR");
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
async fn uninstall_keeps_data_and_reinstall_restores() {
    let env = env();
    // 写一条 KV 建库
    let req = Request::put("/api/store/kv/k")
        .header("x-pegboard-app", "a")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"value":"kept"}"#))
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // 卸载：注销 + 静态 404；数据目录与产物保留
    let r = call(&env, Method::POST, "/api/admin/apps/a/uninstall").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    // 卸载后磁盘候选仍有效：详情返回 not_installed 视图
    let r = call(&env, Method::GET, "/api/admin/apps/a").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["state"], "not_installed");
    assert!(
        env._root.path().join("data/apps_data/a").exists(),
        "卸载保留数据目录"
    );
    assert!(
        env._root.path().join("apps/a/index.html").exists(),
        "卸载保留产物"
    );
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    let v = body_json(r).await;
    let a = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "a")
        .expect("a in list");
    assert_eq!(a["state"], "not_installed");

    // 重装：数据恢复
    let r = call(&env, Method::POST, "/api/admin/apps/a/install").await;
    assert_eq!(r.status(), StatusCode::OK);
    let req = Request::get("/api/store/kv/k")
        .header("x-pegboard-app", "a")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["value"], "kept", "重装后 KV 数据应恢复");
}

#[tokio::test]
async fn delete_removes_everything() {
    let env = env();
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

    let r = call(&env, Method::DELETE, "/api/admin/apps/a").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(
        !env._root.path().join("data/apps_data/a").exists(),
        "删除移除数据目录"
    );
    assert!(
        !env._root.path().join("apps/a").exists(),
        "删除移除产物目录"
    );
    let r = call(&env, Method::GET, "/api/admin/apps/a").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // 删除后列表中不再出现
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    let v = body_json(r).await;
    assert!(v.as_array().unwrap().iter().all(|x| x["id"] != "a"));
}

#[tokio::test]
async fn list_shows_all_states() {
    let env = env();
    // 未安装候选：装好清单的目录，未注册
    let dir = env._root.path().join("apps/cand");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("index.html"), "x").expect("write");
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"id":"cand","name":"Cand","entry":"index.html"}"#,
    )
    .expect("write");
    // 无效候选：坏清单
    let bad = env._root.path().join("apps/broken");
    std::fs::create_dir_all(&bad).expect("mkdir");
    std::fs::write(bad.join("manifest.json"), "{ nope").expect("write");

    let r = call(&env, Method::GET, "/api/admin/apps").await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    let items = v.as_array().expect("array");
    let by_id = |id: &str| items.iter().find(|x| x["id"] == id).cloned();
    assert_eq!(by_id("a").expect("a")["state"], "enabled");
    assert_eq!(by_id("cand").expect("cand")["state"], "not_installed");
    assert_eq!(by_id("cand").unwrap()["name"], "Cand");
    let broken = by_id("broken").expect("broken");
    assert_eq!(broken["state"], "invalid");
    assert!(broken["error"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn uninstall_missing_row_only_app() {
    // 已安装行存在但产物目录被外部删除 → 缺失态可用卸载清理
    let env = env();
    // 先禁用（写行），再移除产物目录模拟外部删除
    call(&env, Method::POST, "/api/admin/apps/a/disable").await;
    std::fs::remove_dir_all(env._root.path().join("apps/a")).expect("rm");
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    let v = body_json(r).await;
    let a = v
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "a")
        .expect("a");
    assert_eq!(a["state"], "missing", "{}", a);
    // 卸载清理残留行
    let r = call(&env, Method::POST, "/api/admin/apps/a/uninstall").await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    let v = body_json(r).await;
    assert!(v.as_array().unwrap().iter().all(|x| x["id"] != "a"));
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
            uri.push_str(&format!("&before={c}"));
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
    let r = call(&env, Method::GET, "/api/admin/logs?before=garbage").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
    let r = call(&env, Method::GET, "/api/admin/logs?after=garbage").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = call(&env, Method::GET, "/api/admin/logs?before=1:1&after=1:1").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logs_bidirectional_paging() {
    let env = env();
    let handle = env.state.auditor.clone();
    for i in 0..6i64 {
        handle
            .record(pegboard_core::audit::AuditEvent {
                ts: 20_000 + i,
                seq: 0,
                app_id: Some("bidir".into()),
                subject: None,
                action: pegboard_core::audit::ActionKind::Net,
                target: Some(format!("t{i}")),
                outcome: pegboard_core::audit::Outcome::Ok,
                error_code: None,
                duration_ms: 1,
            })
            .expect("record");
    }
    // 等待落盘
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let r = call(&env, Method::GET, "/api/admin/logs?app=bidir&limit=2").await;
        let v = body_json(r).await;
        if v["items"].as_array().is_some_and(|a| a.len() == 2) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "审计未落盘");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // 第一页（最新 2 条）：有 next 无 prev
    let r = call(&env, Method::GET, "/api/admin/logs?app=bidir&limit=2").await;
    let v = body_json(r).await;
    let next = v["next"].as_str().expect("next").to_owned();
    assert!(v["prev"].is_null());
    assert_eq!(v["items"][0]["target"], "t5");

    // before 翻到第二页：有 next 有 prev
    let r = call(
        &env,
        Method::GET,
        &format!("/api/admin/logs?app=bidir&limit=2&before={next}"),
    )
    .await;
    let v = body_json(r).await;
    assert_eq!(v["items"][0]["target"], "t3");
    let prev = v["prev"].as_str().expect("prev").to_owned();

    // after 翻回第一页：内容与第一页一致
    let r = call(
        &env,
        Method::GET,
        &format!("/api/admin/logs?app=bidir&limit=2&after={prev}"),
    )
    .await;
    let v = body_json(r).await;
    assert_eq!(v["items"][0]["target"], "t5");
    assert!(v["prev"].is_null());
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

// ---------- 批次三：管理鉴权 + zip 包安装 ----------

fn env_with_token() -> TestEnv {
    state_with_opts(
        &[(
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"store":true}}"#,
        )],
        Some("test-admin-token-123456"),
    )
}

#[tokio::test]
async fn admin_auth_required_when_token_set() {
    let env = env_with_token();
    // 无凭证 → 401 TOKEN_INVALID
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "TOKEN_INVALID");
    // 错误凭证 → 401
    let req = Request::get("/api/admin/apps")
        .header("authorization", "Bearer wrong-token-wrong-token")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // 正确凭证 → 200
    let req = Request::get("/api/admin/apps")
        .header("authorization", "Bearer test-admin-token-123456")
        .body(Body::empty())
        .expect("request");
    let r = build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot");
    assert_eq!(r.status(), StatusCode::OK);
    // 管理页外壳不受鉴权保护（无敏感数据）
    let r = call(&env, Method::GET, "/admin").await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_auth_off_when_no_token() {
    let env = env();
    let r = call(&env, Method::GET, "/api/admin/apps").await;
    assert_eq!(r.status(), StatusCode::OK);
}

/// 构造 zip 字节（含给定条目）。
fn build_zip(entries: Vec<(&str, &[u8])>) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, data) in entries {
            zip.start_file(name, opts).expect("start_file");
            zip.write_all(data).expect("write");
        }
        zip.finish().expect("finish");
    }
    buf.into_inner()
}

async fn upload_package(env: &TestEnv, bytes: Vec<u8>) -> axum::http::Response<Body> {
    let boundary = "pkgboundary";
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"app.zip\"\r\nContent-Type: application/zip\r\n\r\n")
        .into_bytes();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let req = Request::post("/api/admin/apps/package")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .expect("request");
    build_router(Arc::clone(&env.state))
        .oneshot(req)
        .await
        .expect("oneshot")
}

#[tokio::test]
async fn package_install_roundtrip() {
    let env = env();
    let manifest = br#"{"id":"packaged","name":"Packaged","entry":"index.html"}"#;
    let zip = build_zip(vec![
        ("manifest.json", manifest),
        ("index.html", b"<h1>packaged</h1>"),
        ("assets/app.js", b"console.log(1)"),
    ]);
    let r = upload_package(&env, zip).await;
    assert_eq!(r.status(), StatusCode::OK, "包安装应成功");
    let v = body_json(r).await;
    assert_eq!(v["id"], "packaged");
    assert_eq!(v["state"], "enabled");
    // 产物落位 + 静态可访问
    assert!(env._root.path().join("apps/packaged/index.html").exists());
    assert!(env
        ._root
        .path()
        .join("apps/packaged/assets/app.js")
        .exists());
    let r = call(&env, Method::GET, "/apps/packaged/").await;
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn package_zip_slip_rejected() {
    let env = env();
    let manifest = br#"{"id":"slip","name":"Slip","entry":"index.html"}"#;
    let zip = build_zip(vec![("manifest.json", manifest), ("../evil.txt", b"pwned")]);
    let r = upload_package(&env, zip).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // 未写出应用目录之外
    assert!(!env._root.path().join("apps/evil.txt").exists());
    assert!(!env._root.path().join("evil.txt").exists());
    assert!(!env._root.path().join("apps/slip").exists());
}

#[tokio::test]
async fn package_id_conflict_rejected() {
    let env = env();
    let manifest = br#"{"id":"a","name":"Dup","entry":"index.html"}"#;
    let zip = build_zip(vec![("manifest.json", manifest), ("index.html", b"x")]);
    let r = upload_package(&env, zip).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn package_missing_manifest_400() {
    let env = env();
    let zip = build_zip(vec![("index.html", b"x")]);
    let r = upload_package(&env, zip).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "INVALID_REQUEST");
}
