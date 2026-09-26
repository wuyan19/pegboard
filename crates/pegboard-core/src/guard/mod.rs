//! 治理层：权限、目标白名单 + SSRF、限流、请求/响应头双向清理。
//!
//! 无状态（限流除外）；不做授权决策、不感知业务数据。
//! guard 不依赖 audit：决策结果交调用方记录（模块划分 §4）。
//! proxy 对每一跳（含重定向）调用 check_target（SSRF 防护关键）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

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

/// 头清理方向：出站（宿主 → 目标）与入站（目标 → 宿主）。
/// proxy / asset / ws-proxy 三入口共用（模块划分 §4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outbound,
    Inbound,
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

/// 限流窗口：1 秒固定窗口近似滑动。
const RATE_WINDOW_SECS: u64 = 1;

/// 治理层。限流为内存态，进程重启清零（内部工具可接受）。
pub struct Guard {
    rate: Mutex<HashMap<(String, Action), Bucket>>,
    /// 身份注入头（与 identity 配置同清单）：出站剥离，防伪造穿透。
    injected_headers: Vec<String>,
    /// 可注入时钟（测试窗口复位用）。
    clock: fn() -> Instant,
}

struct Bucket {
    window_start: Instant,
    count: u32,
}

impl Guard {
    pub fn new() -> Self {
        Self::with_injected(Vec::new())
    }

    /// 携带身份注入头清单（来自 config.identity.injected_headers）。
    pub fn with_injected(injected_headers: Vec<String>) -> Self {
        Self {
            rate: Mutex::new(HashMap::new()),
            injected_headers,
            clock: Instant::now,
        }
    }

    /// 测试用：注入时钟。
    pub fn with_clock(injected_headers: Vec<String>, clock: fn() -> Instant) -> Self {
        Self {
            rate: Mutex::new(HashMap::new()),
            injected_headers,
            clock,
        }
    }

    /// 能力权限检查：store / files 布尔标志；
    /// Net / Ws 要求对应白名单非空（目标匹配见 check_target）。
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

    /// 目标校验：白名单匹配 + SSRF 防护。proxy 对每一跳调用。
    pub async fn check_target(
        &self,
        app: &AppMeta,
        action: Action,
        url: &Url,
    ) -> Result<(), GuardError> {
        let entries = match action {
            Action::Net => &app.manifest.permissions.net,
            Action::Ws => &app.manifest.permissions.ws,
            other => {
                return Err(GuardError::InvalidTarget(format!(
                    "{other:?} 动作不承载目标校验"
                )))
            }
        };
        let target = url.as_str();
        let matched = entries
            .iter()
            .find(|entry| matches_whitelist(entry, url, action));
        let Some(entry) = matched else {
            return Err(GuardError::TargetDenied(target.to_owned()));
        };
        // SSRF：私有/保留地址默认拒绝；白名单条目为 IP 字面量或 localhost 视为显式信任
        self.check_ssrf(url, entry).await
    }

