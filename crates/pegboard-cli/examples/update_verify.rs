//! 验证 update-manifest.json（发布链路的 smoke 自检：manifest 上传到 release
//! 后下载回来，走客户端同一条验签路径——在这里失败好过发布后所有客户端
//! 静默拒绝更新）。
//!
//! 用法：cargo run --example update_verify -- <update-manifest.json>
//! 成功打印版本与平台资产表，退出码 0；验签失败退出码 1。

use pegboard_server::host::update::verify_signed_manifest;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("用法: update_verify <update-manifest.json>");
        std::process::exit(2);
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读取 {path} 失败: {e}");
            std::process::exit(1);
        }
    };
    match verify_signed_manifest(&body) {
        Ok(m) => {
            println!("验签通过 · version {}", m.version);
            for (key, asset) in &m.platforms {
                println!(
                    "  {key}: {} bytes, sha256 {}…",
                    asset.size,
                    &asset.sha256[..8.min(asset.sha256.len())]
                );
            }
        }
        Err(e) => {
            eprintln!("验签失败: {e}");
            std::process::exit(1);
        }
    }
}
