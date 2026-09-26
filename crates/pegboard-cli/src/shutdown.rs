//! 关闭信号：监听 SIGINT / SIGTERM。

/// 等待关闭信号，返回信号名（"interrupt" / "terminate"）。
/// 信号注册失败时退化为立即返回 terminate（进程已无法优雅关闭）。
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