    async fn check_ssrf(&self, url: &Url, whitelist_entry: &str) -> Result<(), GuardError> {
        let host = url.host_str().unwrap_or_default().to_owned();
        let port = url.port_or_known_default().unwrap_or(0);
        let explicit_local = is_explicit_local(whitelist_entry);

        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_private_or_reserved(ip) && !explicit_local {
                return Err(GuardError::SsrfBlocked(format!(
                    "{ip}（白名单条目非显式本地）"
                )));
            }
            return Ok(());
        }
        if host.eq_ignore_ascii_case("localhost") {
            if explicit_local {
                return Ok(());
            }
            return Err(GuardError::SsrfBlocked(
                "localhost（白名单条目非显式本地）".into(),
            ));
        }
        // 域名：解析后任一地址命中私网即拒绝（除非显式本地）
        let lookup = tokio::net::lookup_host((host.as_str(), port)).await;
        match lookup {
            Ok(addrs) => {
                let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
                if ips.is_empty() {
                    Err(GuardError::InvalidTarget("域名解析无结果".into()))
                } else if ips.iter().any(|ip| is_private_or_reserved(*ip)) && !explicit_local {
                    Err(GuardError::SsrfBlocked(format!(
                        "域名解析到私有地址 {:?}",
                        ips.iter().map(ToString::to_string).collect::<Vec<_>>()
                    )))
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(GuardError::InvalidTarget(format!("域名解析失败: {e}"))),
        }
    }

    /// 限流：按 (app_id, action) 维度固定 1s 窗口。
    /// 仅网络能力调用（数据模型 §8：store/files 不限频）。
    pub fn check_rate(&self, app: &AppMeta, action: Action) -> Result<(), GuardError> {
        let limit = app.limits.net_rps;
        let key = (app.id.clone(), action);
        let now = (self.clock)();
        let mut map = self.rate.lock().unwrap_or_else(|p| p.into_inner());
        let bucket = map.entry(key).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(bucket.window_start).as_secs() >= RATE_WINDOW_SECS {
            bucket.window_start = now;
            bucket.count = 0;
        }
        if bucket.count >= limit {
            return Err(GuardError::RateLimited {
                limit,
                window_secs: RATE_WINDOW_SECS,
            });
        }
        bucket.count += 1;
        Ok(())
    }

    /// 头清理（双向）。出站：剥离 Cookie / Host / X-Forwarded-* / 身份注入头 /
    /// 逐跳头；入站：剥离可能污染宿主的响应头（API 契约 §5）。
    pub fn sanitize(&self, headers: &mut http::HeaderMap, direction: Direction) {
        match direction {
            Direction::Outbound => {
                for name in [
                    http::header::COOKIE,
                    http::header::HOST,
                    http::header::CONNECTION,
                    http::header::CONTENT_LENGTH,
                    http::header::TRANSFER_ENCODING,
                    http::header::UPGRADE,
                    http::header::PROXY_AUTHORIZATION,
                    http::header::TE,
                    http::header::TRAILER,
                ] {
                    headers.remove(name);
                }
                // X-Forwarded-* 与身份注入头
                let names: Vec<String> = headers.keys().map(|k| k.as_str().to_owned()).collect();
                for name in names {
                    let lower = name.to_ascii_lowercase();
                    if lower.starts_with("x-forwarded-")
                        || lower == "x-pegboard-app"
                        || self
                            .injected_headers
                            .iter()
                            .any(|h| h.eq_ignore_ascii_case(&lower))
                    {
                        headers.remove(name.as_str());
                    }
                }
            }
            Direction::Inbound => {
                for name in [
                    "set-cookie",
                    "access-control-allow-origin",
                    "access-control-allow-methods",
                    "access-control-allow-headers",
                    "access-control-allow-credentials",
                    "access-control-expose-headers",
                    "access-control-max-age",
                    "content-security-policy",
                    "content-security-policy-report-only",
                    "x-frame-options",
                    "strict-transport-security",
                    "public-key-pins",
                    "public-key-pins-report-only",
                    "connection",
                    "transfer-encoding",
                    "keep-alive",
                ] {
                    headers.remove(name);
                }
            }
        }
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self::new()
    }
}

/// 白名单匹配：条目为 origin（scheme+host[:port]）或完整 URL 前缀。
/// scheme/host 大小写不敏感（Url 已归一），路径区分大小写；端口按 scheme 默认归一。
fn matches_whitelist(entry: &str, url: &Url, action: Action) -> bool {
    let Ok(parsed_entry) = Url::parse(entry) else {
        return false;
    };
    let expected_scheme: [&str; 2] = match action {
        Action::Ws => ["ws", "wss"],
        _ => ["http", "https"],
    };
    if !expected_scheme.contains(&parsed_entry.scheme()) || parsed_entry.scheme() != url.scheme() {
        return false;
    }
    if parsed_entry.host_str() != url.host_str() {
        return false;
    }
    if parsed_entry.port_or_known_default() != url.port_or_known_default() {
        return false;
    }
    let entry_path = parsed_entry.path();
    if entry_path != "/" && !entry_path.is_empty() {
        return url.path().starts_with(entry_path);
    }
    true
}

/// 私有 / 保留地址判定（需求 §7：内网、本机、云元数据默认拒绝）。
fn is_private_or_reserved(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() // 127/8
                || v4.is_private() // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254/16（含云元数据）
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_broadcast() // 255.255.255.255
                || v4.is_multicast() // 224/4
                || v4.is_documentation() // 192.0.2/24 等
                || (o[0] == 100 && o[1] >= 64 && o[1] <= 127) // 100.64/10 CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地
                || v6.is_multicast()
        }
    }
}

