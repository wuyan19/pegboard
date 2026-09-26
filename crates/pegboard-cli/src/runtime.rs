//! 装配与启动：按依赖顺序构造组件，绑定监听，服务直至关闭信号。
//! 装配顺序（cli 骨架）：日志 → 配置 → 数据目录 → 注册表 → 审计 → 状态 → 路由 → 监听。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use pegboard_core::app::AppRegistry;
use pegboard_core::audit::{AuditConfig, Auditor};
use pegboard_core::config::Config;
use pegboard_server::ingress::{build_router, AppState};

use crate::shutdown;

/// 优雅关闭排空上限。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 已完成装配、可启动的运行时。
pub struct Runtime {
    pub listener: tokio::net::TcpListener,
    pub router: axum::Router,
    pub auditor: Auditor,
    pub audit_handle: pegboard_core::audit::AuditorHandle,
}

/// 按依赖顺序构造所有组件。任一步失败即退出，错误信息含具体组件。
pub async fn build(config: Config) -> Result<Runtime, Box<dyn std::error::Error>> {
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
    let legacy_upgrade = host_db
        .has_only_legacy_rows()
        .map_err(|e| format!("读取 host.db 应用状态失败: {e}"))?;
    let mut registry = AppRegistry::default();
    let mut disabled: std::collections::HashSet<String> = std::collections::HashSet::new();
    if rows.is_empty() || legacy_upgrade {
        let outcome = AppRegistry::scan(&config.storage.apps_dir, &config.limits)
            .map_err(|e| format!("扫描 apps 目录失败: {e}"))?;
        for warning in &outcome.warnings {
            tracing::error!(error = %warning, "app invalid (skipped)");
        }
        // 保留既有行的启停状态（如旧版禁用过的应用），其余注册为启用
        for meta in outcome.registry.list() {
            let enabled = rows.get(&meta.id).map_or(true, |(e, _)| *e);
            host_db
                .upsert_app(
                    &meta.id,
                    &meta.name,
                    &meta.root.display().to_string(),
                    &meta.manifest_json(),
                )
                .map_err(|e| format!("seed 写入 host.db 失败: {e}"))?;
            host_db
                .set_enabled(&meta.id, enabled)
                .map_err(|e| format!("seed 写入 host.db 失败: {e}"))?;
            if !enabled {
                disabled.insert(meta.id.clone());
            }
            tracing::info!(app_id = %meta.id, "seed 安装");
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
    });
    let router = build_router(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(config.server.listen)
        .await
        .map_err(|e| format!("绑定 {} 失败: {e}", config.server.listen))?;

    Ok(Runtime {
        listener,
        router,
        auditor,
        audit_handle,
    })
}

/// 启动 HTTP 服务，直到收到关闭信号；随后排空审计队列。
pub async fn serve(rt: Runtime) -> Result<(), Box<dyn std::error::Error>> {
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
            let _ = drain_tx.send(());
            match tokio::time::timeout(DRAIN_TIMEOUT, &mut done_rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => return Err(format!("server error: {e}").into()),
                Ok(Err(e)) => return Err(format!("server task 失败: {e}").into()),
                Err(_) => {
                    tracing::warn!("graceful shutdown 超时，强制退出");
                    server_task.abort();
                }
            }
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
    tracing::info!("pegboard stopped");
    Ok(())
}
