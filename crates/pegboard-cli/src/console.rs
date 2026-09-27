//! Windows 控制台重接：`windows_subsystem = "windows"` 下 stdout/stderr 默认无效。
//! 从 cmd/PowerShell 启动（有父 console）时 attach 上并重绑 std 句柄，让
//! 横幅 / --help / panic 输出可见；双击启动（无父 console）时失败静默跳过。
//!
//! 返回 true 表示当前进程有可用 stdout（已有 console 或 attach 成功）。

#[cfg(target_os = "windows")]
pub fn attach_parent_console() -> bool {
    use std::ptr;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    unsafe {
        // 已有 stdout（console 子系统直接启动，或 debug 模式）→ 无需 attach
        let existing = GetStdHandle(STD_OUTPUT_HANDLE);
        if !existing.is_null() && existing != INVALID_HANDLE_VALUE {
            return true;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return false; // 双击启动，无父 console
        }
        let name = b"CONOUT$\0";
        let out = CreateFileA(
            name.as_ptr(),
            (GENERIC_READ | GENERIC_WRITE) as u32,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if out.is_null() || out == INVALID_HANDLE_VALUE {
            return false;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, out);
        SetStdHandle(STD_ERROR_HANDLE, out);
        true
    }
}
