# pegboard-core / proxy 模块

## 接口

```rust
// crates/pegboard-core/src/proxy/mod.rs

use std::sync::Arc;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use url::Url;

use crate::app::AppMeta;
use crate::guard::{Action, Guard, GuardError};

/// 转发响应：状态 + 已清理头 + 流式体。
pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BodyStream,
}

/// 异步字节流。SSE、chunked、大文件均走这里，绝不整体缓冲。
pub type BodyStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<Bytes, ProxyError>> + Send>>;

/// WebSocket 连接抽象，避免泄露底层实现。
pub struct WsConn { /* 内部持有 tokio-tungstenite 流 */ }

#[derive(Debug, Clone)]
pub enum WsMessage {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

impl WsConn {
    pub async fn send(&mut self, msg: WsMessage) -> Result<(), ProxyError>;
    pub async fn recv(&mut self) -> Option<Result<WsMessage, ProxyError>>;
    pub async fn close(self) -> Result<(), ProxyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("denied: {0}")]
    Denied(#[from] GuardError),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("timeout")]
    Timeout,
    #[error("too many redirects")]
    RedirectLimit,
    #[error("redirect denied: {0}")]
    RedirectDenied(String),
    #[error("response too large: {size} > {max}")]
    TooLarge { size: u64, max: u64 },
    #[error("ws upgrade failed: {0}")]
    WsUpgrade(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// 转发器。持有复用的 reqwest 客户端与 guard 引用。
pub struct Proxy {
    client: reqwest::Client,
    guard: Arc<Guard>,
    cfg: ProxyConfig,
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub timeout: std::time::Duration,
    pub connect_timeout: std::time::Duration,
    pub max_redirects: u8,
    pub max_body_bytes: u64,        // 非流式响应上限；流式不限
    pub user_agent: String,
}

impl Default for ProxyConfig { /* 合理默认 */ }

impl Proxy {
    pub fn new(guard: Arc<Guard>, cfg: ProxyConfig) -> Self;

    /// 转发 HTTP。含重定向逐跳校验、响应头清理、流式透传。
    /// `body` 为 None 时无请求体；为 Some 时流式转发。
    pub async fn forward(
        &self,
        app: &AppMeta,
        method: Method,
        url: Url,
        headers: HeaderMap,
        body: Option<BodyStream>,
    ) -> Result<ProxyResponse, ProxyError>;

    /// 建立到目标的 WebSocket 连接。校验 + 升级。
    pub async fn connect_ws(
        &self,
        app: &AppMeta,
        url: Url,
        extra_headers: HeaderMap,
    ) -> Result<WsConn, ProxyError>;
}
```

## 行为规则

### forward
1. `guard.check_target(app, Action::Net, &url)`；失败即 `Denied`。
2. `guard.check_rate(app, Action::Net)`。
3. 构造 reqwest 请求：复制 method、headers（去除 `Host`、`Connection`、`Content-Length`，由客户端重算）；`body` 用 `Body::wrap_stream`。
4. `redirect::Policy::none()`，手动处理重定向。
5. 发送；超时 → `Timeout`；连接失败 → `Upstream`。
6. 若为 3xx 且带 `Location`：
   - 解析新 URL；相对地址基于当前 URL 解析。
   - `guard.check_target` 重新校验；失败 → `RedirectDenied`。
   - 跳数超 `max_redirects` → `RedirectLimit`。
   - 继续循环。
7. 非流式（`Content-Length` 已知且 ≤ `max_body_bytes`）→ 缓冲读入，超限 → `TooLarge`。
8. 流式（`text/event-stream`、`Transfer-Encoding: chunked`、`Content-Length` 未知或超限）→ 直接透传流，不缓冲、不计数。
9. `guard.sanitize_headers(&mut headers)`。
10. 返回 `ProxyResponse`。

### connect_ws
1. 校验 + 限流，同 forward。
2. 用 `tokio-tungstenite::client_async` 升级；附加 `extra_headers`。
3. 失败 → `WsUpgrade`。
4. 返回 `WsConn`，由 server 侧与浏览器 WS 双向桥接。

### 请求头处理
移除（由客户端重算或不应转发）：
- `Host`
- `Connection`
- `Content-Length`
- `Transfer-Encoding`
- `Upgrade`（HTTP 转发时）

保留并透传：
- `Authorization`、`Cookie`、`Accept*`、`User-Agent`、`Content-Type`、自定义头

### 响应头处理
统一走 `guard.sanitize_headers`；额外移除：
- `Transfer-Encoding`（由 server 侧重新决定）
- `Connection`

保留：
- `Content-Type`、`Content-Length`、`Content-Encoding`
- `ETag`、`Last-Modified`、`Cache-Control`
- `Content-Disposition`、`Accept-Ranges`、`Content-Range`

### 流式判定
- `Content-Type: text/event-stream` → 流式
- `Transfer-Encoding: chunked` → 流式
- `Content-Length` 未知 → 流式
- `Content-Length > max_body_bytes` → 流式（不拒绝，直接透传）
- 其余 → 缓冲，受 `max_body_bytes` 限制

## 客户端构造

