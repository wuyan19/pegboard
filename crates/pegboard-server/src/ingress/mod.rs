//! 接入层：路由分发、请求上下文、同源约束、错误集中映射。
//!
//! 能力 API 统一经 RequestContext extractor：解析 app → 解析 subject → 构造上下文；
//! handler 先过 guard 再调 core；审计只覆盖能力路径（静态 / SDK / admin 不入审计）。

pub mod context;
pub mod error;
pub mod proxy_api;
pub mod router;
pub mod store_api;

use std::sync::Arc;

use pegboard_core::audit::{ActionKind, AuditEvent, Outcome};

pub use context::{RequestContext, APP_HEADER};
pub use error::ApiError;
pub use router::{build_router, AppState};

use crate::ingress::context::RequestContext as Ctx;
use crate::ingress::error::code;

/// 能力调用统一审计包装：进入记时、退出记事件；不阻塞主路径。
pub async fn trace_audit<T, F>(
    state: &Arc<AppState>,
    ctx: &Ctx,
    action: ActionKind,
    target: Option<String>,
    fut: F,
) -> Result<T, ApiError>
where
    F: std::future::Future<Output = Result<T, ApiError>>,
{
    let result = fut.await;
    let duration_ms = ctx.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let (outcome, error_code) = match &result {
        Ok(_) => (Outcome::Ok, None),
        Err(e) => {
            let denied = matches!(e.code, code::PERMISSION_DENIED | code::TARGET_DENIED);
            (
                if denied {
                    Outcome::Denied
                } else {
                    Outcome::Error
                },
                Some(e.code),
            )
        }
    };
    let event = AuditEvent {
        ts: now_ms(),
        app_id: Some(ctx.app.id.clone()),
        subject: ctx.subject.0.clone(),
        action,
        target,
        outcome,
        error_code: error_code.map(str::to_owned),
        duration_ms,
    };
    if let Err(e) = state.auditor.record(event) {
        tracing::error!(error = %e, "audit record failed");
    }
    result
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
