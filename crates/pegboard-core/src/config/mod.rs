//! 配置加载与校验。加载后只读，不做运行时热更新。
//!
//! 来源优先级：默认 → 文件 → 环境变量（`PEGBOARD_` 前缀，`__` 分层）→ 命令行覆盖。
//! 命令行覆盖由 CLI 装配层应用到 `Config` 字段后重新 `validate`。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 只读配置，加载后不再变更。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub identity: IdentityConfig,
    pub limits: Limits,
    pub storage: StorageConfig,
    pub update: UpdateConfig,
}

/// 在线升级配置。manifest_url 为空 = 更新通道关闭（v1 默认）；
/// 内容真实性由编译期内嵌公钥的 minisign 验签保证，URL 只决定去哪取。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub manifest_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub mode: Mode,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8787),
            mode: Mode::Local,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 仅本机访问。
    Local,
    /// 内网访问，需身份来源。
    Lan,
}

/// 身份来源。v1 部署锁定 fixed 模式，其余为后置能力。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IdentityConfig {
    /// 固定 subject，可为 None。
    Fixed {
        #[serde(default)]
        subject: Option<String>,
    },
    /// 信任来自指定代理 IP 的 X-Forwarded-User。
    Forwarded { trusted: Vec<IpAddr> },
    /// 手工 token → subject。
    Tokens {
        #[serde(default)]
        tokens: HashMap<String, String>,
    },
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self::Fixed { subject: None }
    }
}

impl IdentityConfig {
    /// 出站代理需剥离的身份注入头，与身份配置同清单。
    pub fn injected_headers(&self) -> Vec<&'static str> {
        match self {
            Self::Forwarded { .. } => vec!["x-forwarded-user"],
            _ => Vec::new(),
        }
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

#[derive(Debug, Clone, Deserialize)]
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

/// 校验：模式与身份来源匹配、限额自洽。
fn validate(cfg: &Config) -> Result<(), ConfigError> {
    if cfg.server.mode == Mode::Lan {
        if let IdentityConfig::Fixed { subject: None } = cfg.identity {
            return Err(ConfigError::Invalid(
                "mode=lan 需要非匿名身份来源（fixed.subject / forwarded / tokens）".into(),
            ));
        }
    }
    if let IdentityConfig::Forwarded { trusted } = &cfg.identity {
        if trusted.is_empty() {
            return Err(ConfigError::Invalid(
                "identity.kind=forwarded 需要 non-empty trusted".into(),
            ));
        }
    }
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
mode = "lan"

[identity]
kind = "tokens"
[identity.tokens]
abc = "alice"

[storage]
data_root = "./d1"
apps_dir = "./a1"
"#;
        let cfg = load_from_str(text);
        assert_eq!(cfg.server.listen.port(), 9000);
        assert_eq!(cfg.server.mode, Mode::Lan);
        assert!(matches!(cfg.identity, IdentityConfig::Tokens { .. }));
        assert_eq!(cfg.storage.data_root, PathBuf::from("./d1"));
        assert_eq!(cfg.storage.apps_dir, PathBuf::from("./a1"));
        assert_eq!(cfg.limits, Limits::default());
    }

    #[test]
    fn defaults_when_empty_file() {
        let cfg = load_from_str("");
        assert_eq!(cfg.server.listen.port(), 8787);
        assert_eq!(cfg.server.mode, Mode::Local);
        assert!(matches!(
            cfg.identity,
            IdentityConfig::Fixed { subject: None }
        ));
    }

    #[test]
    fn lan_requires_identity() {
        let cfg = load_from_str("[server]\nmode = \"lan\"\n");
        assert!(matches!(
            validate(&cfg),
            Err(ConfigError::Invalid(msg)) if msg.contains("身份")
        ));
    }

    #[test]
    fn lan_with_fixed_subject_ok() {
        let cfg = load_from_str(
            "[server]\nmode = \"lan\"\n[identity]\nkind = \"fixed\"\nsubject = \"me\"\n",
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn forwarded_requires_trusted() {
        let cfg = load_from_str("[identity]\nkind = \"forwarded\"\ntrusted = []\n");
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn forwarded_with_trusted_ok() {
        let cfg = load_from_str("[identity]\nkind = \"forwarded\"\ntrusted = [\"10.0.0.1\"]\n");
        assert!(validate(&cfg).is_ok());
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
        std::fs::write(
            &path,
            "[server]\nmode = \"local\"\n[identity]\nkind = \"fixed\"\nsubject = \"x\"\n",
        )
        .expect("write config");
        // 真实键名走完整 load 路径；本测试是唯一设置 PEGBOARD_* 变量的测试
        std::env::set_var("PEGBOARD_SERVER__MODE", "lan");
        std::env::set_var("PEGBOARD_LIMITS__NET_RPS", "99");
        let cfg = load(Some(&path));
        std::env::remove_var("PEGBOARD_SERVER__MODE");
        std::env::remove_var("PEGBOARD_LIMITS__NET_RPS");
        let cfg = cfg.expect("load ok");
        assert_eq!(cfg.server.mode, Mode::Lan);
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
