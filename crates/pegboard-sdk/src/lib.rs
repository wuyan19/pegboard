//! pegboard-sdk：前端 SDK 源码与类型产物，零依赖。
//!
//! `assets/` 下的 sdk.js / sdk.d.ts 编译期内嵌（include_str!），
//! 由 server 的 statics 模块经 `/sdk.js`、`/sdk.d.ts` 对外提供并注入应用页面。

/// SDK 运行时（浏览器侧）。
pub const SDK_JS: &str = include_str!("../assets/sdk.js");

/// SDK TypeScript 类型声明（供本地类型检查与 AI 读取）。
pub const SDK_DTS: &str = include_str!("../assets/sdk.d.ts");

/// SDK 版本，与 crate 版本同步。
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");
