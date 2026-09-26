//! 接入层：路由分发、请求上下文、同源约束、错误集中映射。
//!
//! M2：路由骨架与静态托管接线；能力 API 的 RequestContext extractor 随 M3 接入。

pub mod error;
pub mod router;

pub use error::ApiError;
pub use router::{build_router, AppState};
