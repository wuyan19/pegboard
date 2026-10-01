//! 配置加载与校验。加载后只读；admin 页可在线修改 listen/limits（写回文件）。
//!
//! 来源优先级：默认 → 文件 → 环境变量（`PEGBOARD_` 前缀，`__` 分层）→ 命令行覆盖。
//! 命令行覆盖由 CLI 装配层应用到 `Config` 字段后重新 `validate`。
//!
//! 访问语义：listen 绑环回 = 仅本机；绑非环回 = 局域网开放（是否需要访问密码
//! 由 host.db 里的管理密码决定，不在此配置）。身份 subject 固定为内部值，
//! 不再是用户配置。

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 只读配置，加载后不再变更（admin 在线修改经运行时快照替换）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub limits: Limits,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8787),
        }
    }
}

impl ServerConfig {
    /// listen 是否把服务暴露到本机之外（非环回地址）。
    pub fn is_lan_exposed(&self) -> bool {
        !self.listen.ip().is_loopback()
    }
}

/// 限额默认值。键名与应用清单 limits 完全一致（数据模型 §8），
/// 清单只能收紧；`sign_ttl_max` 仅宿主配置，不可被清单覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    pub kv_value_bytes: u64,
    pub kv_total_bytes: u64,
    pub file_bytes: u64,
    pub file_total_bytes: u64,
    pub net_rps: u32,
    pub sign_ttl_max: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            kv_value_bytes: 1_048_576,
            kv_total_bytes: 10_485_760,
            file_bytes: 104_857_600,
            file_total_bytes: 1_073_741_824,
            net_rps: 20,
            sign_ttl_max: 86_400,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// 数据根：apps_data/、logs/、tmp/、host.db 的父目录。
    pub data_root: PathBuf,
    /// 应用产物目录，与数据目录分离。
    pub apps_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_root: PathBuf::from("./data"),
            apps_dir: PathBuf::from("./apps"),
        }
    }
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

/// 加载顺序：默认 → 文件 → 环境变量。
/// 命令行覆盖项由 CLI 在加载后应用并重新校验。
pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
    let mut value = match path {
        Some(p) => {
            let text = std::fs::read_to_string(p)?;
            text.parse::<toml::Value>()?
        }
        None => toml::Value::Table(toml::map::Map::new()),
    };
    apply_env(&mut value)?;
    let config = Config::deserialize(value)?;
    validate(&config)?;
    Ok(config)
}

/// 校验：限额自洽。
pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let l = &cfg.limits;
    for (name, v) in [
        ("kv_value_bytes", l.kv_value_bytes),
        ("kv_total_bytes", l.kv_total_bytes),
        ("file_bytes", l.file_bytes),
        ("file_total_bytes", l.file_total_bytes),
        ("net_rps", l.net_rps as u64),
        ("sign_ttl_max", l.sign_ttl_max),
    ] {
        if v == 0 {
            return Err(ConfigError::Invalid(format!("limits.{name} 必须 > 0")));
        }
    }
    if l.kv_total_bytes < l.kv_value_bytes {
        return Err(ConfigError::Invalid(
            "limits.kv_total_bytes 必须 >= kv_value_bytes".into(),
        ));
    }
    if l.file_total_bytes < l.file_bytes {
        return Err(ConfigError::Invalid(
            "limits.file_total_bytes 必须 >= file_bytes".into(),
        ));
    }
    Ok(())
}

/// 环境变量合并：`PEGBOARD_` 前缀，`__` 作为层级分隔，
/// 如 `PEGBOARD_SERVER__LISTEN` → server.listen、`PEGBOARD_LIMITS__NET_RPS` → limits.net_rps。
fn apply_env(value: &mut toml::Value) -> Result<(), ConfigError> {
    for (key, raw) in std::env::vars() {
        let Some(rest) = key.strip_prefix("PEGBOARD_") else {
            continue;
        };
        if rest.is_empty() || rest.contains("__/") {
            continue;
        }
        let segments: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(ConfigError::Invalid(format!("环境变量 {key} 含空段")));
        }
        let leaf = coerce_toml(&raw);
        insert_path(value, &segments, leaf)?;
    }
    Ok(())
}

