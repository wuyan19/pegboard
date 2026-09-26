# pegboard-core / app 模块

## 接口

```rust
// crates/pegboard-core/src/app/mod.rs

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::config::Limits;

/// 应用清单，对应 apps/<id>/manifest.json。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub entry: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: LimitsOverride,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
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

fn default_true() -> bool { true }

/// 应用覆盖限额，只能收紧，不能放宽。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct LimitsOverride {
    pub store_bytes: Option<u64>,
    pub file_bytes: Option<u64>,
    pub net_rps: Option<u32>,
}

/// 运行期应用元数据，由清单 + 路径 + 生效限额组成。
#[derive(Debug, Clone)]
pub struct AppMeta {
    pub id: String,
    pub name: String,
    pub root: PathBuf,       // apps/<id>/
    pub entry: PathBuf,      // 绝对路径，已校验存在
    pub manifest: Manifest,
    pub limits: Limits,      // 已与默认合并、收紧后的值
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
    #[error("duplicate app id `{0}`")]
    DuplicateId(String),
    #[error("entry not found for `{0}`: {1}")]
    EntryMissing(String, PathBuf),
    #[error("limits override exceeds default for `{0}`")]
    LimitsExceeded(String),
}

/// 应用注册表：扫描目录、解析清单、按 id 查询。
#[derive(Debug, Default)]
pub struct AppRegistry {
    apps: HashMap<String, AppMeta>,
}

impl AppRegistry {
    /// 扫描 apps_dir，解析所有子目录的 manifest.json。
    /// 单个应用失败不阻塞整体；错误汇总返回，成功项保留。
    pub fn scan(apps_dir: &Path, default_limits: &Limits) -> Result<Self, Vec<AppError>>;

    pub fn get(&self, id: &str) -> Option<&AppMeta>;
    pub fn list(&self) -> Vec<&AppMeta>;
    pub fn contains(&self, id: &str) -> bool;

    /// 重新扫描单个应用（用于安装后刷新）。
    pub fn reload_one(&mut self, apps_dir: &Path, id: &str, default_limits: &Limits)
        -> Result<&AppMeta, AppError>;

    /// 卸载：从注册表移除。物理删除由上层决定。
    pub fn remove(&mut self, id: &str) -> Option<AppMeta>;
}

/// id 规则：`[a-z0-9_-]{1,64}`，且必须以字母或数字开头。
pub fn validate_id(id: &str) -> Result<(), AppError>;

/// 条目路径必须在应用根目录内，禁止 `..` 越界。
fn resolve_entry(root: &Path, entry: &str) -> Result<PathBuf, AppError>;
```

## 目录约定

```
apps/
  <app_id>/
    manifest.json
    dist/index.html          或
    index.html
```

- `app_id` 只允许 `[a-z0-9_-]`，长度 1–64，首字符为字母或数字。
- `entry` 相对 `root`，解析后必须在 `root` 内。
- 同一 `id` 重复出现视为错误，扫描时拒绝后者。

## 校验规则

- 清单必填：`id`、`name`、`entry`。
- `id` 必须与目录名一致，避免伪造。
- `entry` 解析后必须落在 `root` 内且文件存在。
- `permissions.net` / `ws` 每项须为合法 URL 或 origin。
- `limits` 覆盖值不得大于默认值，否则 `LimitsExceeded`。
- `shim` 默认 `true`。

## 限额合并

```rust
fn merge_limits(default: &Limits, ov: &LimitsOverride, id: &str) -> Result<Limits, AppError>;
```

规则：
- 未覆盖 → 取默认。
- 覆盖值 ≤ 默认 → 采用覆盖值。
- 覆盖值 > 默认 → 报错。

## 单元测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_apps() -> tempfile::TempDir { /* ... */ }

    #[test]
    fn valid_id_accepted() {
        assert!(validate_id("ollama-chat").is_ok());
        assert!(validate_id("a1").is_ok());
    }

    #[test]
    fn invalid_id_rejected() {
        assert!(validate_id("").is_err());
        assert!(validate_id("A").is_err());
        assert!(validate_id("a/b").is_err());
        assert!(validate_id("..").is_err());
        assert!(validate_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn scan_finds_app() {
        // 写入合法 manifest + index.html，断言注册表包含
    }

    #[test]
    fn id_must_match_dir() {
        // 目录名 a，manifest id b，断言报错
    }

    #[test]
    fn entry_traversal_rejected() {
        // entry = "../../etc/passwd"，断言 EntryMissing 或 InvalidId
    }

    #[test]
    fn duplicate_id_rejected() {
        // 两个目录同 id（大小写或符号绕过），断言后者被拒
    }

    #[test]
    fn bad_manifest_does_not_block_others() {
        // 一个坏 manifest + 一个好 app，断言好 app 仍被注册
    }

    #[test]
    fn limits_can_only_tighten() {
        // 覆盖值 > 默认 → 报错；覆盖值 ≤ 默认 → 生效
    }

    #[test]
    fn shim_defaults_true() {
        // manifest 未写 shim，断言 permissions.shim == true
    }

    #[test]
    fn reload_one_refreshes() {
        // 修改 manifest 后 reload，断言字段更新
    }

    #[test]
    fn remove_returns_meta() {
        // 注册后 remove，断言返回 Some 且 contains == false
    }
}
```

## 依赖

```toml
[dependencies]
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
thiserror = { workspace = true }
url = { workspace = true }        # 校验 net/ws 项

[dev-dependencies]
tempfile = { workspace = true }
```

## 设计要点

- **扫描容错**：单个应用损坏不阻断整体，错误汇总返回；调用方决定是否告警。
- **路径安全**：`resolve_entry` 用 `canonicalize` + 前缀检查，防 `..` 越界。
- **限额收紧单向**：应用只能减小，不能扩大；这是治理层默认值的安全保证。
- **注册表不落盘**：`host.db` 是派生缓存，清单为权威；`scan` 结果可覆盖。
- **不触网、不读写业务数据**：仅文件系统 + 清单解析。
-