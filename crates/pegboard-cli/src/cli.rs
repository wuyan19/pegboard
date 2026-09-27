//! CLI 装配：参数解析、配置加载、`--check` 自检、日志落点、服务编排。
//! 运行时装配（组件构造）在 runtime 模块；进程控制实现在 process 模块。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::Parser;
use pegboard_core::app::AppRegistry;
use pegboard_core::config::{Config, IdentityConfig, Mode};
use pegboard_server::host::ProcessControl;
use tokio::sync::Notify;

use crate::process::HeadlessControl;

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

    /// 不启用系统托盘（无头服务模式：ssh / 开机自启 / 进程监管器场景）
    #[arg(long, env = "PEGBOARD_NO_TRAY")]
    pub no_tray: bool,

    /// 日志级别：error | warn | info | debug | trace
    #[arg(long, default_value = "info")]
    pub log: String,
}

/// 入口编排（同步）：解析 → 配置 → 日志 → check / serve。
/// `has_tty` 由 main 判定（Windows GUI 子系统下已先重接父控制台）。
pub fn run(has_tty: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse();

    // 配置与存储目录解析（resolve_config）：显式配置永远最高优先；
    // 开发树（cwd 有 config.toml）行为不变；发布形态落到平台数据区。
    let resolved = resolve_config(&args)?;
    let mut config = pegboard_core::config::load(resolved.config_path.as_deref())?;
    if let Some(base) = &resolved.rebase {
        rebase_storage(&mut config, base);
    }
    apply_overrides(&args, &mut config);

    if args.check {
        init_tracing(&args.log, has_tty, None);
        return Ok(check_mode(&config));
    }

    // serve：数据目录先建——无终端模式的运行日志落在 data_root/logs
    let logs_dir = config.storage.data_root.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", logs_dir.display()))?;

    // 日志落点：终端 → stdout；无终端（双击/open/自启）→ 追加文件层。
    // 文件层经 non_blocking 写线程落盘，tokio worker 打日志不构成阻塞 IO。
    // guard 必须存活到进程结束（持于本函数栈帧，serve 期间不返回）。
    let _log_guard = init_tracing(&args.log, has_tty, Some(&logs_dir));
    tracing::info!(has_tty, no_tray = args.no_tray, "pegboard starting");

    // 应用产物目录缺失则创建（发布形态首跑友好；权限错误仍 fail-fast）
    if !config.storage.apps_dir.exists() {
        std::fs::create_dir_all(&config.storage.apps_dir)
            .map_err(|e| format!("创建 {} 失败: {e}", config.storage.apps_dir.display()))?;
        tracing::info!(apps_dir = %config.storage.apps_dir.display(), "apps 目录不存在，已创建");
    }

    tracing::info!(
        apps_dir = %config.storage.apps_dir.display(),
        data_root = %config.storage.data_root.display(),
        "serve starting"
    );
    serve(config, args.no_tray)
}

