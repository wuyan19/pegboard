//! 关闭信号：Unix 监听 SIGINT / SIGTERM；Windows 监听 Ctrl+C。

/// 等待关闭信号，返回信号名（"interrupt" / "terminate" / "ctrl_c"）。
/// 信号注册失败时退化为立即返回 terminate（进程已无法优雅关闭）。
#[cfg(unix)]
pub async fn signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let interrupt = signal(SignalKind::interrupt());
    let terminate = signal(SignalKind::terminate());
    let (mut interrupt, mut terminate) = match (interrupt, terminate) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(error = %e, "信号注册失败，退化为立即关闭");
            return "terminate";
        }
    };
    tokio::select! {
        _ = interrupt.recv() => "interrupt",
        _ = terminate.recv() => "terminate",
    }
}

/// Windows 无 SIGTERM：控制台 Ctrl+C（或服务管理器发来的 CTRL_BREAK 等）
/// 统一经 ctrl_c 通道；无头服务场景由进程控制方负责通知关闭。
#[cfg(windows)]
pub async fn signal() -> &'static str {
    match tokio::signal::ctrl_c().await {
        Ok(()) => "ctrl_c",
        Err(e) => {
            tracing::error!(error = %e, "ctrl_c 监听失败，退化为立即关闭");
            "terminate"
        }
    }
}
