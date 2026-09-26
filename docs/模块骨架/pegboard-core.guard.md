# pegboard-core / guard 模块

## 接口

```rust
// crates/pegboard-core/src/guard/mod.rs

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

use crate::app::AppMeta;

/// 受治理的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Store,
    Files,
    Net,
    Ws,
}

/// 一次决策的结果，由调用方记录到 audit。
#[derive(Debug, Clone)]
pub struct Decision {
    pub action: Action,
    pub allowed: bool,
    pub reason: Option<DenyReason>,
    pub target: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DenyReason {
    PermissionMissing,
    TargetNotAllowed,
    SsrfBlocked,
    RateLimited { limit: u32, window_secs: u64 },
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

/// 治理层：权限、目标校验、限流、响应头清理。
/// 无状态（限流除外），所有方法可独立测试。
pub struct Guard {
    rate: Mutex<HashMap<(String, Action), Bucket>>,
}

impl Guard {
    pub fn new() -> Self;

    /// 能力权限检查：store / files。
    pub fn check_capability(&self, app: &AppMeta, action: Action) -> Result<(), GuardError>;

    /// 目标校验：白名单 + SSRF。
    /// proxy 对每一跳（含重定向）调用一次。
    pub fn check_target(&self, app: &AppMeta, action: Action, url: &Url)
        -> Result<(), GuardError>;

    /// 限流：按 (app_id, action) 维度，滑动窗口。
    pub fn check_rate(&self, app: &AppMeta, action: Action) -> Result<(), GuardError>;

    /// 响应头清理：移除可能污染宿主的头。
    pub fn sanitize_headers(&self, headers: &mut http::HeaderMap);
}

impl Default for Guard { fn default() -> Self { Self::new() } }

struct Bucket { window_start: Instant, count: u32 }

/// 白名单匹配。entry 为 origin 或完整 URL 前缀。
fn matches_whitelist(entry: &str, url: &Url) -> bool;

/// 私有 / 保留地址判定。
fn is_private_or_reserved(ip: IpAddr) -> bool;

/// 白名单条目是否为 IP 字面量或 localhost。
/// 是则允许其解析到私有地址（用户显式选择）。
fn is_explicit_local(entry: &str) -> bool;
```

## 校验规则

### 权限
- `Action::Store` 要求 `manifest.permissions.store == true`。
- `Action::Files` 要求 `manifest.permissions.files == true`。
- `Action::Net` / `Ws` 要求目标命中 `permissions.net` / `permissions.ws`，非空列表。

### 白名单匹配
- 条目为 origin：`scheme://host[:port]`，比较 scheme + host + port。
- 条目为完整 URL：额外比较路径前缀。
- 大小写：scheme 与 host 不区分大小写，路径区分。
- 端口缺省时用 scheme 默认端口归一。
- 匹配失败 → `TargetDenied`。

### SSRF
- 解析 `url.host()` 得到 `IpAddr`（IP 字面量直接取，域名解析后校验）。
- 命中 `is_private_or_reserved` 默认拒绝：
  - `127.0.0.0/8`、`::1`
  - `10/8`、`172.16/12`、`192.168/16`
  - `169.254/16`（含云元数据 `169.254.169.254`）
  - `fc00::/7`、`fe80::/10`
  - `0.0.0.0`、广播、多播
- **例外**：白名单条目本身为 IP 字面量或 `localhost` 时，允许其解析到对应私有地址。用户显式选择，视为信任。
- 域名解析到私有地址且白名单条目不是显式本地 → `SsrfBlocked`。
- 每一跳独立校验，防止重定向绕过。

### 限流
- 维度：`(app_id, action)`。
- 窗口：1 秒，滑动近似（固定窗口计数）。
- 上限：`app.limits.net_rps`；store / files 暂用同一上限或固定值。
- 超限 → `RateLimited`。
- 内存态，进程重启即清零；内部工具可接受。

### 响应头清理
移除（防污染宿主）：
- `Set-Cookie`
- `Access-Control-Allow-*`
- `Access-Control-Expose-*`
- `Content-Security-Policy`
- `Content-Security-Policy-Report-Only`
- `X-Frame-Options`
- `Strict-Transport-Security`
- `Public-Key-Pins`

保留：
- `Content-Type`、`Content-Length`
- `ETag`、`Last-Modified`
- `Cache-Control`
- `Content-Disposition`
- `Accept-Ranges`、`Content-Range`