/// serve 编排：无头模式在本线程建 tokio runtime 直跑；托盘模式主线程让给
/// tao 事件循环，服务跑子线程（tray 模块）。托盘初始化失败（无 GUI 会话）
/// 降级为无头模式，服务可用性不受影响。
fn serve(config: Config, no_tray: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if no_tray || !tray_available() {
        serve_headless(config)?;
        return Ok(ExitCode::SUCCESS);
    }
    let fallback = config.clone();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        crate::tray::serve_with_tray(config)
    })) {
        Ok(result) => {
            result?;
            Ok(ExitCode::SUCCESS)
        }
        Err(_) => {
            tracing::error!("托盘初始化失败（无 GUI 会话？），降级为无头服务模式");
            serve_headless(fallback)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// 托盘可用性预判：无显示服务器的 Linux（DISPLAY/WAYLAND_DISPLAY 均缺失）
/// 直接走无头路径；其余平台的初始化失败由 catch_unwind 兜底降级。
fn tray_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        return has_display;
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 无头服务：本线程 tokio runtime，信号 / 进程控制双路优雅关闭。
fn serve_headless(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let notify = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let control: Arc<dyn ProcessControl> = Arc::new(HeadlessControl::new(Arc::clone(&notify)));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime 初始化失败: {e}"))?;
    rt.block_on(async move {
        let runtime = crate::runtime::build(config, control).await?;
        crate::runtime::serve(runtime, Some(notify), Some(stopped)).await
    })?;
    Ok(())
}

/// 初始化 tracing。`file_logs` 给定时追加非阻塞文件层（logs/host.log），
/// stdout 层始终保留（管道/进程监管器仍可采集）。返回文件层 WorkerGuard。
fn init_tracing(
    level: &str,
    has_tty: bool,
    file_logs: Option<&Path>,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout_layer = fmt::layer().with_ansi(has_tty);
    match file_logs {
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .init();
            None
        }
        Some(dir) => {
            rotate_if_large(&dir.join("host.log"), 8 * 1024 * 1024);
            let appender = tracing_appender::rolling::never(dir, "host.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer().with_ansi(false).with_writer(writer);
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .init();
            Some(guard)
        }
    }
}

/// host.log 超 8 MiB 时轮转为 host.log.1（覆盖上一代）。启动时一次性执行。
fn rotate_if_large(path: &Path, max: u64) {
    let oversize = std::fs::metadata(path)
        .map(|m| m.len() > max)
        .unwrap_or(false);
    if oversize {
        let rotated = path.with_extension("log.1");
        if let Err(e) = std::fs::rename(path, &rotated) {
            eprintln!("pegboard: 轮转日志失败: {e}");
        }
    }
}

/// 配置与存储目录解析结果。
struct Resolved {
    /// 实际加载的配置文件（None = 全默认值）
    config_path: Option<PathBuf>,
    /// 相对存储路径的重定基目录（bundle 数据区 / 平台数据区）
    rebase: Option<PathBuf>,
}

/// 配置发现链：显式（--config / $PEGBOARD_CONFIG，clap env 已并入）→ bundle
/// 数据区 → cwd（开发树惯例）→ 平台配置目录 → 全默认（平台数据区）。
///
/// 重定基规则：bundle / 平台配置目录 / 全默认命中时，相对存储路径重定基到
/// 对应数据区——发布的宿主不在 cwd 撒文件；显式与 cwd 配置保持相对 cwd
/// （开发树行为不变）。
fn resolve_config(args: &Args) -> Result<Resolved, Box<dyn std::error::Error>> {
    if args.config.is_some() {
        return Ok(Resolved {
            config_path: args.config.clone(),
            rebase: None,
        });
    }
    // .app bundle（双击）：cwd 是 `/` 且升级会整包替换 bundle——引导把配置与
    // 内置应用合并进平台数据区（prepare_bundle）
    if let Some(base) = prepare_bundle()? {
        let cfg = base.join("config.toml");
        return Ok(Resolved {
            config_path: cfg.exists().then_some(cfg),
            rebase: Some(base),
        });
    }
    let cwd = PathBuf::from("./config.toml");
    if cwd.exists() {
        return Ok(Resolved {
            config_path: Some(cwd),
            rebase: None,
        });
    }
    if let Some(dir) = platform_config_dir() {
        let cfg = dir.join("config.toml");
        if cfg.exists() {
            return Ok(Resolved {
                config_path: Some(cfg),
                rebase: platform_data_dir(),
            });
        }
    }
    Ok(Resolved {
        config_path: None,
        rebase: platform_data_dir(),
    })
}

/// .app bundle 引导：定位 bundle、准备用户数据区（配置模板 + 内置应用合并）。
/// 返回用户数据区根（非 bundle 形态返回 None）。
fn prepare_bundle() -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let Some(base) = bundle_base_dir() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&base).map_err(|e| format!("创建 {} 失败: {e}", base.display()))?;

    // 配置模板：首次落一份（已有用户配置则不动）
    let cfg_path = base.join("config.toml");
    if !cfg_path.exists() {
        if let Some(bundled) = bundle_resource("config.toml") {
            let _ = std::fs::copy(&bundled, &cfg_path);
        }
    }

    // 内置应用合并（只补缺失目录）：bundle 升级带来的新应用随启动注入；
    // 用户在管理页删除的内置应用会随 bundle 再次出现（重置语义，v1 取舍）。
    if let Some(bundled_apps) = bundle_resource("apps") {
        let target = base.join("apps");
        std::fs::create_dir_all(&target)
            .map_err(|e| format!("创建 {} 失败: {e}", target.display()))?;
        if let Ok(entries) = std::fs::read_dir(&bundled_apps) {
            for entry in entries.filter_map(Result::ok) {
                let dest = target.join(entry.file_name());
                if !dest.exists() {
                    copy_dir_all(&entry.path(), &dest).map_err(|e| {
                        format!(
                            "合并内置应用 {} 失败: {e}",
                            entry.file_name().to_string_lossy()
                        )
                    })?;
                    tracing::info!(app = %entry.file_name().to_string_lossy(), "bundle 内置应用已合并");
                }
            }
        }
    }
    Ok(Some(base))
}

/// 递归目录拷贝（std 无 copy_dir_all；应用产物规模小，无需并行/进度）。
fn copy_dir_all(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// 当前进程是否跑在 .app bundle 内（macOS 双击形态）。
fn in_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.ancestors()
                .skip(1)
                .find(|p| p.extension().is_some_and(|e| e == "app"))
                .map(|_| ())
        })
        .is_some()
}

/// bundle 内置资源定位（Contents/MacOS 旁）。
fn bundle_resource(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let res = exe.parent()?.join(name);
    res.exists().then_some(res)
}

/// bundle 形态的宿主数据区根。
fn bundle_base_dir() -> Option<PathBuf> {
    if !in_app_bundle() {
        return None;
    }
    platform_base_dir().map(|p| p.join("Pegboard"))
}

/// 平台用户数据根（Application Support / APPDATA / XDG data）。
fn platform_base_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }
}

/// 平台配置目录（发现的 config.toml 落点）。macOS/Windows 与数据同区；
/// Linux 按 XDG 惯例区分 config 与 data。
fn platform_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        platform_base_dir().map(|p| p.join("Pegboard"))
    }
    #[cfg(windows)]
    {
        platform_base_dir().map(|p| p.join("Pegboard"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("pegboard"))
    }
}

/// 平台数据目录（无配置时的默认存储基）。
fn platform_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        platform_base_dir().map(|p| p.join("Pegboard"))
    }
    #[cfg(windows)]
    {
        platform_base_dir().map(|p| p.join("Pegboard"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        platform_base_dir().map(|p| p.join("pegboard"))
    }
}

/// bundle 模式下相对路径重定基（cwd=/ 下相对路径无意义）；绝对路径尊重用户配置。
fn rebase_storage(config: &mut Config, base: &Path) {
    if config.storage.apps_dir.is_relative() {
        config.storage.apps_dir = base.join(&config.storage.apps_dir);
    }
    if config.storage.data_root.is_relative() {
        config.storage.data_root = base.join(&config.storage.data_root);
    }
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
