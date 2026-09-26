//! 场景测试：驱动真实页面/SDK 语义对真实服务器验证（项目骨架 §8）。
//! M3：SDK host.store；M4：host.fetch/shim/url 跨域拉取 + SSE 流式首字节。
//! node 桥接执行真实 sdk.js（浏览器场景在 M7 用 Playwright）。

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// 起真实服务器；返回 (child, base_url, root)。
fn start_server(extra_apps: &[(&str, &str)]) -> (std::process::Child, String, TempDir) {
    let root = TempDir::new().expect("tempdir");
    let apps_dir = root.path().join("apps");
    // 基础应用：声明 store
    write_app(&apps_dir, "kvtest", r#"{"store":true}"#);
    for (id, perms) in extra_apps {
        write_app(&apps_dir, id, perms);
    }
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = {:?}\napps_dir = {:?}\n",
            root.path().join("data"),
            apps_dir
        ),
    )
    .expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    let stdout = child.stdout.take().expect("stdout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut addr = None;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        if Instant::now() > deadline {
            panic!("server did not start in time");
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("server exited before listening"),
            Ok(_) => {
                if line.contains("listening") {
                    if let Some(pos) = line.find("addr=") {
                        addr = Some(line[pos + 5..].trim().to_owned());
                    }
                    break;
                }
            }
            Err(e) => panic!("read stdout: {e}"),
        }
    }
    let _ = child.stdout.take();
    let addr = addr.expect("addr");
    (child, format!("http://{addr}"), root)
}

fn write_app(apps: &std::path::Path, id: &str, perms: &str) {
    let dir = apps.join(id);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("index.html"), "<h1>x</h1>").expect("write");
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"id":"{id}","name":"{id}","entry":"index.html","permissions":{perms}}}"#),
    )
    .expect("write manifest");
}

fn stop_server(mut child: std::process::Child) {
    let pid = child.id();
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    let _ = child.wait();
}

fn sdk_path() -> String {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../pegboard-sdk/assets/sdk.js");
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn scenario_sdk_store_roundtrip() {
    let (child, base, _root) = start_server(&[]);
    let out = Command::new("node")
        .arg("tests/fixtures/sdk_store_scenario.js")
        .arg(&base)
        .arg(sdk_path())
        .output()
        .expect("run node scenario");
    stop_server(child);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "node scenario failed:\n{text}\nstatus: {:?}",
        out.status.code()
    );
    assert!(text.contains("SDK-SCENARIO-OK"), "{text}");
}

#[tokio::test]
async fn scenario_sdk_store_served_by_host() {
    // /sdk.js 与页面注入的 bootstrap 一致可用（SDK 由宿主分发）
    let (child, base, _root) = start_server(&[]);
    let sdk = reqwest::get(format!("{base}/sdk.js"))
        .await
        .expect("get sdk");
    assert_eq!(sdk.status(), 200);
    let page = reqwest::get(format!("{base}/apps/kvtest/"))
        .await
        .expect("get page");
    let html = page.text().await.expect("page body");
    assert!(html.contains(r#"<script src="/sdk.js"></script>"#));
    stop_server(child);
}

/// 起本地上游（node fixture），返回 (child, base_url)。
fn start_upstream() -> (Child, String) {
    let mut child = Command::new("node")
        .arg("tests/fixtures/upstream_fixture.js")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn upstream");
    let stdout = child.stdout.take().expect("stdout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let base = loop {
        if Instant::now() > deadline {
            panic!("upstream did not start");
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("upstream exited"),
            Ok(_) => {
                if let Some(rest) = line.strip_prefix("UPSTREAM-READY ") {
                    break rest.trim().to_owned();
                }
            }
            Err(e) => panic!("read upstream stdout: {e}"),
        }
    };
    let _ = child.stdout.take();
    (child, base)
}

fn stop_upstream(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[tokio::test]
async fn scenario_sdk_fetch_cross_origin() {
    let (up_child, upstream_base) = start_upstream();
    let perms = format!(r#"{{"net":[{:?}]}}"#, upstream_base);
    let (child, base, _root) = start_server(&[("nettest", &perms)]);
    let out = Command::new("node")
        .arg("tests/fixtures/sdk_fetch_scenario.js")
        .arg(&base)
        .arg(sdk_path())
        .arg(&upstream_base)
        .output()
        .expect("run node scenario");
    stop_server(child);
    stop_upstream(up_child);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "node fetch scenario failed:\n{text}\nstatus: {:?}",
        out.status.code()
    );
    assert!(text.contains("SDK-FETCH-SCENARIO-OK"), "{text}");
}

/// SSE 流式：代理首字节延迟与直连相当（不缓冲整流再返回）。
#[tokio::test]
async fn scenario_sse_first_byte_latency() {
    let (up_child, upstream_base) = start_upstream();
    let sse_url = format!("{upstream_base}/sse");
    let perms = format!(r#"{{"net":[{:?}]}}"#, upstream_base);
    let (child, base, _root) = start_server(&[("ssetest", &perms)]);

    // 直连基线：首字节时间 + 事件数
    let direct_start = Instant::now();
    let direct = reqwest::get(&sse_url).await.expect("direct sse");
    let direct_first = direct_start.elapsed();
    let direct_total = count_sse_events(direct).await;

    // 经代理：首字节时间 + 事件数
    let proxy_target = format!("{base}/api/proxy?url={}", urlencode(&sse_url));
    let client = reqwest::Client::new();
    let proxy_start = Instant::now();
    let proxied = client
        .get(&proxy_target)
        .header("x-pegboard-app", "ssetest")
        .send()
        .await
        .expect("proxied sse");
    assert_eq!(proxied.status(), 200);
    let proxy_first = proxy_start.elapsed();
    let proxy_total = count_sse_events(proxied).await;

    stop_server(child);
    stop_upstream(up_child);

    // 流完整性：事件条数一致
    assert_eq!(direct_total, proxy_total, "代理应完整透传所有事件");
    assert!(proxy_total >= 20, "事件数: {proxy_total}");
    // 首字节与直连相当：允许 500ms 开销；若先缓冲整流（~2s）则远超
    assert!(
        proxy_first < direct_first + Duration::from_millis(500),
        "代理首字节 {proxy_first:?} vs 直连 {direct_first:?}"
    );
    assert!(
        proxy_first.as_millis() < 1500,
        "代理首字节 {proxy_first:?} 表明发生了整流缓冲"
    );
}

/// 逐 chunk 计数 "data: " 行，验证流式接收。
async fn count_sse_events(response: reqwest::Response) -> usize {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("chunk");
        count += chunk.windows(6).filter(|w| w == b"data: ").count();
    }
    count
}

#[tokio::test]
async fn scenario_sdk_files_upload_download_sign() {
    let (child, base, _root) = start_server(&[("filestest", r#"{"files":true}"#)]);
    let out = Command::new("node")
        .arg("tests/fixtures/sdk_files_scenario.js")
        .arg(&base)
        .arg(sdk_path())
        .output()
        .expect("run node scenario");
    stop_server(child);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "node files scenario failed:\n{text}\nstatus: {:?}",
        out.status.code()
    );
    assert!(text.contains("SDK-FILES-SCENARIO-OK"), "{text}");
}
