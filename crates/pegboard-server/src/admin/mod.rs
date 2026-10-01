//! 管理模块：宿主元数据可视化与操作（模块划分 §6）。
//! 只读为主；操作限于启用/禁用/安装/卸载等宿主元数据，不触碰应用业务数据。

pub mod api;
pub mod auth;
pub mod page;

pub use api::routes as api_routes;
pub use auth::routes as auth_routes;
pub use page::routes as page_routes;
