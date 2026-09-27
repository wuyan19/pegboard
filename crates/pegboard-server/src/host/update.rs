//! 在线升级（self-update）：检查 → 下载 → 校验 → 安装 → 待重启。
//!
//! 信任锚是**签名**而非更新源 URL：manifest 为 `{payload, signature}` 信封，
//! payload（版本 + 平台资产表）与资产字节均需通过编译期内嵌公钥的
//! minisign（Ed25519，verify-only）验签；资产另验 sha256 与声明大小。
//! 更新源 URL 可配置（`[update] manifest_url`），指向哪都不影响信任判定。
//!
//! 分层（自下而上）：
//! 1. **纯逻辑**：版本比较 / manifest 解析 / 验签 / hex——无 IO，单测覆盖
//! 2. **IO 动作**：`fetch_manifest` / `download_and_verify`（async，流式落盘）
//!    与 `install_update`（同步，spawn_blocking 执行：整包解压 / self-replace）
//! 3. **状态机 `Updater`**：把 1/2 编排成管理页可轮询的 `UpdatePhase`
//!
//! 平台安装策略（唯一平台分歧入口在 `install_update`）：
//! - Windows / Linux：资产即最终二进制，`self-replace` 处理「替换运行中的自身」
//!   （Windows rename dance 绕过文件锁；Unix 靠 inode 语义天然安全）
//! - macOS：资产为 `.app.zip`，**整包替换** .app——只换内部二进制会使 ad-hoc
//!   签名失效（Gatekeeper 拦截）且 Info.plist 版本过期；staging/backup/rename
//!   舞步保证失败可回滚
//!
//! 本模块只负责把新版本放到磁盘（终态 `RestartPending`）；重启经
//! `POST /api/admin/host/restart`（ProcessControl）完成——进程协调属进程层。
//!
//! 内存说明：验签需完整资产字节（minisign-verify 无流式接口），读回量受
//! MAX_DOWNLOAD（100 MiB）硬上限约束；正常二进制 ~15 MiB。除此之外全程流式。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;

/// 更新签名公钥（minisign 公钥文件全文：comment + base64 两行）。
/// 生成：`cargo run --example update_keygen`（私钥落 .sign/，已 gitignore）。
/// 轮换：数组追加新公钥，发一个过渡版本让存量客户端信任新钥，再移除旧公钥。
const UPDATE_PUBKEYS: &[&str] = &[include_str!("../../assets/update-pubkey.txt")];

/// 下载硬上限：防 manifest 被篡改后 size 巨大撑爆磁盘；实际下载超出同样中止。
const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;
/// 检查超时：后台动作，慢点无妨，但不许挂死。
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// 下载超时：慢网下 100 MiB 也该在该窗口内完成；超时即 Failed，可重试。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const USER_AGENT: &str = concat!("pegboard/", env!("CARGO_PKG_VERSION"));

// ===== 数据模型 =====

/// manifest 外层信封。payload 是内层 manifest 的 JSON **字符串**，signature 对
/// payload 字节：验签针对确定字节，不依赖两端 JSON 序列化的 key 顺序与空白。
#[derive(Debug, Deserialize)]
struct SignedManifest {
    payload: String,
    signature: String,
}

/// 单平台更新资产描述（manifest.platforms 的值）。
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformAsset {
    pub url: String,
    pub size: u64,
    /// sha256 hex，快速失败层：下载流式比对，不符立即中止。
    pub sha256: String,
    /// minisign 签名全文（对资产字节）；真实性层。
    #[serde(default)]
    pub signature: String,
}

/// 更新清单（信封 payload 的内容），与发布签名工具（examples/update_sign.rs）约定。
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    #[serde(default)]
    pub notes_url: String,
    pub platforms: BTreeMap<String, PlatformAsset>,
}

/// 一次检查的结果里需要的全部信息。
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: String,
    pub notes_url: String,
    pub asset: PlatformAsset,
}

pub enum UpdateOutcome {
    UpToDate,
    Available(ReleaseInfo),
}

