//! 场景测试：驱动真实页面/SDK 语义对真实服务器验证（项目骨架 §8）。
//! M3：SDK host.store 读写 KV —— node 桥接执行真实 sdk.js（浏览器场景在 M7 用 Playwright）。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
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