/// 字符串值按 TOML 标量尝试归一，便于数值/布尔字段直接反序列化。
fn coerce_toml(raw: &str) -> toml::Value {
    if let Ok(b) = raw.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    toml::Value::String(raw.to_owned())
}

fn insert_path(
    value: &mut toml::Value,
    segments: &[String],
    leaf: toml::Value,
) -> Result<(), ConfigError> {
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| ConfigError::Invalid("环境变量路径为空".into()))?;
    let mut cur = value;
    for seg in parents {
        let table = match cur.as_table_mut() {
            Some(t) => t,
            None => {
                return Err(ConfigError::Invalid(format!(
                    "环境变量路径与配置结构冲突：{}",
                    segments.join("__")
                )))
            }
        };
        cur = table
            .entry(seg.clone())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    match cur.as_table_mut() {
        Some(table) => {
            table.insert(last.clone(), leaf);
            Ok(())
        }
        None => Err(ConfigError::Invalid(format!(
            "环境变量路径与配置结构冲突：{}",
            segments.join("__")
        ))),
    }
}

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
    fn parse_toml_fields_mapped() {
        let text = r#"
[server]
listen = "127.0.0.1:9000"

[storage]
data_root = "./d1"
apps_dir = "./a1"
"#;
        let cfg = load_from_str(text);
        assert_eq!(cfg.server.listen.port(), 9000);
        assert_eq!(cfg.storage.data_root, PathBuf::from("./d1"));
        assert_eq!(cfg.storage.apps_dir, PathBuf::from("./a1"));
        assert_eq!(cfg.limits, Limits::default());
    }

    #[test]
    fn defaults_when_empty_file() {
        let cfg = load_from_str("");
        assert_eq!(cfg.server.listen.port(), 8787);
        assert!(!cfg.server.is_lan_exposed());
    }

    #[test]
    fn lan_exposure_derived_from_listen() {
        let cfg = load_from_str("[server]\nlisten = \"0.0.0.0:8787\"\n");
        assert!(cfg.server.is_lan_exposed());
        let cfg = load_from_str("[server]\nlisten = \"192.168.1.5:8787\"\n");
        assert!(cfg.server.is_lan_exposed());
        let cfg = load_from_str("[server]\nlisten = \"127.0.0.1:8787\"\n");
        assert!(!cfg.server.is_lan_exposed());
    }

    #[test]
    fn unknown_fields_ignored() {
        // 旧版本遗留的 [update]、[identity]、mode 键不应导致解析失败
        let text = r#"
[server]
listen = "127.0.0.1:9000"
mode = "local"

[identity]
kind = "fixed"
subject = "x"

[update]
manifest_url = "https://example.com/m.json"
"#;
        let cfg = load_from_str(text);
        assert_eq!(cfg.server.listen.port(), 9000);
    }

    #[test]
    fn zero_limit_rejected() {
        let cfg = load_from_str("[limits]\nnet_rps = 0\n");
        assert!(matches!(validate(&cfg), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn total_below_value_rejected() {
        let cfg = load_from_str("[limits]\nkv_value_bytes = 100\nkv_total_bytes = 50\n");
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[server]\nlisten = \"127.0.0.1:8787\"\n").expect("write config");
        // 真实键名走完整 load 路径；本测试是唯一设置 PEGBOARD_* 变量的测试
        std::env::set_var("PEGBOARD_LIMITS__NET_RPS", "99");
        let cfg = load(Some(&path));
        std::env::remove_var("PEGBOARD_LIMITS__NET_RPS");
        let cfg = cfg.expect("load ok");
        assert_eq!(cfg.limits.net_rps, 99);
    }

    #[test]
    fn load_missing_file_errors() {
        let err = load(Some(Path::new("/nonexistent/pegboard-m1/config.toml")));
        assert!(matches!(err, Err(ConfigError::Read(_))));
    }

    #[test]
    fn bad_toml_is_parse_error() {
        let text = "[server\nlisten = 1";
        let v: Result<toml::Value, toml::de::Error> = text.parse();
        assert!(v.is_err());
    }

    fn load_from_str(text: &str) -> Config {
        let value: toml::Value = text.parse().expect("test toml valid");
        Config::deserialize(value).expect("test config valid")
    }
}
