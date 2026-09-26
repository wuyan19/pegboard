//! 身份解析：把请求映射为不透明 subject（可为空）。
//!
//! 三种来源策略（固定 / 反代头 / token），构造时冻结，运行期只读。
//! 不建用户表、不做角色、不做登录。

use std::collections::HashMap;
use std::net::IpAddr;

use http::HeaderMap;

use crate::config::IdentityConfig;

/// X-Forwarded-User 值长度上限。
const FORWARDED_MAX: usize = 256;

/// 不透明调用者标识。None 表示匿名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject(pub Option<String>);

impl Subject {
    pub fn anonymous() -> Self {
        Subject(None)
    }

    pub fn id(&self) -> Option<&str> {
        self.0.as_deref()
    }

    pub fn is_anonymous(&self) -> bool {
        self.0.is_none()
    }
}

/// 解析输入：请求头 + 来源 IP。
pub struct Request<'a> {
    pub headers: &'a HeaderMap,
    pub peer: Option<IpAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("missing bearer token")]
    MissingToken,
    #[error("unknown token")]
    UnknownToken,
    #[error("forwarded header not trusted: peer {0:?}")]
    UntrustedPeer(Option<IpAddr>),
    #[error("missing forwarded header from trusted peer")]
    MissingForwardedHeader,
    #[error("malformed forwarded header: {0}")]
    MalformedForwarded(String),
}

/// 身份解析器。构造时冻结策略，运行期只读。
pub struct Identity {
    strategy: Strategy,
}

enum Strategy {
    Fixed(Option<String>),
    Forwarded { trusted: Vec<IpAddr> },
    Tokens(HashMap<String, String>),
}

impl Identity {
    pub fn new(cfg: &IdentityConfig) -> Self {
        Self {
            strategy: match cfg {
                IdentityConfig::Fixed { subject } => Strategy::Fixed(subject.clone()),
                IdentityConfig::Forwarded { trusted } => Strategy::Forwarded {
                    trusted: trusted.clone(),
                },
                IdentityConfig::Tokens { tokens } => Strategy::Tokens(tokens.clone()),
            },
        }
    }

    /// 解析 subject。匿名策略返回匿名；其余失败即错误（不静默降级）。
    pub fn resolve(&self, req: &Request<'_>) -> Result<Subject, IdentityError> {
        match &self.strategy {
            Strategy::Fixed(subject) => Ok(Subject(subject.clone())),
            Strategy::Forwarded { trusted } => {
                let Some(peer) = req.peer else {
                    return Err(IdentityError::UntrustedPeer(None));
                };
                if !trusted.contains(&peer) {
                    return Err(IdentityError::UntrustedPeer(Some(peer)));
                }
                match forwarded_user(req.headers) {
                    Some(value) => {
                        let trimmed = value.trim();
                        if trimmed.is_empty() || trimmed.len() > FORWARDED_MAX {
                            return Err(IdentityError::MalformedForwarded(
                                trimmed.len().to_string(),
                            ));
                        }
                        Ok(Subject(Some(trimmed.to_owned())))
                    }
                    None => Err(IdentityError::MissingForwardedHeader),
                }
            }
            Strategy::Tokens(map) => {
                let Some(token) = bearer(req.headers) else {
                    return Err(IdentityError::MissingToken);
                };
                // 常量时间比较，防 token 枚举时序侧信道
                let mut matched: Option<&String> = None;
                for (known, subject) in map {
                    if fixed_time_eq(known.as_bytes(), token.as_bytes()) {
                        matched = Some(subject);
                    }
                }
                matched
                    .map(|s| Subject(Some(s.clone())))
                    .ok_or(IdentityError::UnknownToken)
            }
        }
    }
}

/// 从 Authorization: Bearer <token> 提取 token（scheme 大小写不敏感）。
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(token.trim())
    } else {
        None
    }
}

/// 从 X-Forwarded-User 提取原始值。
fn forwarded_user(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-forwarded-user")?
        .to_str()
        .ok()
        .map(str::trim)
}

/// 定长比较：长度差异也恒定耗时（先比长度，再逐字节累积）。
fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn fixed_some_returns_subject() {
        let id = Identity::new(&IdentityConfig::Fixed {
            subject: Some("alice".into()),
        });
        let h = headers(&[]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn fixed_none_returns_anonymous() {
        let id = Identity::new(&IdentityConfig::Fixed { subject: None });
        let h = headers(&[]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert!(id.resolve(&r).unwrap().is_anonymous());
    }

    #[test]
    fn forwarded_trusted_peer_resolves() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request {
            headers: &h,
            peer: Some("10.0.0.1".parse().unwrap()),
        };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn forwarded_trims_value() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "  bob  ")]);
        let r = Request {
            headers: &h,
            peer: Some("10.0.0.1".parse().unwrap()),
        };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("bob"));
    }

    #[test]
    fn forwarded_untrusted_peer_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request {
            headers: &h,
            peer: Some("10.0.0.2".parse().unwrap()),
        };
        assert!(matches!(
            id.resolve(&r),
            Err(IdentityError::UntrustedPeer(_))
        ));
    }

    #[test]
    fn forwarded_missing_header_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[]);
        let r = Request {
            headers: &h,
            peer: Some("10.0.0.1".parse().unwrap()),
        };
        assert!(matches!(
            id.resolve(&r),
            Err(IdentityError::MissingForwardedHeader)
        ));
    }

    #[test]
    fn forwarded_no_peer_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert!(matches!(
            id.resolve(&r),
            Err(IdentityError::UntrustedPeer(None))
        ));
    }

    #[test]
    fn forwarded_malformed_value_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", &"a".repeat(1024))]);
        let r = Request {
            headers: &h,
            peer: Some("10.0.0.1".parse().unwrap()),
        };
        assert!(matches!(
            id.resolve(&r),
            Err(IdentityError::MalformedForwarded(_))
        ));
    }

    #[test]
    fn tokens_valid_resolves() {
        let mut m = HashMap::new();
        m.insert("t0ken".to_owned(), "alice".to_owned());
        let id = Identity::new(&IdentityConfig::Tokens { tokens: m });
        let h = headers(&[("authorization", "Bearer t0ken")]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn tokens_missing_rejected() {
        let id = Identity::new(&IdentityConfig::Tokens {
            tokens: HashMap::new(),
        });
        let h = headers(&[]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert!(matches!(id.resolve(&r), Err(IdentityError::MissingToken)));
    }

    #[test]
    fn tokens_unknown_rejected() {
        let mut m = HashMap::new();
        m.insert("t0ken".to_owned(), "alice".to_owned());
        let id = Identity::new(&IdentityConfig::Tokens { tokens: m });
        let h = headers(&[("authorization", "Bearer wrong")]);
        let r = Request {
            headers: &h,
            peer: None,
        };
        assert!(matches!(id.resolve(&r), Err(IdentityError::UnknownToken)));
    }

    #[test]
    fn bearer_case_insensitive_scheme() {
        let h = headers(&[("authorization", "bearer t0ken")]);
        assert_eq!(bearer(&h), Some("t0ken"));
        let h2 = headers(&[("authorization", "BEARER t0ken")]);
        assert_eq!(bearer(&h2), Some("t0ken"));
    }

    #[test]
    fn non_bearer_scheme_is_none() {
        let h = headers(&[("authorization", "Basic dXNlcg==")]);
        assert!(bearer(&h).is_none());
    }

    #[test]
    fn subject_helpers() {
        assert!(Subject::anonymous().is_anonymous());
        assert_eq!(Subject(Some("x".into())).id(), Some("x"));
        assert!(Subject(None).id().is_none());
    }
}