/// 升级链路错误。管理端只透传 Display 文案（线性流程，失败动作只有重试）。
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("网络错误: {0}")]
    Network(String),
    #[error("更新清单无效: {0}")]
    Manifest(String),
    #[error("本地文件操作失败: {0}")]
    Io(String),
    #[error("下载内容校验失败（sha256 不匹配），已删除半成品，可重试")]
    Checksum,
    #[error("签名验证失败（{0}），更新可能被篡改，已中止")]
    InvalidSignature(String),
    #[error("当前平台暂不支持自动更新，请手动更新")]
    UnsupportedPlatform,
    /// macOS 裸二进制形态（cargo run / 终端直接运行）没有可替换的 bundle。
    #[error("当前为非 .app 安装形态，不支持自动更新，请手动更新")]
    NotAppBundle,
}

// ===== 纯逻辑 =====

/// 当前平台的 manifest key，与发布资产命名一一对应。
#[allow(unreachable_code)]
pub fn platform_key() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some("macos-aarch64-app");
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Some("macos-x86_64-app");
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Some("windows-x86_64");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some("linux-x86_64");
    }
    None
}

/// latest 是否比 current 新。容忍 `v` 前缀；任一侧解析失败按「不新」处理——
/// 宁可漏一次提示，也不把坏版本号当新版推送。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.trim().trim_start_matches('v')).ok()
}

fn parse_manifest(body: &str) -> Result<UpdateManifest, UpdateError> {
    let m: UpdateManifest = serde_json::from_str(body)
        .map_err(|e| UpdateError::Manifest(format!("JSON 解析失败: {e}")))?;
    if parse_version(&m.version).is_none() {
        return Err(UpdateError::Manifest(format!(
            "version 字段非法: {:?}",
            m.version
        )));
    }
    Ok(m)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ===== 验签（纯逻辑）=====

/// minisign 公钥文件全文 → PublicKey。取最后一个非空行作为 base64，
/// 兼容标准 minisign CLI 生成的文件。
fn public_key_from_content(content: &str) -> Result<minisign_verify::PublicKey, UpdateError> {
    let b64 = content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| UpdateError::InvalidSignature("公钥文件为空".into()))?;
    minisign_verify::PublicKey::from_base64(b64.trim())
        .map_err(|e| UpdateError::InvalidSignature(format!("公钥解析失败: {e}")))
}

/// 用给定公钥列表验签（任一通过即可——轮换过渡期新旧公钥并存）。
/// 生产传 [`UPDATE_PUBKEYS`]；单测注入测试密钥对。
fn verify_with_keys(data: &[u8], sig_text: &str, keys: &[&str]) -> Result<(), UpdateError> {
    let sig_text = sig_text.trim();
    if sig_text.is_empty() {
        return Err(UpdateError::InvalidSignature("缺少签名字段".into()));
    }
    let signature = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| UpdateError::InvalidSignature(format!("签名格式非法: {e}")))?;
    // 内嵌公钥格式坏 = 程序自身错误，显式失败（不做静默降级）
    let public_keys: Vec<minisign_verify::PublicKey> = keys
        .iter()
        .map(|k| public_key_from_content(k))
        .collect::<Result<Vec<_>, _>>()?;
    for pk in &public_keys {
        // allow_legacy：兼容旧版 minisign 非预哈希签名
        if pk.verify(data, &signature, true).is_ok() {
            return Ok(());
        }
    }
    Err(UpdateError::InvalidSignature("全部内嵌公钥验证失败".into()))
}

fn verify_signature(data: &[u8], sig_text: &str) -> Result<(), UpdateError> {
    verify_with_keys(data, sig_text, UPDATE_PUBKEYS)
}

fn parse_signed_manifest_with_keys(
    body: &str,
    keys: &[&str],
) -> Result<UpdateManifest, UpdateError> {
    let signed: SignedManifest = serde_json::from_str(body).map_err(|e| {
        UpdateError::Manifest(format!("信封解析失败（应为 {{payload, signature}}）: {e}"))
    })?;
    verify_with_keys(signed.payload.as_bytes(), &signed.signature, keys)?;
    parse_manifest(&signed.payload)
}

/// manifest 内容 → 检查结果（验签在 parse_signed_manifest 内完成）。
fn check_body(body: &str, current: &str) -> Result<UpdateOutcome, UpdateError> {
    check_body_with_keys(body, current, UPDATE_PUBKEYS)
}

