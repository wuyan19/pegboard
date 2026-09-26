//! WebSocket 代理 handler：/api/ws-proxy（API 契约 §5）。
//!
//! 浏览器 WebSocket API 无法携带自定义请求头，应用身份经 `app` 查询参数传入
//! （与 X-Pegboard-App 同源同信任级，均由 SDK 注入）。目标校验在 core::proxy。

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use pegboard_core::audit::ActionKind;
use pegboard_core::guard::Action;
use pegboard_core::proxy::{WsConn, WsMessage};

use crate::ingress::context::manual_context;
use crate::ingress::error::ApiError;
use crate::ingress::{trace_audit, AppState};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/ws-proxy", get(ws_handler))
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    url: String,
    /// WS 无法带自定义头：应用身份经查询参数（SDK 注入）
    app: Option<String>,
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let url = url::Url::parse(&query.url)
        .map_err(|e| ApiError::invalid_request(format!("url 参数非法: {e}")))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(ApiError::invalid_request("url 必须是 ws(s) 绝对地址"));
    }
    let target = safe_target(&url);
    // 上下文：app 来自查询参数；subject 从可用请求头解析（浏览器 WS 头稀疏，v1 fixed 模式无碍）
    let mut fake_headers = headers.clone();
    if let Some(app) = &query.app {
        if let Ok(value) = axum::http::HeaderValue::from_str(app) {
            fake_headers.insert(axum::http::HeaderName::from_static("x-pegboard-app"), value);
        }
    }
    let ctx = manual_context(&state, &fake_headers)?;

    trace_audit(&state, &ctx, ActionKind::Ws, Some(target), async {
        state
            .guard
            .check_capability(&ctx.app, Action::Ws)
            .map_err(ApiError::from)?;
        let app = Arc::clone(&ctx.app);
        let upstream = state
            .proxy
            .connect_ws(&app, url)
            .await
            .map_err(ApiError::from)?;
        Ok(ws.on_upgrade(move |browser| bridge(browser, upstream)))
    })
    .await
}

/// 审计 target 脱敏：只记 scheme://host[:port]。
fn safe_target(url: &url::Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(port) = url.port() {
        s.push_str(&format!(":{port}"));
    }
    s
}

/// 双向桥接：浏览器 ↔ 上游；任一侧关闭即终止。
async fn bridge(mut browser: WebSocket, mut upstream: WsConn) {
    /// 关闭中继：转发 Close 后等对端回 Close（有上限），让两端都完成握手
    async fn relay_close_to_browser(browser: &mut WebSocket, upstream: &mut WsConn) {
        let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(msg)) = upstream.recv().await {
                if matches!(msg, WsMessage::Close { .. }) {
                    if let Some(out) = to_browser(msg) {
                        let _ = browser.send(out).await;
                    }
                    break;
                }
            }
        });
        let _ = drained.await;
    }
    async fn relay_close_to_upstream(upstream: &mut WsConn, browser: &mut WebSocket) {
        let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(Ok(msg)) = browser.recv().await {
                if matches!(msg, Message::Close(_)) {
                    if let Some(out) = to_upstream(msg) {
                        let _ = upstream.send(out).await;
                    }
                    break;
                }
            }
        });
        let _ = drained.await;
    }
    loop {
        tokio::select! {
            from_browser = browser.recv() => {
                match from_browser {
                    None | Some(Err(_)) => break,
                    Some(Ok(msg)) => {
                        let is_close = matches!(msg, Message::Close(_));
                        if let Some(out) = to_upstream(msg) {
                            if upstream.send(out).await.is_err() {
                                break;
                            }
                        }
                        if is_close {
                            relay_close_to_browser(&mut browser, &mut upstream).await;
                            break;
                        }
                    }
                }
            }
            from_upstream = upstream.recv() => {
                match from_upstream {
                    None | Some(Err(_)) => break,
                    Some(Ok(msg)) => {
                        let is_close = matches!(msg, WsMessage::Close { .. });
                        if let Some(out) = to_browser(msg) {
                            if browser.send(out).await.is_err() {
                                break;
                            }
                        }
                        if is_close {
                            relay_close_to_upstream(&mut upstream, &mut browser).await;
                            break;
                        }
                    }
                }
            }
        }
    }
    let _ = upstream.close().await;
}

fn to_upstream(msg: Message) -> Option<WsMessage> {
    match msg {
        Message::Text(t) => Some(WsMessage::Text(t.to_string())),
        Message::Binary(b) => Some(WsMessage::Binary(b)),
        Message::Ping(b) => Some(WsMessage::Ping(b)),
        Message::Pong(b) => Some(WsMessage::Pong(b)),
        Message::Close(frame) => Some(WsMessage::Close {
            code: frame.as_ref().map(|f| f.code).unwrap_or(1005),
            reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
        }),
    }
}

fn to_browser(msg: WsMessage) -> Option<Message> {
    match msg {
        WsMessage::Text(t) => Some(Message::Text(t.into())),
        WsMessage::Binary(b) => Some(Message::Binary(b)),
        WsMessage::Ping(b) => Some(Message::Ping(b)),
        WsMessage::Pong(b) => Some(Message::Pong(b)),
        WsMessage::Close { code, reason } => Some(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))),
    }
}
