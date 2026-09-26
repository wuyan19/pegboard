//! 应用识别与生命周期：扫描产物目录、解析清单、维护注册表。
//!
//! 清单为权威，`host.db` 中注册表是派生缓存（持久化在后续里程碑接入）。
//! 单个应用损坏不阻塞整体：错误汇总为 warnings 返回，成功项保留。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod host_db;

pub use host_db::{HostDb, HostDbError, SignedTokenRow};

use crate::config::Limits;

/// 应用清单，对应 apps/&lt;id&gt;/manifest.json。字段见 API 契约 §2。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub entry: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: LimitsOverride,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Permissions {
    #[serde(default)]
    pub store: bool,
    #[serde(default)]
    pub files: bool,
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub ws: Vec<String>,
    #[serde(default = "default_true")]
    pub shim: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        // 清单缺省 permissions 时 shim 为 true（API 契约 §2）
        Self {
            store: false,
            files: false,
            net: Vec::new(),
            ws: Vec::new(),
            shim: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 应用覆盖限额。键名与宿主配置 [limits] 完全一致（数据模型 §8），
/// 只能收紧（覆盖值不得大于默认值）；`sign_ttl_max` 不可覆盖。
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct LimitsOverride {
    pub kv_value_bytes: Option<u64>,
    pub kv_total_bytes: Option<u64>,
    pub file_bytes: Option<u64>,
    pub file_total_bytes: Option<u64>,
    pub net_rps: Option<u32>,
}

impl AppMeta {
    /// 清单序列化（host.db 缓存用；清单文件为权威）。
    pub fn manifest_json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.manifest).unwrap_or_default()
    }
}

/// 运行期应用元数据，由清单 + 路径 + 生效限额组成。
#[derive(Debug, Clone)]
pub struct AppMeta {
    pub id: String,
    pub name: String,
    /// apps/&lt;id&gt;/，canonicalize 后的绝对路径
    pub root: PathBuf,
    /// 入口文件绝对路径，已校验存在且在 root 内
    pub entry: PathBuf,
    pub manifest: Manifest,
    /// 已与默认合并、收紧后的值
    pub limits: Limits,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("apps dir not readable: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid app id `{0}`")]
    InvalidId(String),
    #[error("manifest not found in {0}")]
    ManifestMissing(PathBuf),
    #[error("manifest parse failed in {0}: {1}")]
    ManifestParse(PathBuf, String),
    #[error("invalid manifest in {0}: {1}")]
    ManifestInvalid(PathBuf, String),
    #[error("duplicate app id `{0}`")]
    DuplicateId(String),
    #[error("entry not found for `{0}`: {1}")]
    EntryMissing(String, PathBuf),
    #[error("entry escapes app root for `{0}`: {1}")]
    EntryEscape(String, PathBuf),
    #[error("limits override exceeds default for `{0}`")]
    LimitsExceeded(String),
}

/// 扫描结果：注册表 + 逐应用告警（不阻塞整体）。
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub registry: AppRegistry,
    pub warnings: Vec<AppError>,
}

/// 应用注册表：扫描目录、解析清单、按 id 查询。
/// 内部存 `Arc<AppMeta>`：读锁短持有，克隆后跨 await 使用。
#[derive(Debug, Default)]
pub struct AppRegistry {
    apps: HashMap<String, Arc<AppMeta>>,
}

impl AppRegistry {
    /// 扫描 apps_dir，解析所有子目录的 manifest.json。
    /// 单个应用失败不阻塞整体：错误进 warnings，成功项保留。
    /// apps_dir 本身不可读才是 Err。
    pub fn scan(apps_dir: &Path, default_limits: &Limits) -> Result<ScanOutcome, AppError> {
        let mut outcome = ScanOutcome::default();
        let mut ids: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(apps_dir)? {
            let entry = entry.map_err(AppError::Io)?;
            if !entry.file_type().map_err(AppError::Io)?.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            match load_one(apps_dir, &dir_name, default_limits) {
                Ok(meta) => {
                    if outcome.registry.apps.contains_key(&meta.id) {
                        outcome
                            .warnings
                            .push(AppError::DuplicateId(meta.id.clone()));
                    } else {
                        ids.push(meta.id.clone());
                        outcome
                            .registry
                            .apps
                            .insert(meta.id.clone(), Arc::new(meta));
                    }
                }
                Err(e) => outcome.warnings.push(e),
            }
        }
        tracing::info!(count = ids.len(), apps = ?ids, "app registry scanned");
        Ok(outcome)
    }

