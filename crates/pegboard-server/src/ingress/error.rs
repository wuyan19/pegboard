//! 契约错误码的统一表达。core 错误到错误码/状态码的映射集中在此，
//! 不散落到 handler。错误体格式见 API 契约 §1。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use pegboard_core::app::AppError;
use pegboard_core::guard::GuardError;
use pegboard_core::store::StoreError;

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

/// core GuardError → ApiError（API 契约 §6）。
impl From<GuardError> for ApiError {
    fn from(e: GuardError) -> Self {
        match &e {
            GuardError::PermissionDenied(action) => Self::new(
                code::PERMISSION_DENIED,
                403,
                format!("应用未声明 {action:?} 权限"),
            ),
            GuardError::TargetDenied(_) | GuardError::SsrfBlocked(_) => {
                Self::new(code::TARGET_DENIED, 403, e.to_string())
            }
            GuardError::RateLimited { limit, window_secs } => Self::new(
                code::LIMIT_EXCEEDED,
                429,
                format!("请求频率超限: {limit}/{window_secs}s"),
            )
            .with_detail(json!({ "limit": "net_rps", "max": limit })),
            GuardError::InvalidTarget(_) => Self::invalid_request(e.to_string()),
        }
    }
}

/// core StoreError → ApiError（API 契约 §6：字节类 413）。
impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match &e {
            StoreError::ValueTooLarge { len, max } => Self::new(
                code::LIMIT_EXCEEDED,
                413,
                format!("value 超出单值上限: {len} > {max}"),
            )
            .with_detail(json!({ "limit": "kv_value_bytes", "max": max, "len": len })),
            StoreError::QuotaExceeded { used, add, max } => Self::new(
                code::LIMIT_EXCEEDED,
                413,
                format!("超出 KV 总量配额: used {used} + {add} > {max}"),
            )
            .with_detail(
                json!({ "limit": "kv_total_bytes", "used": used, "add": add, "max": max }),
            ),
            StoreError::KeyTooLong { len, max } => {
                Self::invalid_request(format!("key 过长: {len} > {max}"))
            }
            StoreError::InvalidValue(msg) => Self::invalid_request(msg.clone()),
            StoreError::InvalidApp(msg) => Self::app_not_found(msg),
            // 本地存储后端故障：契约无 INTERNAL，归一到 UPSTREAM_ERROR
            StoreError::Open(_) | StoreError::Io(_) => {
                Self::new(code::UPSTREAM_ERROR, 502, format!("存储后端错误: {e}"))
            }
        }
    }
}

/// core AppError → ApiError（install/uninstall 路径）。
impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        match &e {
            AppError::InvalidId(_) | AppError::ManifestMissing(_) | AppError::DuplicateId(_) => {
                Self::new(code::APP_NOT_FOUND, 404, e.to_string())
            }
            AppError::ManifestParse(_, _) | AppError::ManifestInvalid(_, _) => {
                Self::invalid_request(e.to_string())
            }
            AppError::EntryMissing(_, _) | AppError::EntryEscape(_, _) => {
                Self::invalid_request(e.to_string())
            }
            AppError::LimitsExceeded(_) => Self::invalid_request(e.to_string()),
            AppError::Io(err) => Self::new(code::UPSTREAM_ERROR, 502, format!("io: {err}")),
        }
    }
}