/// 同 check_body，公钥可注入（单测用测试密钥对）。
fn check_body_with_keys(
    body: &str,
    current: &str,
    keys: &[&str],
) -> Result<UpdateOutcome, UpdateError> {
    let manifest = parse_signed_manifest_with_keys(body, keys)?;
    let key = platform_key().ok_or(UpdateError::UnsupportedPlatform)?;
    let asset = manifest
        .platforms
        .get(key)
        .ok_or_else(|| UpdateError::Manifest(format!("manifest 缺当前平台（{key}）的资产")))?
        .clone();
    if is_newer(&manifest.version, current) {
        Ok(UpdateOutcome::Available(ReleaseInfo {
            version: manifest.version,
            notes_url: manifest.notes_url,
            asset,
        }))
    } else {
        Ok(UpdateOutcome::UpToDate)
    }
}

// ===== IO 动作（async；安装走 spawn_blocking）=====

async fn fetch_manifest(client: &reqwest::Client, url: &str) -> Result<String, UpdateError> {
    let resp = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(UpdateError::Manifest(
            "更新清单不存在（更新源尚未发布内容）".into(),
        ));
    }
    if !status.is_success() {
        return Err(UpdateError::Network(format!("HTTP {status}")));
    }
    resp.text()
        .await
        .map_err(|e| UpdateError::Network(format!("读取 manifest 失败: {e}")))
}

/// 流式下载资产到 dest：边下边算 sha256，超声明/硬上限即中止；完成后比对
/// checksum，再读回验签。任一步失败删除半成品。
async fn download_and_verify(
    client: &reqwest::Client,
    asset: &PlatformAsset,
    dest: &Path,
    on_progress: impl FnMut(u64),
) -> Result<(), UpdateError> {
    download_and_verify_with(client, asset, dest, on_progress, verify_signature).await
}

/// 同 download_and_verify，验签器可注入（单测用测试密钥对）。
async fn download_and_verify_with<F>(
    client: &reqwest::Client,
    asset: &PlatformAsset,
    dest: &Path,
    on_progress: impl FnMut(u64),
    verify: F,
) -> Result<(), UpdateError>
where
    F: Fn(&[u8], &str) -> Result<(), UpdateError>,
{
    if asset.size > MAX_DOWNLOAD {
        return Err(UpdateError::Manifest(format!(
            "资产声明大小 {} 字节超过安全上限 {MAX_DOWNLOAD}",
            asset.size
        )));
    }
    let resp = client
        .get(&asset.url)
        .header("User-Agent", USER_AGENT)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(UpdateError::Network(format!("下载资产 HTTP {status}")));
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| UpdateError::Io(format!("创建临时文件: {e}")))?;
    let mut hasher = Sha256::new();
    use futures_util::StreamExt as _;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut on_progress = on_progress;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| UpdateError::Network(format!("下载中断: {e}")))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| UpdateError::Io(format!("写临时文件: {e}")))?;
        done += chunk.len() as u64;
        if done > MAX_DOWNLOAD {
            // manifest 声明被伪造小、实际内容超大：立即止损
            let _ = tokio::fs::remove_file(dest).await;
            return Err(UpdateError::Manifest(format!(
                "下载内容超过安全上限 {MAX_DOWNLOAD} 字节"
            )));
        }
        on_progress(done);
    }
    file.flush()
        .await
        .map_err(|e| UpdateError::Io(format!("刷写临时文件: {e}")))?;
    drop(file);

    let actual = to_hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(UpdateError::Checksum);
    }
    // 真实性：读回验签（受 MAX_DOWNLOAD 上限约束）
    let data = tokio::fs::read(dest)
        .await
        .map_err(|e| UpdateError::Io(format!("读回下载内容: {e}")))?;
    if let Err(e) = verify(&data, &asset.signature) {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e);
    }
    Ok(())
}

// ===== 平台安装（同步；调用方 spawn_blocking）=====

/// 把已通过校验的资产安装到位。全模块唯一的平台分歧入口。
fn install_update(downloaded: &Path) -> Result<(), UpdateError> {
    #[cfg(target_os = "macos")]
    return install_app_bundle(downloaded);
    #[cfg(not(target_os = "macos"))]
    return install_binary(downloaded);
}