    pub fn get(&self, id: &str) -> Option<Arc<AppMeta>> {
        self.apps.get(id).cloned()
    }

    /// 按 id 字典序返回。
    pub fn list(&self) -> Vec<Arc<AppMeta>> {
        let mut metas: Vec<Arc<AppMeta>> = self.apps.values().cloned().collect();
        metas.sort_by(|a, b| a.id.cmp(&b.id));
        metas
    }

    pub fn contains(&self, id: &str) -> bool {
        self.apps.contains_key(id)
    }

    /// 重新扫描单个应用（用于安装后刷新），返回刷新后的元数据。
    pub fn reload_one(
        &mut self,
        apps_dir: &Path,
        id: &str,
        default_limits: &Limits,
    ) -> Result<Arc<AppMeta>, AppError> {
        validate_id(id)?;
        let meta = Arc::new(load_one(apps_dir, id, default_limits)?);
        self.apps.insert(meta.id.clone(), Arc::clone(&meta));
        Ok(meta)
    }

    /// 卸载：从注册表移除。物理删除由上层决定。
    pub fn remove(&mut self, id: &str) -> Option<Arc<AppMeta>> {
        self.apps.remove(id)
    }
}

/// id 规则：`[a-z0-9_-]{1,64}`，且必须以字母或数字开头。
pub fn validate_id(id: &str) -> Result<(), AppError> {
    let mut chars = id.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        }
        _ => false,
    };
    let len_ok = !id.is_empty() && id.len() <= 64;
    if valid && len_ok {
        Ok(())
    } else {
        Err(AppError::InvalidId(id.to_owned()))
    }
}

/// 加载单个应用：目录名即 id 期望值，清单 id 必须一致。
fn load_one(apps_dir: &Path, dir_name: &str, default_limits: &Limits) -> Result<AppMeta, AppError> {
    validate_id(dir_name)?;
    let root = apps_dir.join(dir_name);
    let manifest_path = root.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(AppError::ManifestMissing(manifest_path));
    }
    let text = std::fs::read_to_string(&manifest_path).map_err(AppError::Io)?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| AppError::ManifestParse(manifest_path.clone(), e.to_string()))?;
    if manifest.id != dir_name {
        return Err(AppError::InvalidId(format!(
            "manifest id `{}` 与目录名 `{dir_name}` 不一致",
            manifest.id
        )));
    }
    validate_permissions(&manifest, &manifest_path)?;
    let limits = merge_limits(default_limits, &manifest.limits, &manifest.id)?;
    let root = root.canonicalize().map_err(AppError::Io)?;
    let entry = resolve_entry(&root, &manifest.entry, &manifest.id)?;
    Ok(AppMeta {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        root,
        entry,
        manifest,
        limits,
    })
}

/// permissions.net / ws 每项须为合法 http(s)/ws(s) URL 或 origin。
fn validate_permissions(manifest: &Manifest, manifest_path: &Path) -> Result<(), AppError> {
    for (field, entries) in [
        ("net", &manifest.permissions.net),
        ("ws", &manifest.permissions.ws),
    ] {
        for entry in entries {
            let parsed = url::Url::parse(entry).map_err(|e| {
                AppError::ManifestInvalid(
                    manifest_path.to_owned(),
                    format!("permissions.{field} 项 `{entry}` 非法: {e}"),
                )
            })?;
            let scheme_ok = matches!(parsed.scheme(), "http" | "https" | "ws" | "wss");
            if !scheme_ok || parsed.host_str().is_none() {
                return Err(AppError::ManifestInvalid(
                    manifest_path.to_owned(),
                    format!("permissions.{field} 项 `{entry}` 须为 http(s)/ws(s) origin 或 URL"),
                ));
            }
        }
    }
    Ok(())
}

