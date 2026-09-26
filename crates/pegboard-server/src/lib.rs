//! pegboard-server：HTTP 接入、路由、静态托管、管理页。
//!
//! 依赖 pegboard-core；协议转换只在 ingress。

pub mod admin;
pub mod ingress;
pub mod statics;
