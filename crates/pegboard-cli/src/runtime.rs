//! 装配与启动：按依赖顺序构造组件，绑定监听，服务直至关闭信号。
//! 装配顺序（cli 骨架）：日志 → 配置 → 数据目录 → 注册表 → 审计 → 状态 → 路由 → 监听。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pegboard_core::app::AppRegistry;
use pegboard_core::audit::{AuditConfig, Auditor};
use pegboard_core::config::Config;
use pegboard_server::host::ProcessControl;
use pegboard_server::ingress::{build_router, AppState};
use tokio::sync::Notify;

use crate::shutdown;

/// 优雅关闭排空上限。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// 重启子进程的端口绑定重试：25 × 100ms ≈ 2.5s，覆盖旧进程释放监听的窗口。
const RESTART_BIND_RETRIES: u32 = 25;
const RESTART_BIND_WAIT: Duration = Duration::from_millis(100);

/// 已完成装配、可启动的运行时。
pub struct Runtime {
    pub listener: tokio::net::TcpListener,
    pub router: axum::Router,
    pub auditor: Auditor,
    pub audit_handle: pegboard_core::audit::AuditorHandle,
}

/// 按依赖顺序构造所有组件。任一步失败即退出，错误信息含具体组件。
pub async fn build(
    config: Config,
    control: Arc<dyn ProcessControl>,
) -> Result<Runtime, Box<dyn std::error::Error>> {
    // 数据目录：data_root 下 logs / tmp / apps_data
    let data_root = config.storage.data_root.clone();
    for sub in ["logs", "tmp", "apps_data"] {
        std::fs::create_dir_all(data_root.join(sub))
            .map_err(|e| format!("创建 {} 失败: {e}", data_root.join(sub).display()))?;
    }

    // host.db 先于注册表：apps 表是「已安装注册表」的权威（数据模型 §6）
    let host_db = Arc::new(
        pegboard_core::app::HostDb::open(&data_root.join("host.db"))
            .map_err(|e| format!("打开 host.db 失败: {e}"))?,
    );

    // 应用注册表：按 host.db 已安装行加载；首次启动（表为空）seed 安装 apps/ 下全部有效应用
    let rows = host_db
        .app_states()
        .map_err(|e| format!("读取 host.db 应用状态失败: {e}"))?;
    let mut registry = AppRegistry::default();
    let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
    if rows.is_empty() {
        let outcome = AppRegistry::scan(&config.storage.apps_dir, &config.limits)
            .map_err(|e| format!("扫描 apps 目录失败: {e}"))?;
        for warning in &outcome.warnings {
            tracing::error!(error = %warning, "app invalid (skipped)");
        }
        for meta in outcome.registry.list() {
            host_db
                .upsert_app(
                    &meta.id,
                    &meta.name,
                    &meta.root.display().to_string(),
                    &meta.manifest_json(),
                )
                .map_err(|e| format!("seed 写入 host.db 失败: {e}"))?;
            tracing::info!(app_id = %meta.id, "首次启动 seed 安装");
        }
        registry = outcome.registry;
    } else {
        for (id, (enabled, _)) in rows {
            match registry.reload_one(&config.storage.apps_dir, &id, &config.limits) {
                Ok(_) => {
                    if !enabled {
                        disabled.insert(id);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, app_id = %id, "已安装应用加载失败（列表中标记缺失/无效）");
                }
            }
        }
    }

    // 审计器
    let auditor = Auditor::start(AuditConfig {
        log_dir: data_root.join("logs"),
        ..Default::default()
    })
    .map_err(|e| format!("启动审计器失败: {e}"))?;
    let audit_handle = auditor.handle();

    let guard = Arc::new(pegboard_core::guard::Guard::with_injected(
        config
            .identity
            .injected_headers()
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    ));
    let proxy = pegboard_core::proxy::Proxy::new(
        Arc::clone(&guard),
        pegboard_core::proxy::ProxyConfig::default(),
    )
    .map_err(|e| format!("构造代理器失败: {e}"))?;
    let signer = Arc::new(pegboard_core::files::Signer::new(Arc::clone(&host_db)));

    // 管理鉴权：设置了 PEGBOARD_ADMIN_TOKEN 即启用（lan 部署建议配置；敏感项走环境变量）
    let admin_token = match std::env::var("PEGBOARD_ADMIN_TOKEN") {
        Ok(t) if !t.trim().is_empty() => {
            if t.trim().len() < 16 {
                return Err("PEGBOARD_ADMIN_TOKEN 长度须至少 16 字符".into());
            }
            Some(t.trim().to_owned())
        }
        _ => None,
    };
    let state = Arc::new(AppState {
        identity: pegboard_core::identity::Identity::new(&config.identity),
        guard,
        stores: pegboard_core::store::StoreManager::new(data_root.join("apps_data"), config.limits),
        files: pegboard_core::files::FilesManager::new(
            data_root.join("apps_data"),
            data_root.join("tmp"),
        ),
        signer: Arc::clone(&signer),
        host_db: Arc::clone(&host_db),
        started: std::time::Instant::now(),
        admin_token,
        disabled: RwLock::new(disabled),
        proxy: Arc::new(proxy),
        config: config.clone(),
        apps: RwLock::new(registry),
        auditor: audit_handle.clone(),
        control,
        update: Arc::new(pegboard_server::host::update::Updater::new(
            &config.update.manifest_url,
            data_root.join("tmp"),
        )),
    });
    let router = build_router(Arc::clone(&state));

    let listener = bind_listener(config.server.listen).await?;

    Ok(Runtime {
        listener,
        router,
        auditor,
        audit_handle,
    })
}

