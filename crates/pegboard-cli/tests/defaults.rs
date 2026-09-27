//! 配置发现与平台默认目录（发布形态行为，实施文档 8.9）。
//!
//! HOME 重定向到临时目录，平台数据区随之落在临时目录内，不污染真实用户目录；
//! Windows 依赖 APPDATA（无法由 HOME 推导），本套件不覆盖。

#![cfg(not(target_os = "windows"))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 平台数据区基（与 cli.rs 的 platform_data_dir 对应；XDG 变量已被清除，
/// 代码回退到 HOME 推导）。
fn platform_data_base(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Pegboard")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        home.join(".local/share/pegboard")
    }
}

/// 子进程守卫：任何退出路径（含断言失败）都 TERM + wait，不留孤儿。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

/// 无配置启动：等 listening 行（build 完成后目录已落盘）。
/// zombie_processes 豁免：Child 的 wait 责任在 ChildGuard::drop（clippy 不跨包装追踪）。
#[allow(clippy::zombie_processes)]
fn spawn_pegboard(cwd: &Path, home: &Path) -> ChildGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pegboard"))
        .args(["--no-tray", "--listen", "127.0.0.1:0"])
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("PEGBOARD_CONFIG")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pegboard");
    let stdout = child.stdout.take().expect("stdout");
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
                    return ChildGuard(child);
                }
            }
            Err(e) => panic!("read stdout: {e}"),
        }
    }
}

#[test]
fn no_config_defaults_to_platform_dirs() {
    let home = tempfile::TempDir::new().expect("home");
    let cwd = tempfile::TempDir::new().expect("cwd");
    let _child = spawn_pegboard(cwd.path(), home.path());

    let base = platform_data_base(home.path());
    assert!(
        base.join("data/apps_data").is_dir(),
        "数据目录应默认落在平台数据区: {:?}",
        base
    );
    assert!(base.join("apps").is_dir(), "apps 目录应自动创建");
    assert!(!cwd.path().join("data").exists(), "cwd 不应有数据落盘");
    assert!(!cwd.path().join("apps").exists());
}

#[test]
fn platform_config_dir_discovered_and_rebased() {
    let home = tempfile::TempDir::new().expect("home");
    let cwd = tempfile::TempDir::new().expect("cwd");
    let cfg_dir = platform_data_base(home.path());
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = \"./hostdata\"\napps_dir = \"./hostapps\"\n",
    )
    .expect("write platform config");

    let _child = spawn_pegboard(cwd.path(), home.path());
    assert!(
        cfg_dir.join("hostdata/apps_data").is_dir(),
        "平台配置的相对路径应重定基到平台数据区"
    );
    assert!(cfg_dir.join("hostapps").is_dir());
    assert!(!cwd.path().join("hostdata").exists(), "不得落在 cwd");
}

#[test]
fn cwd_config_wins_over_platform() {
    let home = tempfile::TempDir::new().expect("home");
    let cwd = tempfile::TempDir::new().expect("cwd");
    // 平台配置目录也放一份（带标记数据路径）——cwd 命中时不得使用
    let cfg_dir = platform_data_base(home.path());
    std::fs::create_dir_all(&cfg_dir).expect("mkdir cfg dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = \"./platdata\"\n",
    )
    .expect("write platform config");
    std::fs::write(
        cwd.path().join("config.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\ndata_root = \"./cwddata\"\napps_dir = \"./cwdapps\"\n",
    )
    .expect("write cwd config");

    let _child = spawn_pegboard(cwd.path(), home.path());
    assert!(
        cwd.path().join("cwddata/apps_data").is_dir(),
        "cwd 配置应生效（开发树行为不变，相对路径相对 cwd）"
    );
    assert!(cwd.path().join("cwdapps").is_dir());
    assert!(!cfg_dir.join("platdata").exists(), "平台配置不应被使用");
}
