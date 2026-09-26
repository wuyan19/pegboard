//! M7 场景测试：三个示例应用端到端（Playwright headless 驱动真实页面，系统 Chrome）。
//! ollama-chat / glm-quota 的上游以本地 mock 替代：页面 verbatim，仅清单白名单指向 mock。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// 暂存三个场景应用；ollama-chat → ollama mock，glm-quota → glm mock（页面 verbatim）。
fn staged_apps(apps_root: &Path, ollama_base: &str, glm_base: &str) -> (TempDir, PathBuf) {
    let staging = TempDir::new().expect("staging");
    let apps = staging.path().join("apps");
    for app in ["ollama-chat", "glm-quota", "lan-share"] {
        copy_dir(&apps_root.join(app), &apps.join(app)).expect("copy app");
    }
    for (app, needle, base) in [
        (
            "ollama-chat",
            "\"net\": [\"http://127.0.0.1:11434\"]",
            ollama_base,
        ),
        (
            "glm-quota",
            "\"net\": [\"https://open.bigmodel.cn\"]",
            glm_base,
        ),
    ] {
        let manifest = apps.join(app).join("manifest.json");
        let text = std::fs::read_to_string(&manifest).expect("read manifest");
        std::fs::write(
            &manifest,
            text.replace(needle, &format!("\"net\": [\"{base}\"]")),
        )
        .expect("write manifest");
    }
    (staging, apps)
}

fn start_mocks() -> (Child, String, String) {
    let mut child = Command::new("node")
        .arg("tests/fixtures/mocks_fixture.js")
        .current_dir(repo_root().join("crates/pegboard-cli"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mocks");
    let stdout = child.stdout.take().expect("stdout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ollama = None;
    let mut glm = None;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        assert!(Instant::now() <= deadline, "mocks did not start");
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("mocks exited"),
            Ok(_) => {
                if let Some(rest) = line.strip_prefix("OLLAMA-READY ") {
                    ollama = Some(rest.trim().to_owned());
                } else if let Some(rest) = line.strip_prefix("GLM-READY ") {
                    glm = Some(rest.trim().to_owned());
                }
                if ollama.is_some() && glm.is_some() {
                    break;
                }
            }
            Err(e) => panic!("read mocks stdout: {e}"),
        }
    }
    let _ = child.stdout.take();
    (child, ollama.expect("ollama"), glm.expect("glm"))
}

fn start_server(apps_dir: &Path) -> (TempDir, Child, String) {
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
    let deadline = Instant::now() + Duration::from_secs(15);
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
            Err(e) => panic!("read server stdout: {e}"),
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
async fn m7_three_scenario_apps() {
    // 1. mock 上游（ollama 聊天 + glm 额度）
    let (mut mock_child, ollama_base, glm_base) = start_mocks();
    // 2. 暂存应用（两个清单白名单指向 mock）
    let (_staging, apps_dir) = staged_apps(&repo_root().join("apps"), &ollama_base, &glm_base);
    // 3. 起 pegboard
    let (_server_root, server_child, base) = start_server(&apps_dir);
    // 4. Playwright 驱动三个场景
    let out = tokio::process::Command::new("node")
        .arg("tests/e2e/m7_scenarios.js")
        .arg(&base)
        .arg(&ollama_base)
        .arg(&glm_base)
        .current_dir(repo_root().join("crates/pegboard-cli"))
        .output()
        .await
        .expect("run playwright scenarios");

    stop(server_child);
    stop(mock_child);

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "m7 scenarios failed:\n{text}\nstatus: {:?}",
        out.status.code()
    );
    for marker in [
        "SCENARIO-OLLAMA-OK",
        "SCENARIO-GLM-OK",
        "SCENARIO-LANSHARE-OK",
    ] {
        assert!(text.contains(marker), "缺少 {marker}:\n{text}");
    }
}