/// 绑定监听。重启子进程（process::RESTART_CHILD_ENV=1）带重试窗口：
/// 旧进程优雅退出需先释放监听，端口被占时按固定间隔重试。
async fn bind_listener(
    addr: std::net::SocketAddr,
) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error>> {
    if std::env::var_os(crate::process::RESTART_CHILD_ENV).is_none() {
        return tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("绑定 {addr} 失败: {e}").into());
    }
    let mut last_err = None;
    for attempt in 0..=RESTART_BIND_RETRIES {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                if attempt > 0 {
                    tracing::info!(attempt, "重启子进程经重试后绑定成功");
                }
                return Ok(l);
            }
            Err(e) if attempt < RESTART_BIND_RETRIES => {
                last_err = Some(e);
                tokio::time::sleep(RESTART_BIND_WAIT).await;
            }
            Err(e) => {
                return Err(
                    format!("绑定 {addr} 失败（重试 {RESTART_BIND_RETRIES} 次后）: {e}").into(),
                )
            }
        }
    }
    Err(format!(
        "绑定 {addr} 失败: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )
    .into())
}

/// 启动 HTTP 服务，直至关闭信号或宿主控制通知；随后排空审计队列并置位
/// `stopped`（托盘/无头控制据此结束进程）。
pub async fn serve(
    rt: Runtime,
    extra_shutdown: Option<Arc<Notify>>,
    stopped: Option<Arc<AtomicBool>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Runtime {
        listener,
        router,
        auditor,
        audit_handle,
    } = rt;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("获取监听地址失败: {e}"))?;
    tracing::info!(%addr, "pegboard listening");

    // 服务任务：信号触发排空，完成后经 done 通知
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<Result<(), std::io::Error>>();
    let server_task = tokio::spawn(async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = drain_rx.await;
            })
            .await;
        let _ = done_tx.send(result);
    });

    // 宿主控制关闭路径（托盘退出 / 重启请求）；无控制方时空等
    let host_control = async {
        match extra_shutdown {
            Some(notify) => notify.notified().await,
            None => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        done = &mut done_rx => {
            match done {
                Ok(Ok(())) => {} // 服务自行结束（正常情况下不会发生）
                Ok(Err(e)) => return Err(format!("server error: {e}").into()),
                Err(e) => return Err(format!("server task 失败: {e}").into()),
            }
        }
        signal = shutdown::signal() => {
            tracing::info!(signal = %signal, "shutting down");
            drain_server(drain_tx, &mut done_rx, &server_task).await?;
        }
        _ = host_control => {
            tracing::info!("host control requested shutdown");
            drain_server(drain_tx, &mut done_rx, &server_task).await?;
        }
    }

    // 审计排空：阻塞收尾放 spawn_blocking，避免 async 上下文阻塞 IO
    let join = tokio::task::spawn_blocking(move || auditor.shutdown())
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { format!("audit join 失败: {e}").into() })?;
    join.map_err(|e| -> Box<dyn std::error::Error> { format!("审计落盘失败: {e}").into() })?;
    let dropped = audit_handle.dropped();
    if dropped > 0 {
        tracing::warn!(dropped, "audit events were dropped under pressure");
    }

    // 收尾完成：托盘/无头控制据此结束进程
    if let Some(flag) = &stopped {
        flag.store(true, Ordering::Release);
    }
    tracing::info!("pegboard stopped");
    Ok(())
}

/// 通知服务排空并等待完成；超时强制中止。
async fn drain_server(
    drain_tx: tokio::sync::oneshot::Sender<()>,
    done_rx: &mut tokio::sync::oneshot::Receiver<Result<(), std::io::Error>>,
    server_task: &tokio::task::JoinHandle<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = drain_tx.send(());
    match tokio::time::timeout(DRAIN_TIMEOUT, done_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(format!("server error: {e}").into()),
        Ok(Err(e)) => Err(format!("server task 失败: {e}").into()),
        Err(_) => {
            tracing::warn!("graceful shutdown 超时，强制退出");
            server_task.abort();
            Ok(())
        }
    }
}
