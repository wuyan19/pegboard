//! 网络代理：HTTP 转发（含重定向逐跳校验、流式透传）、资源代理地址。
//! WebSocket（connect_ws）随 M6 接入。
//!
//! 请求头出站清理与响应头入站清理统一走 guard.sanitize（双向）。
//! 客户端复用连接池；无总超时（SSE 长流会被杀），用连接 + 读超时防挂死。

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, Method, StatusCode};
use url::Url;

use crate::app::AppMeta;
use crate::guard::{Action, Direction, Guard, GuardError};

/// 异步字节流。SSE、chunked、大文件均走这里，绝不整体缓冲。
pub type BodyStream = Pin<Box<dyn futures_core::Stream<Item = Result<Bytes, ProxyError>> + Send>>;

/// 转发响应：状态 + 已清理头 + 流式体。
pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BodyStream,
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
    #[error("redirect with body cannot be replayed")]
    RedirectWithBody,
    #[error("ws upgrade failed: {0}")]
    WsUpgrade(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub connect_timeout: Duration,
    /// 单次读停滞上限（不设总超时，保护 SSE 长流）
    pub read_timeout: Duration,
    pub max_redirects: u8,
    /// 非流式响应的缓冲上限；流式（SSE/chunked/未知长度）不限
    pub max_body_bytes: u64,
    pub user_agent: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(300),
            max_redirects: 5,
            max_body_bytes: 4 * 1024 * 1024,
            user_agent: format!("pegboard/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// 转发器。持有复用的 reqwest 客户端与 guard 引用。
pub struct Proxy {
    client: reqwest::Client,
    guard: Arc<Guard>,
    cfg: ProxyConfig,
}

impl Proxy {
    pub fn new(guard: Arc<Guard>, cfg: ProxyConfig) -> Result<Self, ProxyError> {
        let client = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .read_timeout(cfg.read_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(cfg.user_agent.clone())
            .build()
            .map_err(|e| ProxyError::Upstream(format!("client build: {e}")))?;
        Ok(Self { client, guard, cfg })
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.cfg
    }

    /// 转发 HTTP。含重定向逐跳校验、响应头清理、流式透传。
    /// `body` 为 None 时无请求体；为 Some 时流式转发。
    pub async fn forward(
        &self,
        app: &AppMeta,
        method: Method,
        url: Url,
        headers: HeaderMap,
        body: Option<BodyStream>,
    ) -> Result<ProxyResponse, ProxyError> {
        // 1-2. 目标校验 + 限流（proxy 复用 guard 校验目标；重定向每跳重校验）
        self.guard.check_target(app, Action::Net, &url).await?;
        self.guard.check_rate(app, Action::Net)?;

        let mut current_url = url;
        let mut current_method = method;
        let mut current_body = body;
        let mut redirects = 0u8;
        loop {
            // 3. 出站头清理（Cookie/Host/X-Forwarded-*/身份注入头/逐跳头）
            let mut out_headers = headers.clone();
            self.guard.sanitize(&mut out_headers, Direction::Outbound);

            let mut request = self
                .client
                .request(
                    reqwest::Method::from_bytes(current_method.as_str().as_bytes())
                        .map_err(|e| ProxyError::Upstream(format!("method: {e}")))?,
                    current_url.as_str(),
                )
                .headers(to_reqwest_headers(&out_headers)?);
            if let Some(stream) = current_body.take() {
                request = request.body(reqwest::Body::wrap_stream(stream));
            }
            let response = send_categorizing_timeouts(request).await?;

            // 6. 重定向：解析新 URL → 逐跳重校验
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(http::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let Some(location) = location else {
                    return Err(ProxyError::Upstream("重定向缺少 Location".into()));
                };
                redirects += 1;
                if redirects > self.cfg.max_redirects {
                    return Err(ProxyError::RedirectLimit);
                }
                let next = current_url
                    .join(&location)
                    .map_err(|e| ProxyError::InvalidUrl(format!("Location 解析失败: {e}")))?;
                self.guard
                    .check_target(app, Action::Net, &next)
                    .await
                    .map_err(|e| ProxyError::RedirectDenied(e.to_string()))?;
                // 语义对齐浏览器：303 → GET；301/302 历史行为 → GET；307/308 保方法
                match response.status() {
                    StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => {
                        current_method = Method::GET;
                    }
                    _ => {}
                }
                if current_method == Method::GET || current_method == Method::HEAD {
                    // body 已消费或本就没有
                } else if current_body.is_none() {
                    // 无 body 的 307/308 可安全重放
                } else {
                    return Err(ProxyError::RedirectWithBody);
                }
                current_url = next;
                continue;
            }

            // 7-9. 流式判定 + 头清理
            return self.finish_response(response).await;
        }
    }

    async fn finish_response(
        &self,
        response: reqwest::Response,
    ) -> Result<ProxyResponse, ProxyError> {
        let status = StatusCode::from_u16(response.status().as_u16())
            .map_err(|e| ProxyError::Upstream(format!("status: {e}")))?;
        let mut headers = from_reqwest_headers(response.headers());
        let content_type = headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_length = headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let chunked = headers
            .get(http::header::TRANSFER_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("chunked"));

        // 流式：SSE / chunked / 长度未知。已知长度超上限直接拒绝（TooLarge）。
        let streaming = match (&content_type, content_length) {
            (Some(ct), _) if ct.starts_with("text/event-stream") => true,
            (_, Some(len)) if len > self.cfg.max_body_bytes => {
                return Err(ProxyError::TooLarge {
                    size: len,
                    max: self.cfg.max_body_bytes,
                })
            }
            (_, Some(_)) => false,
            (_, None) => chunked,
        };

        self.guard.sanitize(&mut headers, Direction::Inbound);
        // 长度交给下游 body 决定；缓冲路径长度不变
        if !streaming {
            let limit = content_length.unwrap_or(self.cfg.max_body_bytes);
            let buffered = response
                .bytes()
                .await
                .map_err(|e| classify_reqwest(e, "读响应"))?;
            if buffered.len() as u64 > self.cfg.max_body_bytes || limit > self.cfg.max_body_bytes {
                return Err(ProxyError::TooLarge {
                    size: buffered.len() as u64,
                    max: self.cfg.max_body_bytes,
                });
            }
            let chunk = buffered;
            let body: BodyStream = Box::pin(futures_util::stream::once(async move { Ok(chunk) }));
            return Ok(ProxyResponse {
                status,
                headers,
                body,
            });
        }
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| classify_reqwest(e, "流式读")));
        let body: BodyStream = Box::pin(stream);
        Ok(ProxyResponse {
            status,
            headers,
            body,
        })
    }

    /// 资源代理地址：img/font/css 等无法走 fetch 的资源用同源地址（SDK url()）。
    pub fn asset_url(&self, url: &Url) -> String {
        format!("/api/asset?url={}", urlencoding(url.as_str()))
    }

    /// 建立到目标的 WebSocket 连接：校验（白名单 + SSRF + 限流）后升级。
    /// 浏览器 WS 无法携带自定义头，extra_headers 仅服务端内部使用（当前为空）。
    pub async fn connect_ws(&self, app: &AppMeta, url: Url) -> Result<WsConn, ProxyError> {
        self.guard.check_target(app, Action::Ws, &url).await?;
        self.guard.check_rate(app, Action::Ws)?;
        let (stream, _response) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| ProxyError::WsUpgrade(e.to_string()))?;
        Ok(WsConn { inner: stream })
    }
}

