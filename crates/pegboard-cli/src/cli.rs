//! CLI 装配：参数解析、配置加载、`--check` 自检。
//! 服务启动与优雅关闭在 M2 接入（runtime / shutdown 模块）。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use pegboard_core::app::AppRegistry;
use pegboard_core::config::{Config, IdentityConfig, Mode};

#[derive(Debug, Parser)]
#[command(name = "pegboard", version, about = "Local app runtime")]
pub struct Args {
    /// 配置文件路径。缺省时按顺序查找：$PEGBOARD_CONFIG、./config.toml
    #[arg(long, short, env = "PEGBOARD_CONFIG")]
    pub config: Option<PathBuf>,

    /// 覆盖监听地址
    #[arg(long)]
    pub listen: Option<std::net::SocketAddr>,

    /// 覆盖应用产物目录
    #[arg(long)]
    pub apps_dir: Option<PathBuf>,

    /// 覆盖数据根目录
    #[arg(long)]
    pub data_root: Option<PathBuf>,

    /// 仅校验配置与应用清单，不启动服务
    #[arg(long)]
    pub check: bool,

    /// 日志级别：error | warn | info | debug | trace
    #[arg(long, default_value = "info")]
    pub log: String,
}

pub fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_tracing(&args.log);
    let mut config = load_config(&args)?;
    apply_overrides(&args, &mut config);

    if args.check {
        return Ok(check_mode(&config));
    }
    tracing::warn!("serve 模式尚未接入（M2）；当前版本仅支持 --check");
    Ok(ExitCode::SUCCESS)
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn load_config(args: &Args) -> Result<Config, Box<dyn std::error::Error>> {
    let path = args.config.clone().or_else(|| {
        let cwd = PathBuf::from("./config.toml");
        cwd.exists().then_some(cwd)
    });
    let config = pegboard_core::config::load(path.as_deref())?;
    Ok(config)
}

fn apply_overrides(args: &Args, config: &mut Config) {
    if let Some(listen) = args.listen {
        config.server.listen = listen;
    }
    if let Some(dir) = &args.apps_dir {
        config.storage.apps_dir = dir.clone();
    }
    if let Some(dir) = &args.data_root {
        config.storage.data_root = dir.clone();
    }
}

/// 配置类告警（不算错误）：Lan 模式 + 空 token 表。
fn advisory_warnings(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if config.server.mode == Mode::Lan {
        if let IdentityConfig::Tokens { tokens } = &config.identity {
            if tokens.is_empty() {
                out.push("mode=lan 且 identity tokens 为空：所有请求都将是匿名".to_owned());
            }
        }
    }
    out
}

/// 自检：加载配置、扫描应用、校验清单；不绑定端口、不写数据。
/// 退出码：0 全通过；1 有错误；2 有告警但无错误。
fn check_mode(config: &Config) -> ExitCode {
    let outcome = match AppRegistry::scan(&config.storage.apps_dir, &config.limits) {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!(error = %e, apps_dir = %config.storage.apps_dir.display(), "apps 目录不可读");
            return ExitCode::from(1);
        }
    };
    for meta in outcome.registry.list() {
        tracing::info!(
            app_id = %meta.id,
            name = %meta.name,
            entry = meta.entry.display().to_string(),
            "app ok"
        );
    }
    for warning in &outcome.warnings {
        tracing::error!(error = %warning, "app invalid");
    }
    let advisories = advisory_warnings(config);
    for advisory in &advisories {
        tracing::warn!(advisory = %advisory, "config advisory");
    }
    if !outcome.warnings.is_empty() {
        ExitCode::from(1)
    } else if !advisories.is_empty() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
