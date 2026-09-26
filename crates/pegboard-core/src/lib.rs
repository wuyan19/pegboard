//! pegboard-core：能力与治理，纯逻辑，不依赖 HTTP 框架。
//!
//! 模块划分见 docs/模块划分.md。依赖方向：基础（config、app）最底；
//! audit 为叶子；guard 只依赖基础与 audit；store/files/proxy 依赖基础。

pub mod app;
pub mod audit;
pub mod config;
pub mod files;
pub mod guard;
pub mod identity;
pub mod proxy;
pub mod store;
