//! pegboard：单二进制入口。装配与启动在 M1/M2 里程碑补齐。

fn main() {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "pegboard");
}