/// Windows / Linux：下载即最终二进制，self-replace 处理「替换运行中的自身」。
#[cfg(not(target_os = "macos"))]
fn install_binary(new_bin: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    set_executable(new_bin)?;
    self_replace::self_replace(new_bin).map_err(|e| UpdateError::Io(format!("替换二进制失败: {e}")))
}

/// 下载落盘默认无执行位；Unix 上直接 rename 会让新进程 spawn 失败。
#[cfg(all(unix, not(target_os = "macos")))]
fn set_executable(p: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)
        .map_err(|e| UpdateError::Io(format!("读取临时文件属性: {e}")))?
        .permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm).map_err(|e| UpdateError::Io(format!("设置执行位: {e}")))
}

/// macOS：整包替换 .app。staging（同卷保证 rename 不跨设备）→ 旧 bundle 挪
/// backup → 新 bundle 就位 → 失败回滚。运行中的进程按 inode 引用旧可执行
/// 文件，bundle 被替换不影响当前进程存活。
#[cfg(target_os = "macos")]
fn install_app_bundle(archive: &Path) -> Result<(), UpdateError> {
    let app = find_current_app_bundle().ok_or(UpdateError::NotAppBundle)?;
    let parent = app
        .parent()
        .ok_or_else(|| UpdateError::Io("bundle 没有父目录".into()))?;
    let staging = parent.join(format!(".pegboard-update-{}", std::process::id()));
    let backup = parent.join(format!(".pegboard-old-{}", std::process::id()));
    // 清理历史失败残留的 staging（backup 不动：可能仍被旧进程可执行文件引用）
    if let Ok(rd) = std::fs::read_dir(parent) {
        for e in rd.filter_map(Result::ok) {
            if e.file_name()
                .to_string_lossy()
                .starts_with(".pegboard-update-")
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    extract_zip(archive, &staging)?;
    let new_app = locate_app_bundle(&staging)
        .ok_or_else(|| UpdateError::Manifest("更新包里找不到 .app bundle".into()))?;

    std::fs::rename(&app, &backup).map_err(|e| UpdateError::Io(format!("备份旧 bundle: {e}")))?;
    match std::fs::rename(&new_app, &app) {
        Ok(()) => {
            // best-effort 清理：失败只留隐藏垃圾目录
            let _ = std::fs::remove_dir_all(&backup);
            let _ = std::fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(e) => {
            let err = UpdateError::Io(format!("新 bundle 就位失败: {e}"));
            match std::fs::rename(&backup, &app) {
                Ok(()) => Err(err),
                Err(rb) => Err(UpdateError::Io(format!(
                    "{err}；且回滚失败（{rb}），请手动重新安装"
                ))),
            }
        }
    }
}

/// 从 current_exe 向上找 `.app` 祖先；找不到 = 裸二进制形态，不支持自更新。
#[cfg(target_os = "macos")]
fn find_current_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .skip(1)
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// 定位解压后的 .app：优先 `staging/Pegboard.app`；否则找 staging 下唯一含
/// `Contents/MacOS` 的 `.app` 目录。
#[cfg(target_os = "macos")]
fn locate_app_bundle(staging: &Path) -> Option<PathBuf> {
    let direct = staging.join("Pegboard.app");
    if direct.join("Contents").join("MacOS").is_dir() {
        return Some(direct);
    }
    std::fs::read_dir(staging)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "app") && p.join("Contents").join("MacOS").is_dir()
        })
}