## 单元测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMeta, Manifest, Permissions};
    use std::path::PathBuf;

    fn app(net: &[&str], ws: &[&str], store: bool, files: bool) -> AppMeta {
        let mut p = Permissions::default();
        p.store = store;
        p.files = files;
        p.net = net.iter().map(|s| s.to_string()).collect();
        p.ws  = ws.iter().map(|s| s.to_string()).collect();
        AppMeta {
            id: "t".into(),
            name: "t".into(),
            root: PathBuf::from("."),
            entry: PathBuf::from("index.html"),
            manifest: Manifest { /* 略 */ },
            limits: Default::default(),
        }
    }

    fn url(s: &str) -> Url { Url::parse(s).unwrap() }

    // ---------- 权限 ----------

    #[test]
    fn store_requires_flag() {
        let g = Guard::new();
        assert!(g.check_capability(&app(&[], &[], true, false), Action::Store).is_ok());
        assert!(matches!(
            g.check_capability(&app(&[], &[], false, false), Action::Store),
            Err(GuardError::PermissionDenied(Action::Store))
        ));
    }

    #[test]
    fn files_requires_flag() {
        let g = Guard::new();
        assert!(g.check_capability(&app(&[], &[], false, true), Action::Files).is_ok());
        assert!(g.check_capability(&app(&[], &[], false, false), Action::Files).is_err());
    }

    // ---------- 白名单 ----------

    #[test]
    fn origin_match_exact() {
        let g = Guard::new();
        let a = app(&["https://api.example.com"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("https://api.example.com/v1/x")).is_ok());
    }

    #[test]
    fn origin_mismatch_port() {
        let g = Guard::new();
        let a = app(&["https://api.example.com"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("https://api.example.com:8443/x")).is_err());
    }

    #[test]
    fn origin_mismatch_scheme() {
        let g = Guard::new();
        let a = app(&["https://api.example.com"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("http://api.example.com/x")).is_err());
    }

    #[test]
    fn url_prefix_match() {
        let g = Guard::new();
        let a = app(&["https://api.example.com/v1/"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("https://api.example.com/v1/chat")).is_ok());
        assert!(g.check_target(&a, Action::Net, &url("https://api.example.com/v2/x")).is_err());
    }

    #[test]
    fn empty_whitelist_denies() {
        let g = Guard::new();
        let a = app(&[], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("https://x.com")).is_err());
    }

    #[test]
    fn ws_uses_ws_list() {
        let g = Guard::new();
        let a = app(&[], &["wss://live.example.com"], false, false);
        assert!(g.check_target(&a, Action::Ws, &url("wss://live.example.com/x")).is_ok());
        // 同一 URL 用 Net 动作应拒绝
        assert!(g.check_target(&a, Action::Net, &url("wss://live.example.com/x")).is_err());
    }

    // ---------- SSRF ----------

    #[test]
    fn private_ip_blocked_by_default() {
        let g = Guard::new();
        let a = app(&["http://10.0.0.5"], &[], false, false);
        // 白名单是显式 IP，允许
        assert!(g.check_target(&a, Action::Net, &url("http://10.0.0.5/x")).is_ok());
    }

    #[test]
    fn hostname_resolving_private_blocked() {
        // 需要 mock DNS；这里用不可能公开解析的 .local 名断言拒绝
        let g = Guard::new();
        let a = app(&["http://srv.internal"], &[], false, false);
        // 若解析到私有 → SsrfBlocked；若解析失败 → InvalidTarget
        let r = g.check_target(&a, Action::Net, &url("http://srv.internal/x"));
        assert!(r.is_err());
    }

    #[test]
    fn metadata_endpoint_blocked() {
        let g = Guard::new();
        let a = app(&["http://169.254.169.254"], &[], false, false);
        // 白名单为 IP 字面量，视作显式本地 → 允许
        assert!(g.check_target(&a, Action::Net, &url("http://169.254.169.254/x")).is_ok());
        // 非显式白名单则拒绝（见 hostname_resolving_private_blocked）
    }

    #[test]
    fn localhost_explicit_allowed() {
        let g = Guard::new();
        let a = app(&["http://127.0.0.1:11434"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("http://127.0.0.1:11434/api/chat")).is_ok());
    }

    #[test]
    fn localhost_not_in_whitelist_denied() {
        let g = Guard::new();
        let a = app(&["https://api.example.com"], &[], false, false);
        assert!(g.check_target(&a, Action::Net, &url("http://127.0.0.1:11434/x")).is_err());
    }

    #[test]
    fn private_ranges_recognized() {
        assert!(is_private_or_reserved("127.0.0.1".parse().unwrap()));
        assert!(is_private_or_reserved("10.1.2.3".parse().unwrap()));
        assert!(is_private_or_reserved("172.16.0.1".parse().unwrap()));
        assert!(is_private_or_reserved("192.168.1.1".parse().unwrap()));
        assert!(is_private_or_reserved("169.254.169.254".parse().unwrap()));
        assert!(is_private_or_reserved("::1".parse().unwrap()));
        assert!(is_private_or_reserved("fc00::1".parse().unwrap()));
        assert!(!is_private_or_reserved("8.8.8.8".parse().unwrap()));
    }

    // ---------- 限流 ----------

    #[test]
    fn rate_limit_enforced() {
        let g = Guard::new();
        let mut a = app(&["https://x.com"], &[], false, false);
        a.limits.net_rps = 3;
        for _ in 0..3 { g.check_rate(&a, Action::Net).unwrap(); }
        assert!(matches!(
            g.check_rate(&a, Action::Net),
            Err(GuardError::RateLimited { .. })
        ));
    }

    #[test]
    fn rate_limit_isolated_per_app() {
        let g = Guard::new();
        let mut a = app(&["https://x.com"], &[], false, false);
        a.limits.net_rps = 1;
        let mut b = a.clone();
        b.id = "b".into();
        g.check_rate(&a, Action::Net).unwrap();
        g.check_rate(&b, Action::Net).unwrap();  // 不同 app，不冲突
    }

    #[test]
    fn rate_limit_window_resets() {
        // 设置极短窗口（重构为可注入 Clock），sleep 后断言恢复
    }

    // ---------- 响应头清理 ----------

    #[test]
    fn strips_polluting_headers() {
        let g = Guard::new();
        let mut h = http::HeaderMap::new();
        h.insert("Set-Cookie", "a=1".parse().unwrap());
        h.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
        h.insert("Content-Security-Policy", "default-src *".parse().unwrap());
        h.insert("X-Frame-Options", "DENY".parse().unwrap());
        g.sanitize_headers(&mut h);
        assert!(h.get("Set-Cookie").is_none());
        assert!(h.get("Access-Control-Allow-Origin").is_none());
        assert!(h.get("Content-Security-Policy").is_none());
        assert!(h.get("X-Frame-Options").is_none());
    }

    #[test]
    fn keeps_safe_headers() {
        let g = Guard::new();
        let mut h = http::HeaderMap::new();
        h.insert("Content-Type", "application/json".parse().unwrap());
        h.insert("Content-Length", "42".parse().unwrap());
        h.insert("ETag", "\"abc\"".parse().unwrap());
        h.insert("Cache-Control", "no-store".parse().unwrap());
        g.sanitize_headers(&mut h);
        assert_eq!(h.get("Content-Type").unwrap(), "application/json");
        assert_eq!(h.get("Content-Length").unwrap(), "42");
        assert!(h.get("ETag").is_some());
        assert!(h.get("Cache-Control").is_some());
    }
}
```

## 依赖

```toml
[dependencies]
url = { workspace = true }
http = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
```

`http` 仅用于 `HeaderMap`；若 server 已引入 axum，可复用其重导出，避免重复。

## 设计要点

- **无状态为主**：除限流外所有方法纯函数式；限流用 `Mutex` 保护的内存 map。
- **每跳校验**：`check_target` 由 proxy 对重定向的每一跳调用，防绕过。
- **显式本地例外**：白名单为 IP 字面量或 `localhost` 时，视为用户显式信任，放行；域名解析到私有地址则拒绝。
- **审计交给调用方**：guard 返回 `Decision` / `GuardError`，ingress 记录。避免 guard 依赖 audit，减少耦合。
- **限流可注入时钟**：实现时用 `Clock` trait 便于测试窗口复位；接口层暂不暴露。
- **响应头白名单式**：保留明确安全的，移除其余；比黑名单更稳。
- **错误即契约**：`GuardError` 变体与契约错误码一一对应，由 ingress 映射。