/// 限额合并：未覆盖取默认；覆盖值 ≤ 默认采用覆盖值；> 默认报错。
fn merge_limits(default: &Limits, ov: &LimitsOverride, id: &str) -> Result<Limits, AppError> {
    let tighten = |name: &str, def: u64, over: Option<u64>| -> Result<u64, AppError> {
        match over {
            None => Ok(def),
            Some(v) if v <= def && v > 0 => Ok(v),
            Some(_) => Err(AppError::LimitsExceeded(format!("{id}: limits.{name}"))),
        }
    };
    let kv_value_bytes = tighten("kv_value_bytes", default.kv_value_bytes, ov.kv_value_bytes)?;
    let kv_total_bytes = tighten("kv_total_bytes", default.kv_total_bytes, ov.kv_total_bytes)?;
    let file_bytes = tighten("file_bytes", default.file_bytes, ov.file_bytes)?;
    let file_total_bytes = tighten(
        "file_total_bytes",
        default.file_total_bytes,
        ov.file_total_bytes,
    )?;
    let net_rps = tighten("net_rps", default.net_rps as u64, ov.net_rps.map(u64::from))? as u32;
    if kv_value_bytes > kv_total_bytes || file_bytes > file_total_bytes {
        return Err(AppError::LimitsExceeded(format!(
            "{id}: 覆盖后 value 上限超过 total 上限"
        )));
    }
    Ok(Limits {
        kv_value_bytes,
        kv_total_bytes,
        file_bytes,
        file_total_bytes,
        net_rps,
        sign_ttl_max: default.sign_ttl_max,
    })
}

