//! 进程层：重启子进程的 spawn 与 `ProcessControl` 实现。
//!
//! 重启协议（架构设计 §11）：以相同参数 spawn 当前可执行文件，子进程携带
//! `PEGBOARD_RESTART_CHILD=1` 换得端口绑定重试窗口；随后本进程优雅退出。
//! 托盘模式的实现走 tray 模块（spawn 必须发生在托盘事件循环内），本模块
//! 供无头模式与测试使用。

use std::sync::Arc;

use pegboard_server::host::ProcessControl;
use tokio::sync::Notify;

/// 重启子进程标记：换来 runtime 绑定监听时的重试窗口（等旧进程释放端口）。
pub const RESTART_CHILD_ENV: &str = "PEGBOARD_RESTART_CHILD";

/// 以相同参数 spawn 当前可执行文件（透传 CLI 参数，配置文件改动随之生效）。
pub fn spawn_restart_child() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("定位可执行文件失败: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args().skip(1));
    cmd.env(RESTART_CHILD_ENV, "1");
    cmd.spawn().map(|_| ()).map_err(|e| format!("spawn 新进程失败: {e}"))
}

/// 无头模式的进程控制：直接 spawn 新进程，成功后通知本进程优雅退出。
/// spawn 失败时保持本进程存活（管理端拿到 500，服务不中断）。
pub struct HeadlessControl {
    notify: Arc<Notify>,
}

impl HeadlessControl {
    pub fn new(notify: Arc<Notify>) -> Self {
        Self { notify }
    }
}

impl ProcessControl for HeadlessControl {
    fn request_restart(&self) -> Result<(), String> {
        let result = spawn_restart_child();
        if result.is_ok() {
            self.notify.notify_waiters();
        }
        result
    }
}
