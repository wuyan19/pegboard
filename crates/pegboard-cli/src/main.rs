//! pegboard：单二进制入口。装配与启动见 cli 模块。

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pegboard: {e}");
            ExitCode::FAILURE
        }
    }
}