/// WebSocket 连接抽象，避免泄露底层实现。
pub struct WsConn {
    inner: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[derive(Debug, Clone)]
pub enum WsMessage {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close { code: u16, reason: String },
}

impl WsConn {
    pub async fn send(&mut self, msg: WsMessage) -> Result<(), ProxyError> {
        let inner = match msg {
            WsMessage::Text(t) => tokio_tungstenite::tungstenite::Message::Text(t.into()),
            WsMessage::Binary(b) => tokio_tungstenite::tungstenite::Message::Binary(b),
            WsMessage::Ping(b) => tokio_tungstenite::tungstenite::Message::Ping(b),
            WsMessage::Pong(b) => tokio_tungstenite::tungstenite::Message::Pong(b),
            WsMessage::Close { code, reason } => tokio_tungstenite::tungstenite::Message::Close(
                Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: code.into(),
                    reason: reason.into(),
                }),
            ),
        };
        use futures_util::SinkExt as _;
        self.inner.send(inner).await.map_err(ws_err)
    }

    pub async fn recv(&mut self) -> Option<Result<WsMessage, ProxyError>> {
        use futures_util::StreamExt as _;
        let msg = self.inner.next().await?;
        Some(msg.map(ws_msg).map_err(ws_err))
    }

    pub async fn close(mut self) -> Result<(), ProxyError> {
        self.inner
            .close(None)
            .await
            .map_err(|e| ProxyError::WsUpgrade(e.to_string()))
    }
}

