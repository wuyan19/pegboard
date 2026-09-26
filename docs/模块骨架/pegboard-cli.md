# pegboard-cli / 装配与启动

## 结构

```rust
// crates/pegboard-cli/src/main.rs

mod cli;
mod runtime;
mod shutdown;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pegboard: {e}");
            ExitCode::FAILURE
        }
    }
}
```

```rust
// crates/pegboard-cli/src/cli.rs

use std::path::PathBuf;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "pegboard", version, about = "Local app runtime")]
pub struct Args {
    /// 配置文件路径。缺省时按顺序查找：./config.toml、$PEGBOARD_CONFIG。
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    /// 覆盖监听地址。
    #[arg(long)]
    pub listen: Option<std::net::SocketAddr>,

    /// 覆盖应用目录。
    #[arg(long)]
    pub apps_dir: Option<PathBuf>,

    /// 覆盖数据根目录。
    #[arg(long)]
    pub data_root: Option<PathBuf>,

    /// 仅校验配置与应用清单，不启动服务。
    #[arg(long)]
    pub check: bool,

    /// 日志级别：error | warn | info | debug | trace。
    #[arg(long, default_value = "info")]
    pub log: String,
}

pub fn parse() -> Args;

/// 解析命令行 → 加载配置 → 应用覆盖项 → 校验。
pub fn run() -> Result<(), Box<dyn std::error::Error>>;
```

```rust
// crates/pegboard-cli/src/runtime.rs

use std::sync::Arc;
use pegboard_core::{
    app::AppRegistry, audit::Auditor, config::Config,
    files::{FilesManager, Signer}, guard::Guard,
    identity::Identity, proxy::Proxy, store::StoreManager,
};
use pegboard_server::ingress::AppState;

/// 已完成装配、可启动的运行时。
pub struct Runtime {
    pub state: Arc<AppState>,
    pub listener: tokio::net::TcpListener,
    pub server: axum::serve::Serve<...>,
}

/// 按依赖顺序构造所有组件。
pub fn build(config: Config, apps_dir: &std::path::Path)
    -> Result<Runtime, Box<dyn std::error::Error>>;

/// 启动 HTTP 服务，直到收到关闭信号。
pub async fn run(rt: Runtime) -> Result<(), Box<dyn std::error::Error>>;
```

```rust
// crates/pegboard-cli/src/shutdown.rs

/// 监听 SIGINT / SIGTERM，触发优雅关闭。
pub async fn signal() -> &'static str;

/// 关闭序列：
/// 1. 停止接受新连接
/// 2. 等待进行中请求完成（超时后强制）
/// 3. audit.shutdown() 排空队列
/// 4. 落盘、释放资源
pub async fn graceful<S>(server: S, auditor: Auditor, timeout: std::time::Duration)
    -> Result<(), Box<dyn std::error::Error>>;
```

## 装配顺序

依赖严格单向，按序构造：

1. **日志**：初始化 `tracing`，级别由 `--log` 决定。
2. **配置**：`config::load` + 命令行覆盖 + `validate`。
3. **数据目录**：创建 `data_root` 及子目录（`apps/`、`logs/`、`tmp/`）。
4. **应用注册表**：`AppRegistry::scan(apps_dir, &config.limits)`；错误汇总，坏应用告警不阻塞启动。
5. **审计器**：`Auditor::start(cfg)`。
6. **签名服务**：`Signer::open(host.db)`。
7. **身份**：`Identity::new(&config.identity)`。
8. **治理**：`Guard::new()`。
9. **存储**：`StoreManager::new(...)`。
10. **文件**：`FilesManager::new(..., signer)`。
11. **代理**：`Proxy::new(guard, cfg)`。
12. **HTTP 客户端**：由 `Proxy` 内部持有。
13. **状态**：组装 `AppState`。
14. **路由**：`build_router(state)`。
15. **监听**：`TcpListener::bind(config.server.listen)`。

任一步失败即退出，错误信息含具体组件。

## 启动与关闭流程

```rust
pub async fn run(rt: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(addr = %rt.listener.local_addr()?, "pegboard listening");

    let auditor = rt.state.auditor.clone_handle(); // 或 Arc 包装
    tokio::select! {
        r = axum::serve(rt.listener, rt.state.router) => r?,
        sig = shutdown::signal() => {
            tracing::info!(signal = sig, "shutting down");
        }
    }

    shutdown::graceful(..., auditor, Duration::from_secs(10)).await?;
    Ok(())
}
```

