//! 系统托盘：常驻菜单（打开管理页 / 打开应用目录 / 退出）。
//!
//! 线程模型（macOS NSApplication 约束，Windows/Linux 同构）：
//! - 主线程：tao 事件循环 + tray-icon（图标与菜单事件由事件循环线程持有）。
//! - 子线程：tokio runtime + axum 服务。
//!
//! 协调：托盘「退出」或服务端重启请求 → `Notify` 通知服务优雅关闭 →
//! 事件循环轮询「服务已停止」标志（审计排空完成后置位，12s 超时兜底）→
//! 进程结束。tao 的 `event_loop.run()` 不返回（`-> !`），进程在其内部结束。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pegboard_core::config::Config;
use pegboard_server::host::ProcessControl;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tokio::sync::Notify;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::TrayIconBuilder;

/// 等待服务收尾（含 10s 审计排空上限）的兜底上限。
const STOP_WAIT: Duration = Duration::from_secs(12);
/// 服务停止标志的轮询间隔。
const STOP_POLL: Duration = Duration::from_millis(100);

/// 服务线程写入实际监听地址（端口 0 时决定管理页 URL），托盘读取。
static ACTUAL_ADDR: std::sync::OnceLock<std::net::SocketAddr> = std::sync::OnceLock::new();

/// 服务线程 → 事件循环的用户事件。
#[derive(Debug)]
enum TrayEvent {
    /// 重启子进程已 spawn（spawn 在调用线程同步完成）：通知服务关闭并结束本进程。
    ShutdownForRestart,
    /// 托盘菜单点击。
    Menu(MenuEvent),
}

/// 托盘模式的进程控制：同步 spawn 新进程（结果与 HTTP 响应一致），成功后
/// 发事件让主线程执行退出协调。
struct TrayControl {
    proxy: EventLoopProxy<TrayEvent>,
}

impl ProcessControl for TrayControl {
    fn request_restart(&self) -> Result<(), String> {
        let spawned = crate::process::spawn_restart_child();
        if spawned.is_ok() {
            // 进程已在退出路径上时 send 失败可忽略
            let _ = self.proxy.send_event(TrayEvent::ShutdownForRestart);
        }
        spawned
    }
}

/// 托盘模式编排：主线程事件循环 + 子线程 tokio 服务。正常情况下本函数
/// 不返回（tao 在事件循环内部结束进程）；事件循环不可用（无 GUI 会话 panic）
/// 由调用方 catch_unwind 降级为无头模式。
pub fn serve_with_tray(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoopBuilder::<TrayEvent>::with_user_event().build();

    #[cfg(target_os = "macos")]
    {
        // 菜单栏应用形态：Accessory 策略 + 隐藏 Dock 图标。不显式覆盖时 tao
        // 会按 Regular 拉起，右键 Dock 的退出会绕过我们的收尾流程。
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut loop_mut = event_loop;
        loop_mut.set_activation_policy(ActivationPolicy::Accessory);
        loop_mut.set_dock_visibility(false);
        run_event_loop(loop_mut, config)
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_event_loop(event_loop, config)
    }
}