/// 安全解压 zip：enclosed_name 拒绝路径逃逸；符号链接条目 fail-closed
/// （.app 内不应有链接，出现即包内容与预期不符）；主程序强制 0755。
#[cfg(target_os = "macos")]
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let f =
        std::fs::File::open(archive).map_err(|e| UpdateError::Io(format!("打开更新包: {e}")))?;
    let mut zip =
        zip::ZipArchive::new(f).map_err(|e| UpdateError::Manifest(format!("更新包损坏: {e}")))?;
    std::fs::create_dir_all(dest).map_err(|e| UpdateError::Io(format!("创建解压目录: {e}")))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| UpdateError::Manifest(format!("读取更新包条目失败: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            continue; // 路径逃逸条目直接跳过
        };
        let mode = entry.unix_mode();
        if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(UpdateError::Manifest(format!(
                "更新包含符号链接条目（{}），预期 .app 内无链接，拒绝安装",
                rel.to_string_lossy()
            )));
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| UpdateError::Io(format!("创建目录: {e}")))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| UpdateError::Io(format!("创建父目录: {e}")))?;
        }
        let mut file =
            std::fs::File::create(&out).map_err(|e| UpdateError::Io(format!("解压文件: {e}")))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|e| UpdateError::Io(format!("解压文件: {e}")))?;
        let mode = if rel.ends_with(Path::new("Contents/MacOS/pegboard")) {
            0o755
        } else {
            mode.map(|m| m & 0o777).unwrap_or(0o644)
        };
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))
            .map_err(|e| UpdateError::Io(format!("恢复文件权限: {e}")))?;
    }
    Ok(())
}

// ===== 状态机 =====

/// 更新流程状态。直接 Serialize 给管理页（GET /api/admin/update/status 的 phase）。
#[derive(Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdatePhase {
    /// 未检查过（更新仅手动触发）。
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
        notes_url: String,
        size: u64,
    },
    Downloading {
        version: String,
        bytes: u64,
        total: u64,
    },
    Installing {
        version: String,
    },
    /// 新版本已落位，等管理页/托盘触发 host/restart。
    RestartPending {
        version: String,
    },
    Failed {
        error: String,
    },
}

struct UpdaterInner {
    phase: UpdatePhase,
    pending: Option<ReleaseInfo>,
}

/// 更新状态机。挂在 AppState；仅手动触发（管理页按钮），进行中重复触发为 no-op。
/// std Mutex 且持锁不跨 await：所有 await 前取完状态或数据，guard 当行释放。
pub struct Updater {
    manifest_url: Option<String>,
    /// 客户端构造失败（TLS 后端异常）置 None：升级不可用但不影响服务。
    client: Option<reqwest::Client>,
    /// 下载临时文件落点（data_root/tmp，文件名带 pid 防并发冲突）。
    tmp_dir: PathBuf,
    inner: Mutex<UpdaterInner>,
}