```rust
fn build_client(cfg: &ProxyConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(&cfg.user_agent)
        .build()
        .expect("client build")
}
```

## 单元 / 集成测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMeta, Manifest, Permissions};
    use crate::guard::Guard;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn app(net: &[&str]) -> AppMeta {
        let mut p = Permissions::default();
        p.net = net.iter().map(|s| s.to_string()).collect();
        AppMeta {
            id: "t".into(),
            name: "t".into(),
            root: PathBuf::from("."),
            entry: PathBuf::from("index.html"),
            manifest: Manifest { /* 略 */ },
            limits: Default::default(),
        }
    }

    fn proxy() -> Proxy {
        Proxy::new(Arc::new(Guard::new()), ProxyConfig::default())
    }

    // ---------- 目标校验 ----------

    #[tokio::test]
    async fn denied_target_rejected() {
        let p = proxy();
        let a = app(&["https://allowed.com"]);
        let r = p.forward(
            &a,
            Method::GET,
            Url::parse("https://other.com/x").unwrap(),
            HeaderMap::new(),
            None,
        ).await;
        assert!(matches!(r, Err(ProxyError::Denied(_))));
    }

    // ---------- 基本转发（需本地 mock server） ----------

    #[tokio::test]
    async fn forwards_get_and_sanitizes() {
        // 起一个本地 mock（如 axum），返回带 Set-Cookie 的响应
        // 断言：body 一致、Set-Cookie 被移除、Content-Type 保留
    }

    #[tokio::test]
    async fn forwards_post_body() {
        // mock 回显 body，断言一致
    }

    // ---------- 流式 ----------

    #[tokio::test]
    async fn sse_streams_without_buffering() {
        // mock 以 SSE 持续发送，断言：
        // 1. 首个 chunk 在全部完成前到达
        // 2. 总大小可超过 max_body_bytes 而不报 TooLarge
    }

    #[tokio::test]
    async fn buffered_response_respects_limit() {
        // mock 返回大 Content-Length，断言 TooLarge
    }

    // ---------- 重定向 ----------

    #[tokio::test]
    async fn redirect_to_allowed_followed() {
        // mock /a 302 → /b（同 origin），断言最终 body 来自 /b
    }

    #[tokio::test]
    async fn redirect_to_denied_rejected() {
        // mock /a 302 → https://other.com，断言 RedirectDenied
    }

    #[tokio::test]
    async fn redirect_loop_hits_limit() {
        // mock /a → /a，断言 RedirectLimit
    }

    #[tokio::test]
    async fn relative_redirect_resolved() {
        // mock 302 Location: /b，断言基于当前 URL 解析
    }

    // ---------- 请求头 ----------

    #[tokio::test]
    async fn strips_host_and_connection() {
        // 传 Host / Connection 头，mock 侧断言未收到
    }

    #[tokio::test]
    async fn preserves_authorization() {
        // 传 Authorization，mock 侧断言收到
    }

    // ---------- 超时 ----------

    #[tokio::test]
    async fn timeout_yields_error() {
        // mock 延迟超过 timeout，断言 Timeout
    }

    // ---------- WebSocket ----------

    #[tokio::test]
    async fn ws_echo_roundtrip() {
        // 起本地 ws echo server
        // connect_ws → send → recv，断言一致
    }

    #[tokio::test]
    async fn ws_denied_target_rejected() {
        // 未在白名单，断言 Denied
    }

    #[tokio::test]
    async fn ws_binary_frames_preserved() {
        // 发送 Binary，断言原样返回
    }

    #[tokio::test]
    async fn ws_close_handshake() {
        // 发送 Close，断言对端收到并连接终止
    }
}
```

## 依赖

```toml
[dependencies]
tokio = { workspace = true, features = ["net", "time"] }
reqwest = { workspace = true, features = ["stream", "rustls-tls"] }
tokio-tungstenite = { workspace = true, features = ["rustls-tls-webpki-roots"] }
futures-core = { workspace = true }
futures-util = { workspace = true }
bytes = { workspace = true }
http = { workspace = true }
url = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
axum = { workspace = true }       # mock server
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

## 设计要点

- **异步模块**：与 store/files 不同，proxy 本质是网络 IO，暴露 async 接口。core 内部混合同步/异步，调用方（server）按需选择。
- **手动重定向**：`Policy::none` + 自己循环，确保每一跳都走 guard；这是 SSRF 防护的关键。
- **流式与缓冲分治**：SSE / chunked / 未知长度直接透传，不设大小上限；其余缓冲并受 `max_body_bytes` 限制。判定只看响应头，不嗅探内容。
- **客户端复用**：`reqwest::Client` 持有连接池，`Proxy` 单例复用。
- **头处理显式**：请求头去 `Host`/`Connection`/`Content-Length`，响应头走 `sanitize_headers`；避免宿主被污染。
- **WS 抽象薄**：`WsConn` 包住底层流，暴露 `send`/`recv`/`close`，不泄露 tokio-tungstenite 类型；未来可替换。
- **错误映射契约**：`ProxyError` 变体与契约错误码一一对应，由 ingress 转换。
- **不缓存**：不引入任何响应缓存；一致性优先。