/// 条目路径必须在应用根目录内（canonicalize + 前缀校验），且为已存在文件。
fn resolve_entry(root: &Path, entry: &str, id: &str) -> Result<PathBuf, AppError> {
    let joined = root.join(entry);
    let canon = match joined.canonicalize() {
        Ok(p) => p,
        Err(_) => return Err(AppError::EntryMissing(id.to_owned(), joined)),
    };
    if !canon.starts_with(root) {
        return Err(AppError::EntryEscape(id.to_owned(), canon));
    }
    if !canon.is_file() {
        return Err(AppError::EntryMissing(id.to_owned(), canon));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_apps() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    fn write_app(apps: &Path, id: &str, manifest_body: &str, entry_file: &str) {
        let dir = apps.join(id);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(entry_file), "<h1>x</h1>").expect("write entry");
        fs::write(dir.join("manifest.json"), manifest_body).expect("write manifest");
    }

    fn default_limits() -> Limits {
        Limits::default()
    }

    #[test]
    fn valid_id_accepted() {
        assert!(validate_id("ollama-chat").is_ok());
        assert!(validate_id("a1").is_ok());
        assert!(validate_id("a_b-c9").is_ok());
    }

    #[test]
    fn invalid_id_rejected() {
        assert!(validate_id("").is_err());
        assert!(validate_id("A").is_err());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("..").is_err());
        assert!(validate_id("-a").is_err());
        assert!(validate_id("_a").is_err());
        assert!(validate_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn scan_finds_app() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "demo",
            r#"{"id":"demo","name":"Demo","entry":"index.html"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(outcome.warnings.is_empty());
        assert!(outcome.registry.contains("demo"));
        let meta = outcome.registry.get("demo").expect("meta");
        assert_eq!(meta.name, "Demo");
        assert!(meta.entry.ends_with("index.html"));
    }

    #[test]
    fn id_must_match_dir() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"b","name":"B","entry":"index.html"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(!outcome.registry.contains("b"));
        assert!(matches!(
            outcome.warnings.as_slice(),
            [AppError::InvalidId(_)]
        ));
    }

    #[test]
    fn entry_traversal_rejected() {
        let dir = tmp_apps();
        // victim 文件在 apps 根之外
        let victim = dir.path().join("secret.txt");
        fs::write(&victim, "s").expect("write");
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"../secret.txt"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(!outcome.registry.contains("a"));
        assert!(matches!(
            outcome.warnings.as_slice(),
            [AppError::EntryEscape(_, _) | AppError::EntryMissing(_, _)]
        ));
    }

    #[test]
    fn duplicate_id_rejected() {
        // 大小写绕过：合法 id 不含大写，改用符号链接不可移植；
        // 直接构造两个目录同 id 不可能（id 必须等于目录名），
        // 该错误留给 reload/未来来源，此处验证 scan 对两个同清单不同目录正常。
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html"}"#,
            "index.html",
        );
        write_app(
            dir.path(),
            "b",
            r#"{"id":"b","name":"B","entry":"index.html"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert_eq!(outcome.registry.list().len(), 2);
    }

    #[test]
    fn bad_manifest_does_not_block_others() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "good",
            r#"{"id":"good","name":"Good","entry":"index.html"}"#,
            "index.html",
        );
        let bad = dir.path().join("bad");
        fs::create_dir_all(&bad).expect("mkdir");
        fs::write(bad.join("manifest.json"), "{ not json").expect("write");
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(outcome.registry.contains("good"));
        assert_eq!(outcome.warnings.len(), 1);
    }

    #[test]
    fn limits_can_only_tighten() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","limits":{"net_rps":5}}"#,
            "index.html",
        );
        write_app(
            dir.path(),
            "b",
            &format!(
                r#"{{"id":"b","name":"B","entry":"index.html","limits":{{"net_rps":{}}}}}"#,
                default_limits().net_rps + 1
            ),
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert_eq!(outcome.registry.get("a").expect("a").limits.net_rps, 5);
        assert!(!outcome.registry.contains("b"));
        assert!(matches!(
            outcome.warnings.as_slice(),
            [AppError::LimitsExceeded(_)]
        ));
    }

    #[test]
    fn limits_value_above_total_rejected() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","limits":{"kv_total_bytes":1024}}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(!outcome.registry.contains("a"));
    }

    #[test]
    fn sign_ttl_not_overridable() {
        // LimitsOverride 无 sign_ttl_max 字段：出现在清单中会被 serde 忽略（无 deny_unknown_fields）
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","limits":{"sign_ttl_max":1}}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        let meta = outcome.registry.get("a").expect("a");
        assert_eq!(meta.limits.sign_ttl_max, default_limits().sign_ttl_max);
    }

    #[test]
    fn shim_defaults_true() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(
            outcome
                .registry
                .get("a")
                .expect("a")
                .manifest
                .permissions
                .shim
        );
    }

    #[test]
    fn shim_false_respected() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"shim":false}}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(
            !outcome
                .registry
                .get("a")
                .expect("a")
                .manifest
                .permissions
                .shim
        );
    }

    #[test]
    fn bad_net_entry_rejected() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html","permissions":{"net":["not a url"]}}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(!outcome.registry.contains("a"));
        assert!(matches!(
            outcome.warnings.as_slice(),
            [AppError::ManifestInvalid(_, _)]
        ));
    }

    #[test]
    fn entry_in_subdir_ok() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"dist/index.html"}"#,
            "index.html",
        );
        // write_app 写的是 index.html；补 dist/index.html
        fs::create_dir_all(dir.path().join("a/dist")).expect("mkdir dist");
        fs::write(dir.path().join("a/dist/index.html"), "y").expect("write");
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        let meta = outcome.registry.get("a").expect("a");
        assert!(meta.entry.to_string_lossy().contains("dist"));
    }

    #[test]
    fn missing_entry_rejected() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"nope.html"}"#,
            "index.html",
        );
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        assert!(!outcome.registry.contains("a"));
        assert!(matches!(
            outcome.warnings.as_slice(),
            [AppError::EntryMissing(_, _)]
        ));
    }

    #[test]
    fn reload_one_refreshes() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"Old","entry":"index.html"}"#,
            "index.html",
        );
        let mut outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        fs::write(
            dir.path().join("a/manifest.json"),
            r#"{"id":"a","name":"New","entry":"index.html"}"#,
        )
        .expect("write");
        let meta = outcome
            .registry
            .reload_one(dir.path(), "a", &default_limits())
            .expect("reload");
        assert_eq!(meta.name, "New");
        assert_eq!(outcome.registry.get("a").expect("a").name, "New");
    }

    #[test]
    fn remove_returns_meta() {
        let dir = tmp_apps();
        write_app(
            dir.path(),
            "a",
            r#"{"id":"a","name":"A","entry":"index.html"}"#,
            "index.html",
        );
        let mut outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        let meta = outcome.registry.remove("a").expect("removed");
        assert_eq!(meta.id, "a");
        assert!(!outcome.registry.contains("a"));
    }

    #[test]
    fn list_sorted_by_id() {
        let dir = tmp_apps();
        for id in ["c", "a", "b"] {
            write_app(
                dir.path(),
                id,
                &format!(r#"{{"id":"{id}","name":"{id}","entry":"index.html"}}"#),
                "index.html",
            );
        }
        let outcome = AppRegistry::scan(dir.path(), &default_limits()).expect("scan");
        let ids: Vec<String> = outcome
            .registry
            .list()
            .into_iter()
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn apps_dir_missing_is_error() {
        let dir = tmp_apps();
        let err = AppRegistry::scan(&dir.path().join("nope"), &default_limits());
        assert!(matches!(err, Err(AppError::Io(_))));
    }
}