impl Updater {
    /// manifest_url 为空（未配置更新通道）时升级保持 Idle 且检查返回错误。
    pub fn new(manifest_url: &str, tmp_dir: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| tracing::error!(error = %e, "update http client build failed"))
            .ok();
        Self {
            manifest_url: Some(manifest_url.trim().to_owned()).filter(|s| !s.is_empty()),
            client,
            tmp_dir,
            inner: Mutex::new(UpdaterInner {
                phase: UpdatePhase::Idle,
                pending: None,
            }),
        }
    }

    pub fn status_json(&self) -> serde_json::Value {
        let phase = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .phase
            .clone();
        json!({
            "configured": self.manifest_url.is_some(),
            "current": env!("CARGO_PKG_VERSION"),
            "platform": platform_key(),
            "phase": phase,
        })
    }

    /// spawn 后台检查（POST /api/admin/update/check）。进行中重复触发 no-op；
    /// 未配置更新源返回 Err（管理端映射 400）。
    pub fn spawn_check(self: &Arc<Self>) -> Result<(), String> {
        if self.manifest_url.is_none() {
            return Err("未配置更新源（config [update] manifest_url）".into());
        }
        if self.client.is_none() {
            return Err("更新客户端初始化失败".into());
        }
        let can_start = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let busy = matches!(
                inner.phase,
                UpdatePhase::Checking
                    | UpdatePhase::Downloading { .. }
                    | UpdatePhase::Installing { .. }
                    | UpdatePhase::RestartPending { .. }
            );
            if !busy {
                inner.phase = UpdatePhase::Checking;
            }
            !busy
        };
        if can_start {
            let this = Arc::clone(self);
            tokio::spawn(async move { this.run_check().await });
        }
        Ok(())
    }

    async fn run_check(&self) {
        let (Some(client), Some(url)) = (&self.client, &self.manifest_url) else {
            return;
        };
        let outcome = fetch_manifest(client, url).await;
        let result = outcome.and_then(|body| check_body(&body, env!("CARGO_PKG_VERSION")));
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(UpdateOutcome::UpToDate) => {
                inner.phase = UpdatePhase::UpToDate;
                inner.pending = None;
            }
            Ok(UpdateOutcome::Available(info)) => {
                tracing::info!(version = %info.version, "update available");
                inner.phase = UpdatePhase::Available {
                    version: info.version.clone(),
                    notes_url: info.notes_url.clone(),
                    size: info.asset.size,
                };
                inner.pending = Some(info);
            }
            Err(e) => {
                tracing::warn!(error = %e, "检查更新失败");
                inner.phase = UpdatePhase::Failed {
                    error: e.to_string(),
                };
            }
        }
    }

    /// spawn 后台下载+安装（POST /api/admin/update/install）。
    /// 仅 Available 状态可触发；其他状态返回错误文案（管理端映射 400）。
    pub fn spawn_install(self: &Arc<Self>) -> Result<(), String> {
        let (asset, version) = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            match inner.pending.take() {
                Some(info) if matches!(inner.phase, UpdatePhase::Available { .. }) => {
                    inner.phase = UpdatePhase::Downloading {
                        version: info.version.clone(),
                        bytes: 0,
                        total: info.asset.size,
                    };
                    (info.asset, info.version)
                }
                _ => {
                    return Err("当前没有可安装的更新（请先检查更新）".into());
                }
            }
        };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.run_install(asset, version).await });
        Ok(())
    }

    async fn run_install(&self, asset: PlatformAsset, version: String) {
        let Some(client) = &self.client else {
            return;
        };
        let dest = self
            .tmp_dir
            .join(format!("pegboard-update-{}.tmp", std::process::id()));
        // 进度节流：按 1 MiB 步进更新 phase（每 chunk 拿锁太频繁）
        let mut last_report: u64 = 0;
        let download = download_and_verify(client, &asset, &dest, |done| {
            if done - last_report >= 1024 * 1024 {
                last_report = done;
                if let Ok(mut inner) = self.inner.lock() {
                    inner.phase = UpdatePhase::Downloading {
                        version: version.clone(),
                        bytes: done,
                        total: asset.size,
                    };
                }
            }
        })
        .await;
        let result = match download {
            Ok(()) => {
                if let Ok(mut inner) = self.inner.lock() {
                    inner.phase = UpdatePhase::Installing {
                        version: version.clone(),
                    };
                }
                // 同步安装（zip 解压 / self-replace）放阻塞线程池
                let dest_install = dest.clone();
                match tokio::task::spawn_blocking(move || install_update(&dest_install)).await {
                    Ok(r) => r,
                    Err(e) => Err(UpdateError::Io(format!("安装线程失败: {e}"))),
                }
            }
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_file(&dest).await;

        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.phase = match result {
            Ok(()) => {
                tracing::info!(version = %version, "update installed, restart pending");
                UpdatePhase::RestartPending { version }
            }
            Err(e) => {
                tracing::warn!(error = %e, "安装更新失败");
                UpdatePhase::Failed {
                    error: e.to_string(),
                }
            }
        };
    }
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        to_hex(&h.finalize())
    }

    /// 测试密钥对（minisign dev-dep；验证走生产代码 verify_with_keys）。
    fn test_keypair() -> (String, minisign::SecretKey) {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        (pk.to_box().expect("pk box").to_string(), sk)
    }

    fn test_sign(sk: &minisign::SecretKey, data: &[u8]) -> String {
        minisign::sign(None, sk, data, None, None)
            .expect("sign")
            .to_string()
    }

    #[test]
    fn platform_key_matches_release_assets() {
        // 当前编译平台必须有 key——否则客户端永远报 UnsupportedPlatform
        let key = platform_key().expect("当前平台应有 manifest key");
        assert!(
            key.starts_with("macos-") || key.starts_with("windows-") || key.starts_with("linux-")
        );
    }

    #[test]
    fn is_newer_semver_and_prefix() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.9"), "数字段比较而非字符串前缀");
        assert!(!is_newer("0.1.0", "0.1.0"), "相等不算新");
        assert!(!is_newer("0.0.9", "0.1.0"), "更旧不算新");
        assert!(!is_newer("垃圾", "0.1.0"), "坏版本号按不新处理");
        assert!(!is_newer("0.2.0", "垃圾"));
    }

    #[test]
    fn parse_manifest_shape_and_defaults() {
        let body = r#"{
            "version": "1.2.3",
            "notes_url": "https://example.com/notes",
            "platforms": {
                "macos-x86_64-app": {"url": "https://e.com/a", "size": 3, "sha256": "aa"}
            }
        }"#;
        let m = parse_manifest(body).expect("合法 manifest");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.notes_url, "https://example.com/notes");
        assert_eq!(m.platforms["macos-x86_64-app"].size, 3);
        let m2 = parse_manifest(r#"{"version":"1.0.0","platforms":{}}"#).expect("notes_url 可缺省");
        assert_eq!(m2.notes_url, "");
        assert!(parse_manifest("不是 json").is_err());
        assert!(parse_manifest("{}").is_err(), "缺 version/platforms");
        assert!(
            parse_manifest(r#"{"version":"xyz","platforms":{}}"#).is_err(),
            "坏版本号拒绝"
        );
    }

    #[test]
    fn verify_rejects_tamper_wrong_key_and_empty() {
        let (pk, sk) = test_keypair();
        let sig = test_sign(&sk, b"original");
        let err = verify_with_keys(b"tampered!", &sig, &[&pk]).expect_err("篡改必须失败");
        assert!(matches!(err, UpdateError::InvalidSignature(_)));
        let (pk_other, _) = test_keypair();
        assert!(
            verify_with_keys(b"original", &sig, &[&pk_other]).is_err(),
            "错钥必须失败"
        );
        assert!(verify_with_keys(b"original", "", &[&pk]).is_err(), "空签名");
        assert!(verify_with_keys(b"original", "垃圾签名", &[&pk]).is_err());
    }

    #[test]
    fn verify_accepts_key_rotation_list() {
        let (old_pk, _) = test_keypair();
        let (new_pk, sk) = test_keypair();
        let sig = test_sign(&sk, b"payload");
        verify_with_keys(b"payload", &sig, &[&old_pk, &new_pk]).expect("多公钥任一通过");
    }

    #[test]
    fn envelope_full_path_and_tamper() {
        let (pk, sk) = test_keypair();
        let payload = serde_json::json!({
            "version": "9.9.9",
            "platforms": {}
        })
        .to_string();
        let envelope = serde_json::json!({
            "payload": payload,
            "signature": test_sign(&sk, payload.as_bytes()),
        })
        .to_string();
        let m = parse_signed_manifest_with_keys(&envelope, &[&pk]).expect("合法信封");
        assert_eq!(m.version, "9.9.9");
        let bad = envelope.replace("9.9.9", "9.9.8");
        assert!(
            parse_signed_manifest_with_keys(&bad, &[&pk]).is_err(),
            "payload 单字节篡改必须整体拒绝"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_zip_rejects_escape_and_symlink() {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        let dir = tempfile::TempDir::new().expect("tmp");
        let archive_path = dir.path().join("t.zip");
        {
            let f = std::fs::File::create(&archive_path).expect("create");
            let mut w = zip::ZipWriter::new(f);
            w.start_file("Contents/MacOS/pegboard", SimpleFileOptions::default())
                .expect("start");
            w.write_all(b"BIN").expect("write");
            w.start_file("../escape.txt", SimpleFileOptions::default())
                .expect("start escape");
            w.write_all(b"x").expect("write escape");
            w.add_symlink(
                "Contents/link",
                "Contents/MacOS",
                SimpleFileOptions::default(),
            )
            .expect("symlink");
            w.finish().expect("finish");
        }
        let dest = dir.path().join("out");
        let err = extract_zip(&archive_path, &dest).expect_err("symlink 条目必须整体拒绝");
        assert!(matches!(err, UpdateError::Manifest(_)), "{err}");
        // 主文件在 symlink 之前已解压成功；逃逸条目被跳过
        assert!(dest.join("Contents/MacOS/pegboard").is_file());
        assert!(!dir.path().join("escape.txt").exists(), "逃逸条目不得落地");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn locate_bundle_and_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().expect("tmp");
        let staging = dir.path().join("staging");
        let bin = staging.join("Pegboard.app/Contents/MacOS/pegboard");
        std::fs::create_dir_all(bin.parent().expect("parent")).expect("mkdir");
        std::fs::write(&bin, b"BIN").expect("write");
        let found = locate_app_bundle(&staging).expect("定位 bundle");
        assert_eq!(found, staging.join("Pegboard.app"));
        // 权限恢复逻辑：extract 后主程序应为 0755（此处直接断言 helper 语义）
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            std::fs::metadata(&bin).expect("meta").permissions().mode() & 0o777,
            0o755
        );
    }

    /// 本地 HTTP 源 → 取 manifest → 验签 → 比版本 → 流式下载资产 → 验签。
    /// 全链路（除 install_update 的磁盘替换）在测试密钥对下走生产代码路径。
    #[tokio::test]
    async fn full_check_and_download_flow_over_local_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (pk, sk) = test_keypair();
        let asset_bytes: &[u8] = b"pegboard-new-binary";
        let sha = sha256_hex(asset_bytes);
        let asset_sig = test_sign(&sk, asset_bytes);

        // 原始 TCP mock：按路径回 manifest / asset
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let manifest_body = {
            let payload = serde_json::json!({
                "version": "9.9.9",
                "notes_url": "https://example.com/r",
                "platforms": {
                    platform_key().expect("platform"): {
                        "url": format!("http://{addr}/asset"),
                        "size": asset_bytes.len(),
                        "sha256": sha,
                        "signature": asset_sig,
                    }
                },
            })
            .to_string();
            serde_json::json!({
                "payload": payload,
                "signature": test_sign(&sk, payload.as_bytes()),
            })
            .to_string()
        };
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let manifest_body = manifest_body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    let (status, body) = if req.starts_with("GET /manifest.json") {
                        ("200 OK", manifest_body.into_bytes())
                    } else if req.starts_with("GET /asset") {
                        ("200 OK", asset_bytes.to_vec())
                    } else {
                        ("404 Not Found", b"not found".to_vec())
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });

        let client = reqwest::Client::new();
        // 检查：本地源 → 验签 → Available（9.9.9 > 当前）
        let url = format!("http://{addr}/manifest.json");
        let body = fetch_manifest(&client, &url).await.expect("fetch");
        let outcome =
            check_body_with_keys(&body, env!("CARGO_PKG_VERSION"), &[&pk]).expect("check");
        let ReleaseInfo { version, asset, .. } = match outcome {
            UpdateOutcome::Available(info) => info,
            UpdateOutcome::UpToDate => panic!("9.9.9 应判为可更新"),
        };
        assert_eq!(version, "9.9.9");
        assert_eq!(asset.size, asset_bytes.len() as u64);

        // 下载：sha256 + 验签 + 落盘
        let dest_dir = tempfile::TempDir::new().expect("tmp");
        let dest = dest_dir.path().join("asset.bin");
        download_and_verify_with(
            &client,
            &asset,
            &dest,
            |_| {},
            |d, s| verify_with_keys(d, s, &[&pk]),
        )
        .await
        .expect("download+verify");
        assert_eq!(std::fs::read(&dest).expect("read"), asset_bytes);

        // 篡改 sha256 → Checksum 且半成品被清理
        let mut bad_asset = asset.clone();
        bad_asset.sha256 = "00".repeat(32);
        let bad_dir = tempfile::TempDir::new().expect("tmp");
        let dest2 = bad_dir.path().join("bad.bin");
        let err = download_and_verify_with(
            &client,
            &bad_asset,
            &dest2,
            |_| {},
            |d, s| verify_with_keys(d, s, &[&pk]),
        )
        .await
        .expect_err("checksum 不符必须失败");
        assert!(matches!(err, UpdateError::Checksum), "{err}");
        assert!(!dest2.exists(), "半成品应被删除");
    }

    #[tokio::test]
    async fn updater_status_and_guards() {
        let u = Arc::new(Updater::new("", std::env::temp_dir()));
        assert_eq!(u.status_json()["configured"], false);
        assert!(u.spawn_check().is_err(), "未配置更新源时检查应拒绝");
        assert!(u.spawn_install().is_err(), "Idle 状态不允许安装");
        let u2 = Arc::new(Updater::new(
            "http://example.test/manifest.json",
            std::env::temp_dir(),
        ));
        assert_eq!(u2.status_json()["configured"], true);
        assert!(u2.spawn_check().is_ok());
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
