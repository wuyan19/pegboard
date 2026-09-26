//! M1 集成测试：--check 自检与「坏应用不阻塞」。
//! 驱动真实 pegboard 二进制（CARGO_BIN_EXE_pegboard）。

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn write_app(apps: &Path, id: &str, manifest: &str) {
    let dir = apps.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<h1>x</h1>").unwrap();
    std::fs::write(dir.join("manifest.json"), manifest).unwrap();
}

fn good_manifest(id: &str) -> String {
    format!(r#"{{"id":"{id}","name":"{id}","entry":"index.html"}}"#)
}

/// config 与 apps 都放进 holder 临时目录，返回 config 路径。
fn make_config(holder: &TempDir, apps: &Path) -> std::path::PathBuf {
    let config = holder.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[storage]\ndata_root = {:?}\napps_dir = {:?}\n",
            holder.path().join("data"),
            apps
        ),
    )
    .unwrap();
    config
}

fn run_check(config: &Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .arg("--check")
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();
    let code = out.status.code().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (code, text)
}

#[test]
fn check_passes_on_valid_apps() {
    let apps_dir = TempDir::new().unwrap();
    write_app(apps_dir.path(), "good", &good_manifest("good"));
    let holder = TempDir::new().unwrap();
    let config = make_config(&holder, apps_dir.path());
    let (code, text) = run_check(&config);
    assert_eq!(code, 0, "output: {text}");
    assert!(text.contains("good"), "output: {text}");
}

#[test]
fn bad_app_does_not_block_good_app() {
    let apps_dir = TempDir::new().unwrap();
    write_app(apps_dir.path(), "good", &good_manifest("good"));
    write_app(apps_dir.path(), "bad", "{ not json");
    let holder = TempDir::new().unwrap();
    let config = make_config(&holder, apps_dir.path());
    let (code, text) = run_check(&config);
    // 坏应用使 --check 报错（exit 1），但好应用仍被扫描与列出（不阻塞）
    assert_eq!(code, 1, "output: {text}");
    assert!(text.contains("good"), "good app listed: {text}");
    assert!(text.contains("bad"), "bad app reported: {text}");
}

#[test]
fn check_fails_on_missing_apps_dir() {
    let holder = TempDir::new().unwrap();
    let config = make_config(&holder, &holder.path().join("nope-apps"));
    let (code, text) = run_check(&config);
    assert_eq!(code, 1, "output: {text}");
    assert!(text.contains("apps"), "output: {text}");
}

#[test]
fn check_reports_invalid_config() {
    let holder = TempDir::new().unwrap();
    let config = holder.path().join("config.toml");
    std::fs::write(&config, "[limits]\nnet_rps = 0\n").unwrap();
    let (code, _text) = run_check(&config);
    assert_eq!(code, 1);
}