#[allow(unused_assignments)] // tray_icon 持有于闭包内，赋值后不再读取是持有语义
fn run_event_loop(event_loop: EventLoop<TrayEvent>, config: Config) -> ! {
    // 菜单「打开应用目录」用；config 本体 move 进服务线程
    let apps_dir = config.storage.apps_dir.clone();
    let notify = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let proxy = event_loop.create_proxy();

    // 服务子线程：结束（含审计排空）后置位 stopped；致命失败直接退出进程
    {
        let notify = Arc::clone(&notify);
        let stopped = Arc::clone(&stopped);
        let proxy = proxy.clone();
        let control: Arc<dyn ProcessControl> = Arc::new(TrayControl { proxy: proxy.clone() });
        std::thread::Builder::new()
            .name("pegboard-server".to_owned())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "tokio runtime 初始化失败");
                        std::process::exit(1);
                    }
                };
                let result = rt.block_on(async move {
                    let runtime = match crate::runtime::build(config, control).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!(error = %e, "宿主装配失败");
                            std::process::exit(1);
                        }
                    };
                    if let Ok(addr) = runtime.listener.local_addr() {
                        let _ = ACTUAL_ADDR.set(addr);
                    }
                    crate::runtime::serve(runtime, Some(notify), Some(stopped)).await
                });
                if let Err(e) = result {
                    tracing::error!(error = %e, "server error");
                    std::process::exit(1);
                }
                // serve 正常返回（信号路径）→ 事件循环兜底退出
                let _ = proxy.send_event(TrayEvent::ShutdownForRestart);
            })
            .map_err(|e| format!("启动服务线程失败: {e}"))
            .unwrap_or_else(|e| {
                // 线程起不来：无服务可托管，进程即失败
                tracing::error!(error = %e, "启动服务线程失败");
                std::process::exit(1);
            });
    }

    // 菜单（事件循环线程构建）
    let menu = Menu::new();
    let open_admin = MenuItem::new("打开管理页", true, None);
    let open_apps = MenuItem::new("打开应用目录", true, None);
    let quit = MenuItem::new("退出", true, None);
    let _ = menu.append_items(&[&open_admin, &open_apps, &PredefinedMenuItem::separator(), &quit]);

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = menu_proxy.send_event(TrayEvent::Menu(e));
    }));

    // 退出协调状态；tray_icon 由本闭包持有至事件循环结束（drop 即图标消失）
    // 前缀下划线不改变语义：值仍被持有至事件循环结束（drop 即图标消失）
    let mut _tray_icon: Option<tray_icon::TrayIcon> = None;
    let mut quitting = false;
    let mut quit_deadline = Instant::now();
    let mut next_poll = Instant::now();

    event_loop.run(move |event, _, control_flow| {
        // 每轮重置：退出协调中按 STOP_POLL 轮询，平时长眠等事件
        *control_flow = if quitting {
            ControlFlow::WaitUntil(next_poll)
        } else {
            ControlFlow::Wait
        };
        match event {
            Event::NewEvents(tao::event::StartCause::Init) => {
                match build_tray_icon(&menu) {
                    Ok(icon) => _tray_icon = Some(icon),
                    Err(e) => {
                        tracing::error!(error = %e, "托盘图标创建失败，退出");
                        notify.notify_waiters();
                        std::process::exit(1);
                    }
                }
                tracing::info!("tray ready");
            }
            Event::NewEvents(tao::event::StartCause::ResumeTimeReached { .. }) if quitting => {
                if stopped.load(Ordering::Acquire) || Instant::now() >= quit_deadline {
                    if !stopped.load(Ordering::Acquire) {
                        tracing::warn!("服务收尾超时，强制退出");
                    }
                    *control_flow = ControlFlow::Exit;
                } else {
                    next_poll = Instant::now() + STOP_POLL;
                    *control_flow = ControlFlow::WaitUntil(next_poll);
                }
            }
            Event::UserEvent(TrayEvent::ShutdownForRestart) => {
                begin_quit(&mut quitting, &mut quit_deadline, &notify);
            }
            Event::UserEvent(TrayEvent::Menu(e)) => {
                if e.id == open_admin.id() {
                    match admin_url() {
                        Some(url) => open_in_browser(&url),
                        None => tracing::warn!("管理页地址未知（服务尚未就绪）"),
                    }
                } else if e.id == open_apps.id() {
                    open_in_file_manager(&apps_dir);
                } else if e.id == quit.id() {
                    tracing::info!("tray quit requested");
                    begin_quit(&mut quitting, &mut quit_deadline, &notify);
                }
            }
            _ => {}
        }
    })
}

fn begin_quit(quitting: &mut bool, deadline: &mut Instant, notify: &Arc<Notify>) {
    if *quitting {
        return; // 已在退出协调中，不重复通知
    }
    *quitting = true;
    *deadline = Instant::now() + STOP_WAIT;
    notify.notify_waiters();
}

fn build_tray_icon(menu: &Menu) -> Result<tray_icon::TrayIcon, String> {
    // 原生 RGBA 资产（64×64）：免 image 解码依赖
    const SIZE: u32 = 64;
    const RGBA: &[u8] = include_bytes!("../assets/tray-icon.rgba");
    if RGBA.len() != (SIZE * SIZE * 4) as usize {
        return Err(format!("tray-icon.rgba 尺寸异常: {} bytes", RGBA.len()));
    }
    let icon = tray_icon::Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE)
        .map_err(|e| format!("icon: {e}"))?;
    TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip("Pegboard 宿主")
        .with_icon(icon)
        .build()
        .map_err(|e| format!("tray build: {e}"))
}

/// 管理页 URL：实际监听地址回环化（0.0.0.0 → 127.0.0.1）。
fn admin_url() -> Option<String> {
    let addr = ACTUAL_ADDR.get()?;
    let host = if addr.ip().is_loopback() {
        addr.ip().to_string()
    } else {
        "127.0.0.1".to_owned()
    };
    Some(format!("http://{host}:{}/admin", addr.port()))
}

/// 用系统默认浏览器打开 URL。失败只打日志——托盘应用没有 UI 兜底。
/// Windows 经 `cmd /C start`：占位 "" 防止 URL 被当窗口标题；CREATE_NO_WINDOW
/// 防止 GUI 子系统父进程下的 cmd 闪黑窗。
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
            tracing::warn!(url, error = %e, "打开浏览器失败");
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        if let Err(e) = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            tracing::warn!(url, error = %e, "打开浏览器失败");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Err(e) = std::process::Command::new("xdg-open").arg(url).spawn() {
            tracing::warn!(url, error = %e, "打开浏览器失败");
        }
    }
}

/// 在系统文件管理器里打开目录。
fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    if let Err(e) = std::process::Command::new(program).arg(path).spawn() {
        tracing::warn!(path = %path.display(), error = %e, "打开目录失败");
    }
}
