//! pegboard：单二进制入口。装配与启动见 cli / runtime / shutdown / process 模块。

// release 模式（Windows）用 GUI 子系统：双击不弹终端黑窗；从 cmd/PowerShell 启动时
// 由 console 模块重接父控制台恢复输出。debug 模式保留控制台便于开发。
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod cli;
mod console;
mod process;
mod runtime;
mod shutdown;
mod tray;

use std::process::ExitCode;

fn main() -> ExitCode {
    // GUI 子系统下 stdout 默认无效：从终端启动时重接父控制台（banner/--help 可见），
    // 双击启动 attach 失败静默继续（无终端模式，日志走文件）。
    #[cfg(target_os = "windows")]
    let _ = console::attach_parent_console();

    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    match cli::run(has_tty) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pegboard: {e}");
            ExitCode::FAILURE
        }
    }
}
