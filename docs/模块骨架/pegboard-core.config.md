# pegboard-core / config 模块

## 接口

```rust
// crates/pegboard-core/src/config/mod.rs

use std::net::SocketAddr;
use std::path::PathBuf;

/// 只读配置，加载后不再变更。
#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub identity: IdentityConfig,
    pub limits: Limits,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 仅本机访问。
    Local,
    /// 内网访问，需身份来源。
    Lan,
}

#[derive(Debug, Clone)]
pub enum IdentityConfig {
    /// 固定 subject，可为 None。
    Fixed(Option<String>),
    /// 信任来自指定代理 IP 的 X-Forwarded-User。
    Forwarded { trusted: Vec<std::net::IpAddr> },
    /// 手工 token → subject。
    Tokens(std::collections::HashMap<String, String>),
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub kv_value_bytes: u64,
    pub kv_total_bytes: u64,
    pub file_bytes: u64,
    pub file_total_bytes: u64,
    pub net_rps: u32,
    pub sign_ttl_max: u64,
}

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub data_root: PathBuf,
}

impl Default for Limits {
    fn default() -> Self { /* 常量默认值 */ }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// 加载顺序：默认 → 文件 → 环境变量 → 命令行覆盖项。
pub fn load(path: Option<&std::path::Path>) -> Result<Config, ConfigError>;

/// 校验：模式与身份来源匹配、路径可建、限额自洽。
fn validate(cfg: &Config) -> Result<(), ConfigError>;
```

## 落盘结构

```toml
[server]
listen = "127.0.0.1:8787"
mode = "local"

[identity]
kind = "fixed"
subject = "local"

[limits]
kv_value_bytes = 1048576
kv_total_bytes = 10485760
file_bytes = 104857600
file_total_bytes = 1073741824
net_rps = 20
sign_ttl_max = 86400

[storage]
data_root = "./data"
```

环境变量前缀 `PEGBOARD_`，如 `PEGBOARD_SERVER__LISTEN`。

## 校验规则

- `Mode::Lan` 时 `IdentityConfig::Fixed(None)` 视为配置错误，避免匿名多用户。
- `data_root` 不存在则创建；不可写即失败。
- `limits` 各值 > 0，且 `*_total >= *_value`。
- `Forwarded.trusted` 为空视为错误，避免信任任意来源。
- `Tokens` 为空允许，但 `Mode::Lan` 下会告警。

## 单元测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_are_positive() {
        let l = Limits::default();
        assert!(l.kv_value_bytes > 0);
        assert!(l.kv_total_bytes >= l.kv_value_bytes);
        assert!(l.file_total_bytes >= l.file_bytes);
    }

    #[test]
    fn lan_requires_identity() {
        let cfg = /* Mode::Lan + Fixed(None) */;
        assert!(matches!(validate(&cfg), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn fixed_subject_allowed_in_local() {
        let cfg = /* Mode::Local + Fixed(None) */;
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn forwarded_requires_trusted() {
        let cfg = /* Forwarded { trusted: vec![] } */;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn parse_toml_roundtrip() {
        // 从内联 TOML 加载，断言字段映射正确
    }

    #[test]
    fn env_overrides_file() {
        // 设置 PEGBOARD_ 前缀环境变量，断言覆盖生效
    }

    #[test]
    fn data_root_created() {
        // 临时目录下加载，断言目录被创建
    }
}
```

## 依赖

```toml
[dependencies]
serde = { workspace = true, features = ["derive"] }
toml = { workspace = true }
thiserror = { workspace = true }
```

`figment` 可后置；先用 `toml` + 手工环境变量合并，减少依赖面。
