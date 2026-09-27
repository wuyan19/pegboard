//! 构造并签名在线升级 manifest（发布流程，开发机/CI 专用）。
//!
//! 输入 payload 文件（JSON）：
//! ```json
//! {
//!   "version": "0.2.0",
//!   "notes_url": "https://example.com/release",
//!   "platforms": {
//!     "macos-x86_64-app":  { "url": "https://…/Pegboard.app.zip", "file": "dist/Pegboard.app.zip" },
//!     "windows-x86_64":    { "url": "https://…/pegboard.exe",     "file": "dist/pegboard.exe" },
//!     "linux-x86_64":      { "url": "https://…/pegboard",         "file": "dist/pegboard" }
//!   }
//! }
//! ```
//! 输出 update-manifest.json：信封 `{payload, signature}`，每个平台补全
//! `size` / `sha256`（对 `file` 指向的本地资产计算）与 `signature`（对资产字节
//! 的 minisign 签名全文）；外层签名对 payload 字节。客户端侧验签逻辑见
//! server/src/host/update.rs。
//!
//! 用法：
//! ```shell
//! cargo run --example update_sign -- <私钥文件> <payload.json> <输出文件> [--password=<口令>]
//! ```
//! 口令缺省从私钥同目录的 `pegboard-update.pwd` 读取（update_keygen 的产物布局）。

use std::collections::BTreeMap;

const PLATFORMS: &[&str] = &[
    "macos-aarch64-app",
    "macos-x86_64-app",
    "windows-x86_64",
    "linux-x86_64",
];

fn die(msg: &str) -> ! {
    eprintln!("update_sign: {msg}");
    std::process::exit(1);
}

fn main() {
    let mut password: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            a if a.starts_with("--password=") => {
                password = Some(a.trim_start_matches("--password=").to_owned())
            }
            "--password" => password = args.next(),
            other => positional.push(other.to_owned()),
        }
    }
    let [key_path, payload_path, out_path] = match &positional[..] {
        [a, b, c] => [a.clone(), b.clone(), c.clone()],
        _ => die("用法: update_sign <私钥文件> <payload.json> <输出文件> [--password=<口令>]"),
    };

    let key_str = match std::fs::read_to_string(&key_path) {
        Ok(s) => s,
        Err(e) => die(&format!("读取私钥 {} 失败: {e}", key_path)),
    };
    let password = match password {
        Some(p) => p,
        None => {
            let pwd_file = std::path::Path::new(&key_path).with_extension("pwd");
            match std::fs::read_to_string(&pwd_file) {
                Ok(p) => p.trim().to_owned(),
                Err(e) => die(&format!(
                    "缺少口令: --password 未给，且读取 {} 失败: {e}",
                    pwd_file.display()
                )),
            }
        }
    };
    let sk_box = match minisign::SecretKeyBox::from_string(&key_str) {
        Ok(b) => b,
        Err(e) => die(&format!("私钥文件格式非法: {e}")),
    };
    let sk = match sk_box.into_secret_key(Some(password)) {
        Ok(sk) => sk,
        Err(e) => die(&format!("私钥口令错误或私钥损坏: {e}")),
    };

    // payload 原文（平台描述里的 file 字段就地展开，其余键值保留）
    let payload_raw = match std::fs::read_to_string(&payload_path) {
        Ok(s) => s,
        Err(e) => die(&format!("读取 {} 失败: {e}", payload_path)),
    };
    let mut payload: serde_json::Value = match serde_json::from_str(&payload_raw) {
        Ok(v) => v,
        Err(e) => die(&format!("payload JSON 解析失败: {e}")),
    };

    let platforms = payload
        .get_mut("platforms")
        .and_then(|p| p.as_object_mut())
        .unwrap_or_else(|| die("payload 缺 platforms 对象"));
    let mut assets: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (key, desc) in platforms.iter() {
        if !PLATFORMS.contains(&key.as_str()) {
            die(&format!(
                "未知平台 key: {key}（合法: {}）",
                PLATFORMS.join(", ")
            ));
        }
        let url = desc
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| die(&format!("平台 {key} 缺 url")));
        let file = desc
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| die(&format!("平台 {key} 缺 file（本地资产路径）")));
        let data = match std::fs::read(file) {
            Ok(d) => d,
            Err(e) => die(&format!("读取资产 {file} 失败: {e}")),
        };
        let mut hasher = sha2::Sha256::new();
        hasher.update(&data);
        use sha2::Digest as _;
        let sha256 = to_hex(&hasher.finalize());
        let trusted = format!(
            "pegboard-update\tfile:{}",
            std::path::Path::new(file)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        );
        let sig = match minisign::sign(None, &sk, data.as_slice(), Some(&trusted), None) {
            Ok(s) => s.to_string(),
            Err(e) => die(&format!("签名资产 {file} 失败: {e}")),
        };
        let entry = serde_json::json!({
            "url": url,
            "size": data.len(),
            "sha256": sha256,
            "signature": sig,
        });
        assets.insert(key.clone(), entry);
    }
    payload["platforms"] = serde_json::Value::Object(assets.into_iter().collect());
    let payload_bytes = serde_json::to_vec_pretty(&payload)
        .unwrap_or_else(|e| die(&format!("payload 序列化失败: {e}")));

    let envelope_sig = match minisign::sign(
        None,
        &sk,
        payload_bytes.as_slice(),
        Some("pegboard-update\tmanifest"),
        None,
    ) {
        Ok(s) => s.to_string(),
        Err(e) => die(&format!("签名 manifest 失败: {e}")),
    };
    let envelope = serde_json::json!({
        "payload": String::from_utf8(payload_bytes).expect("utf8"),
        "signature": envelope_sig,
    });
    let out = serde_json::to_string_pretty(&envelope).expect("序列化信封");
    if let Err(e) = std::fs::write(&out_path, out + "\n") {
        die(&format!("写 {} 失败: {e}", out_path));
    }
    println!("已生成 {}", out_path);
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
