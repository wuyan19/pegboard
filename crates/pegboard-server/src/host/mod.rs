//! 进程编排接口：宿主进程的重启控制。
//!
//! 服务层只声明重启意图；spawn 新进程与触发本进程退出的实现属于进程层（cli）：
//! 托盘模式下必须由托盘事件循环 spawn（tao 的 `event_loop.run()` 不返回，
//! 退出路径只能在其事件处理器内），无头模式由 cli 直接 spawn。服务层不关心差异。

pub mod update;

/// 宿主进程控制。实现必须可在任意线程调用（axum handler 触发）。
pub trait ProcessControl: Send + Sync {
    /// 请求以相同启动参数重启宿主进程；成功返回后本进程将优雅退出
    /// （实现负责 spawn 新进程与通知关闭）。失败时本进程保持存活。
    fn request_restart(&self) -> Result<(), String>;
}

/// 空实现：单测与「重启通道不可用」场景（如进程由外部监管器拉起时也可换成拒绝实现）。
pub struct NoControl;

impl ProcessControl for NoControl {
    fn request_restart(&self) -> Result<(), String> {
        Err("当前运行形态不支持进程内重启".to_owned())
    }
}