fn ws_msg(msg: tokio_tungstenite::tungstenite::Message) -> WsMessage {
    use tokio_tungstenite::tungstenite::Message as M;
    match msg {
        M::Text(t) => WsMessage::Text(t.to_string()),
        M::Binary(b) => WsMessage::Binary(b),
        M::Ping(b) => WsMessage::Ping(b),
        M::Pong(b) => WsMessage::Pong(b),
        M::Close(Some(frame)) => WsMessage::Close {
            code: u16::from(frame.code),
            reason: frame.reason.to_string(),
        },
        M::Close(None) => WsMessage::Close {
            code: 1005,
            reason: String::new(),
        },
        M::Frame(_) => WsMessage::Binary(Bytes::new()),
    }
}

fn ws_err(e: tokio_tungstenite::tungstenite::Error) -> ProxyError {
    ProxyError::WsUpgrade(e.to_string())
}

/// query 参数编码（url crate 提供，form-urlencoded 语义）。
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn to_reqwest_headers(headers: &HeaderMap) -> Result<reqwest::header::HeaderMap, ProxyError> {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        let n = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes())
            .map_err(|e| ProxyError::Upstream(format!("header name: {e}")))?;
        let v = reqwest::header::HeaderValue::from_bytes(value.as_bytes())
            .map_err(|e| ProxyError::Upstream(format!("header value: {e}")))?;
        out.insert(n, v);
    }
    Ok(out)
}

fn from_reqwest_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_str().as_bytes()),
            http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out
}

