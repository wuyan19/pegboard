//! M2 集成测试：CLI 启动服务 + 静态端到端 + 优雅关闭（审计落盘）。
//! 驱动真实 pegboard 二进制，端口由 OS 分配，从 stdout 解析实际监听地址。

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Server {
    child: std::process::Child,
    _root: TempDir,
    addr: String,
}

fn write_app(apps: &std::path::Path, id: &str) {
    let dir = apps.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>served</h1>").unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"id":"{id}","name":"{id}","entry":"index.html"}}"#),
    )
    .unwrap();
}

fn start_server() -> Server {
    let root = TempDir::new().unwrap();
    let apps_dir = root.path().join("apps");
    write_app(&apps_dir, "t");
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = {:?}\napps_dir = {:?}\n",
            root.path().join("data"),
            apps_dir
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");

    // 等待 listening 行并解析地址（10s 上限）
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
                    // 形如 ... INFO pegboard::runtime: pegboard listening addr=127.0.0.1:53712
                    if let Some(pos) = line.find("addr=") {
                        addr = Some(line[pos + 5..].trim().to_owned());
                    }
                    break;
                }
            }
            Err(e) => panic!("read stdout failed: {e}"),
        }
    }
    // stdout 被 take 后 child.stdout 为 None；后续 kill 不需要它
    let _ = child.stdout.take();
    Server {
        child,
        _root: root,
        addr: addr.unwrap_or_default(),
    }
}

fn sigterm(child: &mut std::process::Child) {
    let pid = child.id();
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM");
    assert!(status.success());
}

#[tokio::test]
async fn serve_static_and_graceful_shutdown() {
    let mut server = start_server();
    assert!(!server.addr.is_empty(), "addr parsed from log");

    let url = format!("http://{}/apps/t/index.html", server.addr);
    let response = reqwest::get(&url).await.expect("http get");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.contains("served"), "{body}");
    // HTML 注入 SDK
    assert!(
        body.contains(r#"<script src="/sdk.js"></script>"#),
        "{body}"
    );

    let sdk = reqwest::get(format!("http://{}/sdk.js", server.addr))
        .await
        .expect("sdk get");
    assert_eq!(sdk.status(), 200);

    // 优雅关闭：SIGTERM → 退出码 0
    sigterm(&mut server.child);
    let output = server.child.wait().expect("wait child");
    assert!(output.success(), "exit code: {output:?}");
}

#[tokio::test]
async fn bad_app_alongside_good_still_serves() {
    let root = TempDir::new().unwrap();
    let apps_dir = root.path().join("apps");
    write_app(&apps_dir, "good");
    let bad = apps_dir.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("manifest.json"), "{ not json").unwrap();

    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = {:?}\napps_dir = {:?}\n",
            root.path().join("data"),
            apps_dir
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().expect("stdout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut addr = None;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        if Instant::now() > deadline {
            panic!("server did not start");
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("server exited"),
            Ok(_) => {
                if line.contains("listening") {
                    if let Some(pos) = line.find("addr=") {
                        addr = Some(line[pos + 5..].trim().to_owned());
                    }
                    break;
                }
            }
            Err(e) => panic!("read: {e}"),
        }
    }
    let addr = addr.expect("addr");
    let response = reqwest::get(format!("http://{addr}/apps/good/index.html"))
        .await
        .expect("get");
    assert_eq!(response.status(), 200);
    // bad 应用返回 APP_NOT_FOUND
    let bad_resp = reqwest::get(format!("http://{addr}/apps/bad/index.html"))
        .await
        .expect("get bad");
    assert_eq!(bad_resp.status(), 404);

    let pid = child.id();
    Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill");
    let output = child.wait().expect("wait");
    assert!(output.success());
    // 数据目录与日志目录就绪（审计落盘点）
    assert!(root.path().join("data/logs").is_dir());
}

#[test]
fn serve_missing_apps_dir_is_created() {
    // 发布形态首跑友好：apps 目录缺失时自动创建（配置发现链，实施文档 M8）
    let root = TempDir::new().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = {:?}\napps_dir = {:?}\n",
            root.path().join("data"),
            root.path().join("missing-apps")
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .args(["--config", config.to_str().unwrap(), "--no-tray"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let stdout = child.stdout.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        assert!(Instant::now() <= deadline, "server did not start");
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("server exited early"),
            Ok(_) => {
                if line.contains("listening") {
                    break;
                }
            }
            Err(e) => panic!("read stdout: {e}"),
        }
    }
    assert!(
        root.path().join("missing-apps").is_dir(),
        "apps 目录应被自动创建"
    );
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
}
