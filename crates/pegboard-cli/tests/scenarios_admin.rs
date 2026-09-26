//! Admin 页面真实浏览器冒烟（Playwright + 系统 Chrome）：
//! 加载、server-info、应用链接、用量展开、日志过滤、XSS 转义、安装/卸载。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
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

fn start_server(apps_dir: &std::path::Path) -> (TempDir, Child, String) {
    let root = TempDir::new().expect("tempdir");
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
        assert!(Instant::now() <= deadline, "server did not start");
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
            Err(e) => panic!("read stdout: {e}"),
        }
    }
    let _ = child.stdout.take();
    (root, child, format!("http://{}", addr.expect("addr")))
}

fn stop(mut child: Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
}

#[tokio::test]
async fn admin_page_browser_smoke() {
    let staging = TempDir::new().expect("staging");
    let apps = staging.path().join("apps");
    write_app(&apps, "lan-share", r#"{"files":true}"#);
    // 触发一条 Denied 审计事件：未授权能力调用（lan-share 无 store）
    let (_root, child, base) = start_server(&apps);
    // 造 25 条 Denied 审计事件（lan-share 未声明 store），让 20/页 有第二页
    let client = reqwest::Client::new();
    for _ in 0..25 {
        let _ = client
            .get(format!("{base}/api/store/kv/x"))
            .header("x-pegboard-app", "lan-share")
            .send()
            .await
            .expect("trigger denied event");
    }

    let out = tokio::process::Command::new("node")
        .arg("tests/e2e/admin_smoke.js")
        .arg(&base)
        .env("ADMIN_E2E_APPS_DIR", &apps)
        .current_dir(repo_root().join("crates/pegboard-cli"))
        .output()
        .await
        .expect("run admin smoke");

    stop(child);

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "admin smoke failed:\n{text}\nstatus: {:?}",
        out.status.code()
    );
    assert!(text.contains("ADMIN-SMOKE-OK"), "{text}");
}