async fn send_categorizing_timeouts(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProxyError> {
    request
        .send()
        .await
        .map_err(|e| classify_reqwest(e, "发送请求"))
}

fn classify_reqwest(e: reqwest::Error, stage: &str) -> ProxyError {
    if e.is_timeout() {
        ProxyError::Timeout
    } else if e.is_connect() {
        ProxyError::Upstream(format!("连接失败: {e}"))
    } else {
        ProxyError::Upstream(format!("{stage}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppMeta, LimitsOverride, Manifest, Permissions};
    use crate::guard::Guard;
    use std::path::PathBuf;

    fn app(net: &[&str]) -> AppMeta {
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
                    store: false,
                    files: false,
                    net: net.iter().map(|s| s.to_string()).collect(),
                    ws: Vec::new(),
                    shim: true,
                },
                limits: LimitsOverride::default(),
            },
            limits: crate::config::Limits {
                net_rps: 1000,
                ..crate::config::Limits::default()
            },
        }
    }

    fn proxy(cfg: ProxyConfig) -> Proxy {
        Proxy::new(Arc::new(Guard::new()), cfg).unwrap()
    }

    /// 极简 raw-HTTP mock：接受一个连接，读请求，返回配置的响应字节。
    /// 返回 (addr, 请求捕获)。capture 在连接处理后填充。
    struct MockServer {
        addr: std::net::SocketAddr,
        captured: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    }

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// 每个连接由 `respond` 闭包给出响应字节（可 await 任意延时）。
    async fn spawn_mock<F, Fut>(respond: F) -> MockServer
    where
        F: Fn(CapturedRequest) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<std::sync::Mutex<Vec<CapturedRequest>>> = Arc::default();
        let cap2 = Arc::clone(&captured);
        let respond = Arc::new(respond);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let cap = Arc::clone(&cap2);
                let respond = Arc::clone(&respond);
                tokio::spawn(async move {
                    let mut socket = socket;
                    let req = match read_request(&mut socket).await {
                        Some(r) => r,
                        None => return,
                    };
                    cap.lock().unwrap().push(req.clone());
                    let bytes = respond(req).await;
                    let _ = socket.write_all(&bytes).await;
                });
            }
        });
        MockServer { addr, captured }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<CapturedRequest> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // 读到 header 结束
        let header_end = loop {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_header_end(&buf) {
                break pos;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next()?.to_owned();
        let mut parts = request_line.split(' ');
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut headers = Vec::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
            }
        }
        let chunked = headers
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
        let mut raw = buf[header_end + 4..].to_vec();
        if chunked {
            // 最小 chunked 解码：读到终止块 0
            loop {
                match find_chunk_terminator(&raw) {
                    Some(true) => break,
                    Some(false) | None => {
                        let n = socket.read(&mut chunk).await.ok()?;
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            let body = decode_chunked(&raw).unwrap_or_default();
            return Some(CapturedRequest {
                method,
                path,
                headers,
                body,
            });
        }
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        while raw.len() < content_length {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        raw.truncate(content_length);
        Some(CapturedRequest {
            method,
            path,
            headers,
            body: raw,
        })
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// 完整 chunked 体是否收到终止块（0 大小块）。
    fn find_chunk_terminator(raw: &[u8]) -> Option<bool> {
        let mut pos = 0usize;
        loop {
            let line_end = raw[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
            let size_str = String::from_utf8_lossy(&raw[pos..line_end]);
            let size = usize::from_str_radix(size_str.trim().split(';').next()?.trim(), 16).ok()?;
            pos = line_end + 2;
            if size == 0 {
                return Some(true);
            }
            pos += size + 2; // 数据 + CRLF
            if pos > raw.len() {
                return Some(false);
            }
        }
    }

    fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            let line_end = raw[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
            let size_str = String::from_utf8_lossy(&raw[pos..line_end]);
            let size = usize::from_str_radix(size_str.trim().split(';').next()?.trim(), 16).ok()?;
            pos = line_end + 2;
            if size == 0 {
                return Some(out);
            }
            if pos + size > raw.len() {
                return None;
            }
            out.extend_from_slice(&raw[pos..pos + size]);
            pos += size + 2;
        }
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
        out.push_str(body);
        out.into_bytes()
    }

    fn mock_url(addr: std::net::SocketAddr, path: &str) -> Url {
        Url::parse(&format!("http://{addr}{path}")).unwrap()
    }

    fn whitelist_entry(addr: std::net::SocketAddr) -> String {
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn denied_target_rejected() {
        let p = proxy(ProxyConfig::default());
        let a = app(&["http://127.0.0.1:1"]);
        let r = p
            .forward(
                &a,
                Method::GET,
                Url::parse("http://127.0.0.2:2/x").unwrap(),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(matches!(r, Err(ProxyError::Denied(_))));
    }

    #[tokio::test]
    async fn forwards_get_and_sanitizes() {
        let server = spawn_mock(|_req| async move {
            http_response(
                "200 OK",
                &[
                    ("content-type", "application/json"),
                    ("set-cookie", "session=leak"),
                    ("access-control-allow-origin", "*"),
                ],
                r#"{"ok":true}"#,
            )
        })
        .await;
        let p = proxy(ProxyConfig::default());
        let a = app(&[&whitelist_entry(server.addr)]);
        let mut headers = HeaderMap::new();
        headers.insert("x-pegboard-app", "t".parse().unwrap());
        headers.insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
        headers.insert(http::header::AUTHORIZATION, "Bearer k".parse().unwrap());
        let resp = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/api"),
                headers,
                None,
            )
            .await
            .unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        // 入站清理：Set-Cookie / ACAO 移除，Content-Type 保留
        assert!(resp.headers.get("set-cookie").is_none());
        assert!(resp.headers.get("access-control-allow-origin").is_none());
        assert_eq!(
            resp.headers.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        // 出站清理：mock 侧不应收到注入头，应收到 Authorization（快照后释放锁）
        let saw_auth = {
            let captured = server.captured.lock().unwrap();
            let last = captured.last().unwrap();
            let no_injected = last
                .headers
                .iter()
                .all(|(k, _)| k != "x-pegboard-app" && k != "x-forwarded-for");
            let has_auth = last
                .headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer k");
            no_injected && has_auth
        };
        assert!(saw_auth, "出站头清理断言失败");
        // body 透传
        let collected = collect(resp.body).await.unwrap();
        assert_eq!(collected, br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn forwards_post_body() {
        let server = spawn_mock(|req| async move {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            http_response("200 OK", &[("content-type", "text/plain")], &body)
        })
        .await;
        let p = proxy(ProxyConfig::default());
        let a = app(&[&whitelist_entry(server.addr)]);
        let body: BodyStream = Box::pin(futures_util::stream::once(async {
            Ok(Bytes::from_static(b"hello-proxy-body"))
        }));
        let resp = p
            .forward(
                &a,
                Method::POST,
                mock_url(server.addr, "/echo"),
                HeaderMap::new(),
                Some(body),
            )
            .await
            .unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        let collected = collect(resp.body).await.unwrap();
        assert_eq!(collected, b"hello-proxy-body");
        let captured = server.captured.lock().unwrap();
        let last = captured.last().unwrap();
        assert_eq!(last.method, "POST");
        assert_eq!(last.body, b"hello-proxy-body");
    }

    #[tokio::test]
    async fn sse_streams_without_buffering() {
        // SSE：总大小超过 max_body_bytes 仍透传（流式不限）
        let server = spawn_mock(|_req| async move {
            let mut out = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n".to_owned();
            for i in 0..50 {
                out.push_str(&format!("data: chunk-{i}\n\n"));
            }
            out.into_bytes()
        })
        .await;
        let cfg = ProxyConfig {
            max_body_bytes: 128, // 远小于总大小
            ..ProxyConfig::default()
        };
        let p = proxy(cfg);
        let a = app(&[&whitelist_entry(server.addr)]);
        let resp = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/sse"),
                HeaderMap::new(),
                None,
            )
            .await
            .unwrap();
        let collected = collect(resp.body).await.unwrap();
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("data: chunk-0"), "{text}");
        assert!(text.contains("data: chunk-49"), "{text}");
    }

    #[tokio::test]
    async fn buffered_response_respects_limit() {
        let big = "x".repeat(1024);
        let server = spawn_mock(move |_req| {
            let big = big.clone();
            async move { http_response("200 OK", &[("content-type", "text/plain")], &big) }
        })
        .await;
        let cfg = ProxyConfig {
            max_body_bytes: 256,
            ..ProxyConfig::default()
        };
        let p = proxy(cfg);
        let a = app(&[&whitelist_entry(server.addr)]);
        let r = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/big"),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(matches!(r, Err(ProxyError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn redirect_to_allowed_followed() {
        let server = spawn_mock(|req| async move {
            if req.path.starts_with("/a") {
                "HTTP/1.1 302 Found\r\nlocation: /b\r\ncontent-length: 0\r\n\r\n".into()
            } else {
                http_response("200 OK", &[("content-type", "text/plain")], "from-b")
            }
        })
        .await;
        let p = proxy(ProxyConfig::default());
        let a = app(&[&whitelist_entry(server.addr)]);
        let resp = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/a"),
                HeaderMap::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        let collected = collect(resp.body).await.unwrap();
        assert_eq!(collected, b"from-b");
    }

    #[tokio::test]
    async fn redirect_to_denied_rejected() {
        let server = spawn_mock(|_req| async move {
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.2:2/x\r\ncontent-length: 0\r\n\r\n"
                .into()
        })
        .await;
        let p = proxy(ProxyConfig::default());
        let a = app(&[&whitelist_entry(server.addr)]);
        let r = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/a"),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(matches!(r, Err(ProxyError::RedirectDenied(_))));
    }

    #[tokio::test]
    async fn redirect_loop_hits_limit() {
        let server = spawn_mock(|_req| async move {
            "HTTP/1.1 302 Found\r\nlocation: /loop\r\ncontent-length: 0\r\n\r\n".into()
        })
        .await;
        let p = proxy(ProxyConfig::default());
        let a = app(&[&whitelist_entry(server.addr)]);
        let r = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/loop"),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(matches!(r, Err(ProxyError::RedirectLimit)));
    }

    #[tokio::test]
    async fn stall_yields_timeout() {
        // mock 接受连接但延迟响应超过 read_timeout
        let server = spawn_mock(|_req| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            http_response("200 OK", &[], "late")
        })
        .await;
        let cfg = ProxyConfig {
            read_timeout: Duration::from_millis(200),
            ..ProxyConfig::default()
        };
        let p = proxy(cfg);
        let a = app(&[&whitelist_entry(server.addr)]);
        let r = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/slow"),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(r.is_err(), "expected timeout");
    }

    #[tokio::test]
    async fn rate_limited_request_denied() {
        let server = spawn_mock(|_req| async move { http_response("200 OK", &[], "x") }).await;
        let cfg = ProxyConfig {
            read_timeout: Duration::from_secs(5),
            ..ProxyConfig::default()
        };
        let p = Proxy::new(Arc::new(Guard::new()), cfg).unwrap();
        let mut a = app(&[&whitelist_entry(server.addr)]);
        a.limits.net_rps = 2;
        for _ in 0..2 {
            let r = p
                .forward(
                    &a,
                    Method::GET,
                    mock_url(server.addr, "/"),
                    HeaderMap::new(),
                    None,
                )
                .await;
            assert!(r.is_ok(), "forward should succeed");
        }
        let r = p
            .forward(
                &a,
                Method::GET,
                mock_url(server.addr, "/"),
                HeaderMap::new(),
                None,
            )
            .await;
        assert!(matches!(
            r,
            Err(ProxyError::Denied(GuardError::RateLimited { .. }))
        ));
    }

    #[test]
    fn asset_url_encodes() {
        let p = proxy(ProxyConfig::default());
        let url = Url::parse("https://example.com/img cat.png?a=1&b=2").unwrap();
        let asset = p.asset_url(&url);
        assert!(asset.starts_with("/api/asset?url="), "{asset}");
        assert!(!asset.contains(' '), "{asset}");
    }

    async fn collect(mut body: BodyStream) -> Result<Vec<u8>, ProxyError> {
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    // ---------- WebSocket ----------

    use tokio_tungstenite::tungstenite::Message as TgMessage;

    async fn spawn_ws_echo() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use futures_util::{SinkExt as _, StreamExt as _};
                    let mut ws = match tokio_tungstenite::accept_async(socket).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            TgMessage::Text(_) | TgMessage::Binary(_) | TgMessage::Ping(_) => {
                                if ws.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            TgMessage::Close(frame) => {
                                let _ = ws.send(TgMessage::Close(frame)).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        addr
    }

    fn ws_app(entry: &str) -> AppMeta {
        let mut a = app(&[]);
        a.manifest.permissions.ws = vec![entry.to_owned()];
        a
    }

    #[tokio::test]
    async fn ws_echo_roundtrip() {
        let addr = spawn_ws_echo().await;
        let p = proxy(ProxyConfig::default());
        let a = ws_app(&format!("ws://{addr}"));
        let mut conn = p
            .connect_ws(&a, Url::parse(&format!("ws://{addr}/")).unwrap())
            .await
            .unwrap();
        conn.send(WsMessage::Text("hello-ws".into())).await.unwrap();
        match conn.recv().await {
            Some(Ok(WsMessage::Text(t))) => assert_eq!(t, "hello-ws"),
            other => panic!("expected text echo, got {other:?}"),
        }
        conn.close().await.unwrap();
    }

    #[tokio::test]
    async fn ws_binary_frames_preserved() {
        let addr = spawn_ws_echo().await;
        let p = proxy(ProxyConfig::default());
        let a = ws_app(&format!("ws://{addr}"));
        let mut conn = p
            .connect_ws(&a, Url::parse(&format!("ws://{addr}/")).unwrap())
            .await
            .unwrap();
        let payload = Bytes::from_static(b"\x00\x01\x02binary");
        conn.send(WsMessage::Binary(payload.clone())).await.unwrap();
        match conn.recv().await {
            Some(Ok(WsMessage::Binary(b))) => assert_eq!(b, payload),
            other => panic!("expected binary echo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ws_denied_target_rejected() {
        let p = proxy(ProxyConfig::default());
        let a = ws_app("ws://127.0.0.1:1");
        let r = p
            .connect_ws(&a, Url::parse("ws://127.0.0.2:2/").unwrap())
            .await;
        assert!(matches!(r, Err(ProxyError::Denied(_))));
    }

    #[tokio::test]
    async fn ws_close_handshake() {
        let addr = spawn_ws_echo().await;
        let p = proxy(ProxyConfig::default());
        let a = ws_app(&format!("ws://{addr}"));
        let mut conn = p
            .connect_ws(&a, Url::parse(&format!("ws://{addr}/")).unwrap())
            .await
            .unwrap();
        // 先通信再关闭：验证关闭握手完成且不报错（echo 侧回 Close 帧）
        conn.send(WsMessage::Text("before-close".into()))
            .await
            .unwrap();
        let _ = conn.recv().await;
        conn.close().await.unwrap();
    }
}