关闭序列：
1. `axum::serve` 收到信号后停止接受新连接。
2. 等待进行中请求完成，超时 10s 后强制终止。
3. `Auditor::shutdown()` 排空队列，join 写入线程。
4. 释放 `StoreManager` / `FilesManager`，关闭 SQLite 连接（WAL checkpoint）。
5. 进程退出。

## `--check` 模式

- 加载配置、扫描应用、校验清单。
- 输出应用列表与问题清单，不绑定端口、不写数据。
- 退出码：0 全通过；1 有错误；2 有告警但无错误。
- 用于 CI 与部署前自检。

## 日志

- `tracing_subscriber` + `EnvFilter`。
- 控制台输出，`info` 默认；`--log debug` 用于排障。
- 结构化字段：`app_id`、`subject`、`action`、`duration_ms`。
- **审计日志独立**：由 `Auditor` 写文件，不经 `tracing`。
- 不记录凭据、请求体、业务数据。

## 单元 / 集成测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn write_app(apps: &Path, id: &str) {
        let d = apps.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), b"<h1>x</h1>").unwrap();
        std::fs::write(d.join("manifest.json"), format!(r#"{{
            "id":"{id}","name":"{id}","entry":"index.html"
        }}"#)).unwrap();
    }

    // ---------- 配置加载 ----------

    #[test]
    fn loads_config_file() {
        // 写合法配置，断言 listen / mode 生效
    }

    #[test]
    fn cli_overrides_config() {
        // --listen 覆盖文件中的 listen
    }

    #[test]
    fn invalid_config_exits_nonzero() {
        // 非法配置 → run() 返回 Err
    }

    // ---------- 装配 ----------

    #[tokio::test]
    async fn build_creates_data_dirs() {
        // build 后断言 data_root 下子目录存在
    }

    #[tokio::test]
    async fn build_scans_apps() {
        // 两个合法 app → 注册表含 2 项
    }

    #[tokio::test]
    async fn bad_app_warns_but_starts() {
        // 一个坏 app + 一个好 app → 启动成功，注册表含 1 项
    }

    #[tokio::test]
    async fn build_fails_on_bad_identity() {
        // Mode::Lan + Fixed(None) → build 失败
    }

    #[tokio::test]
    async fn listener_binds_configured_addr() {
        // 断言 local_addr 与配置一致
    }

    // ---------- check 模式 ----------

    #[test]
    fn check_mode_passes_on_valid() {
        // 合法配置 + app → exit 0
    }

    #[test]
    fn check_mode_reports_errors() {
        // 非法 app → exit 1，输出含 app id
    }

    #[test]
    fn check_mode_does_not_bind() {
        // 监听端口被占用时 check 仍成功
    }

    // ---------- 关闭 ----------

    #[tokio::test]
    async fn graceful_shutdown_drains_audit() {
        // 启动服务 → 发请求 → 触发关闭 → 断言审计日志含该请求
    }

    #[tokio::test]
    async fn graceful_shutdown_within_timeout() {
        // 长请求进行中触发关闭，断言在 timeout 内退出
    }

    // ---------- 端到端 ----------

    #[tokio::test]
    async fn serve_static_app() {
        // 启动 → GET /apps/t/index.html → 200
    }

    #[tokio::test]
    async fn serve_capability_api() {
        // 启动 → GET /api/store/kv/x（带 app 头）→ 200 / 404
    }
}
```

## 依赖

```toml
[dependencies]
pegboard-core   = { path = "../pegboard-core" }
pegboard-server = { path = "../pegboard-server" }

clap  = { workspace = true, features = ["derive", "env"] }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "net", "signal"] }
axum  = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true, features = ["env-filter", "fmt"] }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
reqwest  = { workspace = true }
```

## 设计要点

- **装配集中在 CLI**：core 与 server 都不感知彼此如何被组合；只有 CLI 知道完整依赖图。
- **失败早退**：配置与装配阶段任何错误立即退出，不进入半可用状态。
- **坏应用不阻塞**：单个应用清单损坏只告警，其余照常服务；符合“内部工具”容错预期。
- **优雅关闭显式**：信号 → 停接入 → 排空审计 → checkpoint → 退出；不依赖 Drop 顺序。
- **审计独立于 tracing**：两者落点不同、生命周期不同，混在一起会丢事件或阻塞请求路径。
- **`--check` 用于 CI**：不绑定端口、不写数据，适合部署前自检。
- **单二进制**：CLI 是唯一入口，其他 crate 不产出可执行文件。