/// 白名单条目是否为 IP 字面量或 localhost：用户显式选择，允许私网目标。
fn is_explicit_local(entry: &str) -> bool {
    match Url::parse(entry) {
        Ok(url) => match url.host_str() {
            Some(host) => host.parse::<IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost"),
            None => false,
        },
        Err(_) => false,
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

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    // ---------- 权限 ----------

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

    // ---------- 白名单 ----------

    #[tokio::test]
    async fn origin_match_exact() {
        // IP 字面量白名单：不依赖 DNS，覆盖 匹配 + SSRF 放行 全链路
        let g = Guard::new();
        let a = app(&["http://127.0.0.1:9000"], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://127.0.0.1:9000/v1/x"))
            .await
            .is_ok());
    }

    #[test]
    fn whitelist_domain_matching() {
        // 域名白名单的匹配矩阵（纯函数，无 DNS）
        let e = "https://api.example.com";
        assert!(matches_whitelist(
            e,
            &url("https://api.example.com/v1/x"),
            Action::Net
        ));
        assert!(matches_whitelist(
            e,
            &url("https://api.example.com"),
            Action::Net
        ));
        assert!(!matches_whitelist(
            e,
            &url("https://api.example.com:8443/x"),
            Action::Net
        ));
        assert!(!matches_whitelist(
            e,
            &url("http://api.example.com/x"),
            Action::Net
        ));
        assert!(!matches_whitelist(
            e,
            &url("https://other.example.com/x"),
            Action::Net
        ));
        let p = "https://api.example.com/v1/";
        assert!(matches_whitelist(
            p,
            &url("https://api.example.com/v1/chat"),
            Action::Net
        ));
        assert!(!matches_whitelist(
            p,
            &url("https://api.example.com/v2/x"),
            Action::Net
        ));
        // 大小写：host 归一
        assert!(matches_whitelist(
            "https://API.Example.com",
            &url("https://api.example.com/x"),
            Action::Net
        ));
    }

    #[tokio::test]
    async fn origin_mismatch_port() {
        let g = Guard::new();
        let a = app(&["http://127.0.0.1:9000"], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://127.0.0.1:9001/x"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn empty_whitelist_denies() {
        let g = Guard::new();
        let a = app(&[], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://127.0.0.1:9000"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn ws_uses_ws_list() {
        let g = Guard::new();
        let a = app(&[], &["ws://127.0.0.1:9000"], false, false);
        assert!(g
            .check_target(&a, Action::Ws, &url("ws://127.0.0.1:9000/x"))
            .await
            .is_ok());
        // 同一 URL 用 Net 动作应拒绝（scheme 不匹配 http/https 白名单）
        assert!(g
            .check_target(&a, Action::Net, &url("ws://127.0.0.1:9000/x"))
            .await
            .is_err());
    }

    // ---------- SSRF ----------

    #[tokio::test]
    async fn explicit_local_ip_allowed() {
        let g = Guard::new();
        let a = app(&["http://127.0.0.1:11434"], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://127.0.0.1:11434/api/chat"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn localhost_explicit_allowed() {
        let g = Guard::new();
        let a = app(&["http://localhost:11434"], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://localhost:11434/api/chat"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn localhost_not_in_whitelist_denied() {
        let g = Guard::new();
        let a = app(&["https://api.example.com"], &[], false, false);
        // 白名单是域名（非显式本地）；目标 127.0.0.1 不匹配白名单 → TargetDenied
        assert!(g
            .check_target(&a, Action::Net, &url("http://127.0.0.1:11434/x"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn metadata_ip_whitelisted_is_explicit_local() {
        // 显式白名单 169.254.169.254 视作用户显式选择（骨架语义）；
        // 非显式场景由域名解析路径拒绝
        let g = Guard::new();
        let a = app(&["http://169.254.169.254"], &[], false, false);
        assert!(g
            .check_target(&a, Action::Net, &url("http://169.254.169.254/latest/meta"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn private_ip_via_domain_blocked() {
        // 不可能公开解析的保留域名：解析失败或私网 → 拒绝
        let g = Guard::new();
        let a = app(&["http://invalid.pegboard-test"], &[], false, false);
        let r = g
            .check_target(&a, Action::Net, &url("http://invalid.pegboard-test/x"))
            .await;
        assert!(r.is_err());
    }

    #[test]
    fn private_ranges_recognized() {
        assert!(is_private_or_reserved("127.0.0.1".parse().unwrap()));
        assert!(is_private_or_reserved("10.1.2.3".parse().unwrap()));
        assert!(is_private_or_reserved("172.16.0.1".parse().unwrap()));
        assert!(is_private_or_reserved("192.168.1.1".parse().unwrap()));
        assert!(is_private_or_reserved("169.254.169.254".parse().unwrap()));
        assert!(is_private_or_reserved("0.0.0.0".parse().unwrap()));
        assert!(is_private_or_reserved("::1".parse().unwrap()));
        assert!(is_private_or_reserved("fc00::1".parse().unwrap()));
        assert!(is_private_or_reserved("fe80::1".parse().unwrap()));
        assert!(!is_private_or_reserved("8.8.8.8".parse().unwrap()));
        assert!(!is_private_or_reserved("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn explicit_local_detection() {
        assert!(is_explicit_local("http://127.0.0.1:11434"));
        assert!(is_explicit_local("http://localhost"));
        assert!(is_explicit_local("http://10.0.0.5"));
        assert!(!is_explicit_local("https://api.example.com"));
        assert!(!is_explicit_local("http://internal.corp"));
    }

    // ---------- 限流 ----------

    #[test]
    fn rate_limit_enforced() {
        let g = Guard::new();
        let mut a = app(&["https://x.com"], &[], false, false);
        a.limits.net_rps = 3;
        for _ in 0..3 {
            g.check_rate(&a, Action::Net).unwrap();
        }
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
        g.check_rate(&b, Action::Net).unwrap(); // 不同 app，不冲突
    }

    /// 测试时钟偏移（毫秒）：fn 指针无法捕获环境，用模块级 static。
    static CLOCK_OFFSET_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

    fn test_clock() -> Instant {
        let ms = CLOCK_OFFSET_MS.load(std::sync::atomic::Ordering::SeqCst);
        Instant::now() + std::time::Duration::from_millis(ms as u64)
    }

    #[test]
    fn rate_limit_window_resets() {
        CLOCK_OFFSET_MS.store(0, std::sync::atomic::Ordering::SeqCst);
        let g = Guard::with_clock(Vec::new(), test_clock);
        let mut a = app(&["https://x.com"], &[], false, false);
        a.limits.net_rps = 2;
        g.check_rate(&a, Action::Net).unwrap();
        g.check_rate(&a, Action::Net).unwrap();
        assert!(g.check_rate(&a, Action::Net).is_err());
        CLOCK_OFFSET_MS.store(1100, std::sync::atomic::Ordering::SeqCst);
        g.check_rate(&a, Action::Net).unwrap();
        CLOCK_OFFSET_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    // ---------- 头清理 ----------

    #[test]
    fn outbound_strips_cookie_host_forwarded_and_injected() {
        let g = Guard::with_injected(vec!["x-forwarded-user".into()]);
        let mut h = http::HeaderMap::new();
        h.insert(http::header::COOKIE, "session=x".parse().unwrap());
        h.insert(http::header::HOST, "evil".parse().unwrap());
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        h.insert("x-forwarded-user", "alice".parse().unwrap());
        h.insert("x-pegboard-app", "t".parse().unwrap());
        h.insert(http::header::AUTHORIZATION, "Bearer k".parse().unwrap());
        h.insert(http::header::ORIGIN, "http://t.local".parse().unwrap());
        g.sanitize(&mut h, Direction::Outbound);
        assert!(h.get(http::header::COOKIE).is_none());
        assert!(h.get(http::header::HOST).is_none());
        assert!(h.get("x-forwarded-for").is_none());
        assert!(h.get("x-forwarded-user").is_none());
        assert!(h.get("x-pegboard-app").is_none());
        // 应用显式凭证与真实来源保留（API 契约 §5）
        assert!(h.get(http::header::AUTHORIZATION).is_some());
        assert!(h.get(http::header::ORIGIN).is_some());
    }

    #[test]
    fn inbound_strips_polluting_headers() {
        let g = Guard::new();
        let mut h = http::HeaderMap::new();
        h.insert("set-cookie", "a=1".parse().unwrap());
        h.insert("access-control-allow-origin", "*".parse().unwrap());
        h.insert("content-security-policy", "default-src *".parse().unwrap());
        h.insert("x-frame-options", "DENY".parse().unwrap());
        h.insert("strict-transport-security", "max-age=1".parse().unwrap());
        g.sanitize(&mut h, Direction::Inbound);
        assert!(h.get("set-cookie").is_none());
        assert!(h.get("access-control-allow-origin").is_none());
        assert!(h.get("content-security-policy").is_none());
        assert!(h.get("x-frame-options").is_none());
        assert!(h.get("strict-transport-security").is_none());
    }

    #[test]
    fn inbound_keeps_safe_headers() {
        let g = Guard::new();
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        h.insert(http::header::CONTENT_LENGTH, "42".parse().unwrap());
        h.insert(http::header::ETAG, "\"abc\"".parse().unwrap());
        h.insert(http::header::CACHE_CONTROL, "no-store".parse().unwrap());
        g.sanitize(&mut h, Direction::Inbound);
        assert_eq!(
            h.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(h.get(http::header::CONTENT_LENGTH).unwrap(), "42");
        assert!(h.get(http::header::ETAG).is_some());
        assert!(h.get(http::header::CACHE_CONTROL).is_some());
    }
}
