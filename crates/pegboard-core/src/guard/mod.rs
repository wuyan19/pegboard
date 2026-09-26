//! 治理层：权限校验（M3 能力部分）；目标白名单 + SSRF + 限流 + 头清理随 M4 接入。
//!
//! 无状态；不做授权决策（那是应用的事）、不感知业务数据。
//! guard 不依赖 audit：决策结果交调用方记录（模块划分 §4）。

use crate::app::AppMeta;

/// 受治理的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Store,
    Files,
    Net,
    Ws,
}

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("permission denied: {0:?}")]
    PermissionDenied(Action),
    #[error("target denied: {0}")]
    TargetDenied(String),
    #[error("ssrf blocked: {0}")]
    SsrfBlocked(String),
    #[error("rate limited: {limit} per {window_secs}s")]
    RateLimited { limit: u32, window_secs: u64 },
    #[error("invalid target: {0}")]
    InvalidTarget(String),
}

/// 治理层。M3：能力权限检查。
pub struct Guard {
    _reserved: (),
}

impl Guard {
    pub fn new() -> Self {
        Self { _reserved: () }
    }

    /// 能力权限检查：store / files 布尔标志；
    /// Net / Ws 要求对应白名单非空（目标匹配在 M4 的 check_target）。
    pub fn check_capability(&self, app: &AppMeta, action: Action) -> Result<(), GuardError> {
        let p = &app.manifest.permissions;
        let ok = match action {
            Action::Store => p.store,
            Action::Files => p.files,
            Action::Net => !p.net.is_empty(),
            Action::Ws => !p.ws.is_empty(),
        };
        if ok {
            Ok(())
        } else {
            Err(GuardError::PermissionDenied(action))
        }
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMeta, LimitsOverride, Manifest, Permissions};
    use std::path::PathBuf;

    fn app(net: &[&str], ws: &[&str], store: bool, files: bool) -> AppMeta {
        AppMeta {
            id: "t".into(),
            name: "t".into(),
            root: PathBuf::from("."),
            entry: PathBuf::from("index.html"),
            manifest: Manifest {
                id: "t".into(),
                name: "t".into(),
                entry: "index.html".into(),
                permissions: Permissions {
                    store,
                    files,
                    net: net.iter().map(|s| s.to_string()).collect(),
                    ws: ws.iter().map(|s| s.to_string()).collect(),
                    shim: true,
                },
                limits: LimitsOverride::default(),
            },
            limits: crate::config::Limits::default(),
        }
    }

    #[test]
    fn store_requires_flag() {
        let g = Guard::new();
        assert!(g
            .check_capability(&app(&[], &[], true, false), Action::Store)
            .is_ok());
        assert!(matches!(
            g.check_capability(&app(&[], &[], false, false), Action::Store),
            Err(GuardError::PermissionDenied(Action::Store))
        ));
    }

    #[test]
    fn files_requires_flag() {
        let g = Guard::new();
        assert!(g
            .check_capability(&app(&[], &[], false, true), Action::Files)
            .is_ok());
        assert!(g
            .check_capability(&app(&[], &[], false, false), Action::Files)
            .is_err());
    }

    #[test]
    fn net_requires_nonempty_list() {
        let g = Guard::new();
        assert!(g
            .check_capability(&app(&["https://x.com"], &[], false, false), Action::Net)
            .is_ok());
        assert!(g
            .check_capability(&app(&[], &[], false, false), Action::Net)
            .is_err());
    }

    #[test]
    fn ws_requires_nonempty_list() {
        let g = Guard::new();
        assert!(g
            .check_capability(&app(&[], &["wss://x.com"], false, false), Action::Ws)
            .is_ok());
        assert!(g
            .check_capability(&app(&[], &[], false, false), Action::Ws)
            .is_err());
    }
}
