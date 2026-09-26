//! 契约错误码的统一表达。core 错误到错误码/状态码的映射集中在此，
//! 不散落到 handler。错误体格式见 API 契约 §1。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// 契约错误码常量（API 契约 §6）。
pub mod code {
    pub const APP_NOT_FOUND: &str = "APP_NOT_FOUND";
    pub const PERMISSION_DENIED: &str = "PERMISSION_DENIED";
    pub const TARGET_DENIED: &str = "TARGET_DENIED";
    pub const LIMIT_EXCEEDED: &str = "LIMIT_EXCEEDED";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
    pub const TOKEN_INVALID: &str = "TOKEN_INVALID";
    pub const UPSTREAM_ERROR: &str = "UPSTREAM_ERROR";
    pub const TIMEOUT: &str = "TIMEOUT";
}

/// handler 的统一错误类型，由 IntoResponse 序列化为契约错误体。
#[derive(Debug)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
    pub detail: serde_json::Value,
    pub status: u16,
}

impl ApiError {
    pub fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: serde_json::Value::Null,
            status,
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn app_not_found(id: &str) -> Self {
        Self::new(code::APP_NOT_FOUND, 404, format!("应用 `{id}` 不存在"))
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(code::NOT_FOUND, 404, what.to_owned())
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(code::INVALID_REQUEST, 400, message)
    }

    fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "detail": self.detail,
            }
        });
        (self.status_code(), Json(body)).into_response()
    }
}
