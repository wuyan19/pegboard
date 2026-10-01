//! 管理访问密码与会话。
//!
//! 规则（与访问分层一致）：密码只保护 /api/admin/*（应用页面与能力 API 不设门）。
//! - 未设密码：管理端点全放行（环回部署默认态；正常路径到不了「环回外 + 无密码」，
//!   配置保存接口会拒绝该组合）。
//! - 已设密码：要求有效会话 cookie（`pegboard_session`，HttpOnly，30 天）。
//!
//! 密码 Argon2id 哈希落 host.db；会话 token 明文只在下发时的 cookie 里，
//! 库中存 sha256。auth 端点本身挂中间件之外（否则鸡生蛋）。

use std::sync::Arc;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{State, State as ExtractState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::ingress::error::{code, ApiError};
use crate::ingress::AppState;

/// 会话 cookie 名。
pub const SESSION_COOKIE: &str = "pegboard_session";
/// 会话有效期：30 天。
const SESSION_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// 最短密码长度。
pub const PASSWORD_MIN: usize = 8;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/auth/state", get(auth_state))
        .route("/api/admin/auth/setup", post(auth_setup))
        .route("/api/admin/auth/login", post(auth_login))
        .route("/api/admin/auth/change", post(auth_change))
        .route("/api/admin/auth/logout", post(auth_logout))
}

// ===== 密码哈希 =====

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::internal(format!("密码哈希失败: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ===== 会话 =====

/// 生成会话 token：32 字节 OsRng → hex（明文），返回 (明文, sha256 哈希)。
fn new_session_token() -> Result<(String, String), ApiError> {
    use argon2::password_hash::rand_core::RngCore;
    let mut buf = [0u8; 32];
    argon2::password_hash::rand_core::OsRng.fill_bytes(&mut buf);
    let plain = hex::encode(buf);
    let hash = sha256_hex(&plain);
    Ok((plain, hash))
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// 下发会话 cookie。HttpOnly 防 JS 读取；SameSite=Lax 允许站内导航携带；
/// 不加 Secure（局域网是明文 HTTP，加了 cookie 反而永远发不出来）。
fn session_cookie(plain: &str) -> String {
    format!(
        "{SESSION_COOKIE}={plain}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_TTL_MS / 1000
    )
}

fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// 从 Cookie 头提取会话 token 明文。
fn session_token_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix(SESSION_COOKIE)?.strip_prefix("=")
            })
        })
        .map(str::to_owned)
}

/// 会话校验（存在 + 未过期）。
fn session_ok(state: &AppState, headers: &HeaderMap) -> bool {
    session_token_from(headers)
        .map(|t| matches!(state.host_db.lookup_session(&sha256_hex(&t)), Ok(Some(_))))
        .unwrap_or(false)
}

// ===== 中间件（挂在除 auth 外的全部 /api/admin/* 上） =====

pub async fn auth_middleware(
    ExtractState(state): ExtractState<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    let has_password = state
        .host_db
        .get_password_hash()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?
        .is_some();
    if !has_password || session_ok(&state, req.headers()) {
        Ok(next.run(req).await)
    } else {
        Err(ApiError::new(
            code::TOKEN_INVALID,
            401,
            "需要登录：访问密码未验证",
        ))
    }
}

// ===== 端点 =====

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

#[derive(Deserialize)]
struct ChangeBody {
    old: String,
    #[serde(rename = "new")]
    new_password: String,
}

fn validate_password(pw: &str) -> Result<(), ApiError> {
    if pw.chars().count() < PASSWORD_MIN {
        return Err(ApiError::invalid_request(format!(
            "密码长度须至少 {PASSWORD_MIN} 字符"
        )));
    }
    Ok(())
}

/// 鉴权状态：前端据此渲染登录 / 首次设密界面。
async fn auth_state(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let has_password = state
        .host_db
        .get_password_hash()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?
        .is_some();
    Ok(Json(json!({
        "has_password": has_password,
        "lan_exposed": state.config().server.is_lan_exposed(),
    }))
    .into_response())
}

/// 首次设置密码。已设密码时拒绝（改密走 /change，需旧密码）。
async fn auth_setup(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordBody>,
) -> Result<Response, ApiError> {
    validate_password(&body.password)?;
    if state
        .host_db
        .get_password_hash()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?
        .is_some()
    {
        return Err(ApiError::new(
            code::INVALID_REQUEST,
            409,
            "密码已设置，修改请走 /change",
        ));
    }
    let hash = hash_password(&body.password)?;
    state
        .host_db
        .set_password_hash(&hash)
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?;
    issue_session(&state).await
}

async fn auth_login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordBody>,
) -> Result<Response, ApiError> {
    let Some(hash) = state
        .host_db
        .get_password_hash()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?
    else {
        return Err(ApiError::new(code::INVALID_REQUEST, 409, "密码尚未设置"));
    };
    if !verify_password(&body.password, &hash) {
        return Err(ApiError::new(code::TOKEN_INVALID, 401, "密码错误"));
    }
    issue_session(&state).await
}

/// 修改密码：须已登录；成功后吊销全部旧会话并发新会话。
async fn auth_change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ChangeBody>,
) -> Result<Response, ApiError> {
    validate_password(&body.new_password)?;
    let Some(hash) = state
        .host_db
        .get_password_hash()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?
    else {
        return Err(ApiError::new(code::INVALID_REQUEST, 409, "密码尚未设置"));
    };
    if !session_ok(&state, &headers) {
        return Err(ApiError::new(
            code::TOKEN_INVALID,
            401,
            "需要登录：访问密码未验证",
        ));
    }
    if !verify_password(&body.old, &hash) {
        return Err(ApiError::new(code::TOKEN_INVALID, 401, "旧密码错误"));
    }
    let new_hash = hash_password(&body.new_password)?;
    state
        .host_db
        .set_password_hash(&new_hash)
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?;
    state
        .host_db
        .delete_all_sessions()
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?;
    issue_session(&state).await
}

async fn auth_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from(&headers) {
        let _ = state.host_db.delete_session(&sha256_hex(&token));
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, clear_cookie())],
        Json(json!({"ok": true})),
    )
        .into_response()
}

/// 建会话 + 下发 cookie；顺带 gc 过期会话（表小，代价可忽略）。
async fn issue_session(state: &Arc<AppState>) -> Result<Response, ApiError> {
    let (plain, hash) = new_session_token()?;
    let expires_at = now_ms() + SESSION_TTL_MS;
    state
        .host_db
        .insert_session(&hash, expires_at)
        .map_err(|e| ApiError::internal(format!("host.db: {e}")))?;
    let _ = state.host_db.gc_sessions();
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, session_cookie(&plain))],
        Json(json!({"ok": true})),
    )
        .into_response())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn session_token_is_random_hex() {
        let (p1, h1) = new_session_token().unwrap();
        let (p2, h2) = new_session_token().unwrap();
        assert_ne!(p1, p2);
        assert_eq!(p1.len(), 64);
        assert_eq!(h1, sha256_hex(&p1));
        assert_ne!(h1, h2);
    }

    #[test]
    fn short_password_rejected() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("long-enough-password").is_ok());
    }
}
