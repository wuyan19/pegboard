//! 生成在线升级的 minisign 签名密钥对（一次性开发者动作）。
//!
//! 用法：
//! ```shell
//! cargo run --example update_keygen -- --password="私钥口令" [--force]
//! ```
//!
//! 产物：
//! - `.sign/pegboard-update.key` 私钥（口令加密；目录已 gitignore，绝不提交）
//! - `.sign/pegboard-update.pwd` 口令明文（供 update_sign 免交互读取）
//! - `crates/pegboard-server/assets/update-pubkey.txt` 公钥（提交进仓库，客户端编译期内嵌验签）
//!
//! 密钥轮换：生成新密钥对后，把新旧公钥都挂进 server/src/host/update.rs 的
//! UPDATE_PUBKEYS 数组，发一个过渡版本让存量客户端信任新钥，之后移除旧公钥。

fn main() {
    let mut password: Option<String> = None;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            a if a.starts_with("--password=") => {
                password = Some(a.trim_start_matches("--password=").to_owned())
            }
            "--force" => force = true,
            other => {
                eprintln!("未知参数: {other}（只支持 --password=<口令> 和 --force）");
                std::process::exit(2);
            }
        }
    }
    let password = match password {
        Some(p) => p,
        None => {
            eprintln!("用法: cargo run --example update_keygen -- --password=<私钥口令> [--force]");
            eprintln!("口令必填（用于加密私钥文件），请妥善保管。");
            std::process::exit(2);
        }
    };

    let dir = std::path::Path::new(".sign");
    let sk_path = dir.join("pegboard-update.key");
    let pwd_path = dir.join("pegboard-update.pwd");
    let repo_pk = std::path::Path::new("crates/pegboard-server/assets/update-pubkey.txt");

    if sk_path.exists() && !force {
        eprintln!(
            "已存在 {}（--force 覆盖；轮换密钥先读本文件顶部注释）",
            sk_path.display()
        );
        std::process::exit(1);
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("创建 .sign 目录失败: {e}");
        std::process::exit(1);
    }

    let minisign::KeyPair { pk, sk } =
        match minisign::KeyPair::generate_encrypted_keypair(Some(password.clone())) {
            Ok(kp) => kp,
            Err(e) => {
                eprintln!("生成密钥对失败: {e}");
                std::process::exit(1);
            }
        };
    let sk_box = match sk.to_box(None) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("序列化私钥失败: {e}");
            std::process::exit(1);
        }
    };
    let pk_box = match pk.to_box() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("序列化公钥失败: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::write(&sk_path, sk_box.to_string()) {
        eprintln!("写私钥失败: {e}");
        std::process::exit(1);
    }
    // 私钥文件收紧权限（0600）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sk_path, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::write(&pwd_path, &password) {
        eprintln!("写口令文件失败: {e}");
        std::process::exit(1);
    }
    if let Some(parent) = repo_pk.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("创建 {} 失败: {e}", parent.display());
            std::process::exit(1);
        }
    }
    if let Err(e) = std::fs::write(repo_pk, pk_box.to_string()) {
        eprintln!("写 {} 失败: {e}", repo_pk.display());
        std::process::exit(1);
    }

    println!(
        "私钥: {}（.sign/ 已 gitignore，勿提交、勿外传）",
        sk_path.display()
    );
    println!("口令: {}", pwd_path.display());
    println!(
        "公钥: {}（提交进仓库，客户端编译期内嵌）",
        repo_pk.display()
    );
}
