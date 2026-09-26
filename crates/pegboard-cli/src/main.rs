//! pegboard：单二进制入口。装配与启动见 cli / runtime / shutdown 模块。

mod cli;
mod runtime;
mod shutdown;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match cli::run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pegboard: {e}");
            ExitCode::FAILURE
        }
    }
}
