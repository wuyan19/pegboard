# pegboard-core / identity 模块

## 接口

```rust
// crates/pegboard-core/src/identity/mod.rs

use std::collections::HashMap;
use std::net::IpAddr;
use http::HeaderMap;

use crate::config::IdentityConfig;

/// 不透明调用者标识。None 表示匿名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject(pub Option<String>);

impl Subject {
    pub fn anonymous() -> Self { Subject(None) }
    pub fn id(&self) -> Option<&str> { self.0.as_deref() }
    pub fn is_anonymous(&self) -> bool { self.0.is_none() }
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
    pub fn new(cfg: &IdentityConfig) -> Self;

    /// 解析 subject。匿名策略返回匿名；其余失败即错误。
    pub fn resolve(&self, req: &Request) -> Result<Subject, IdentityError>;
}

/// 从 Authorization: Bearer <token> 提取 token。
fn bearer(headers: &HeaderMap) -> Option<&str>;

/// 从 X-Forwarded-User 提取 subject。
fn forwarded_user(headers: &HeaderMap) -> Option<&str>;
```

## 策略行为

| 策略 | 输入 | 输出 |
|---|---|---|
| Fixed(Some(s)) | 任意 | Subject(Some(s)) |
| Fixed(None) | 任意 | Subject(None) |
| Forwarded { trusted } | peer ∈ trusted 且有 `X-Forwarded-User` | Subject(Some(值)) |
| Forwarded { trusted } | peer ∉ trusted | `UntrustedPeer` |
| Forwarded { trusted } | peer ∈ trusted 但无 header | `MissingForwardedHeader` |
| Tokens(map) | `Bearer <t>` 且 t ∈ map | Subject(Some(map[t])) |
| Tokens(map) | 无 header | `MissingToken` |
| Tokens(map) | t ∉ map | `UnknownToken` |

规则：
- `Forwarded` 只信任来自 `trusted` 列表的 peer；`peer` 为 `None` 时一律拒绝。
- `X-Forwarded-User` 值做 trim 与长度上限校验，避免注入。
- `Tokens` 的 bearer 比较固定时间，防时序侧信道。
- 值一律不解析语义；调用方拿到的只是不透明字符串。

## 安全约束

- `Forwarded` 的 `trusted` 由 `Config` 校验非空；空列表拒绝启动。
- `X-Forwarded-User` 仅在 peer 命中时读取；否则忽略，避免伪造。
- 值长度上限（如 256 字节），超限 `MalformedForwarded`。
- 不做任何 header 链解析（如 `X-Forwarded-For`），只认显式配置。
- `Tokens` 常量时间比较，避免 token 枚举。

## 单元测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn req_with(headers: HeaderMap, peer: Option<IpAddr>) -> Request<'static> {
        // 注意：Request 借用了 headers，测试里手工管理生命周期
        unimplemented!()
    }

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
        let id = Identity::new(&IdentityConfig::Fixed(Some("alice".into())));
        let h = headers(&[]);
        let r = Request { headers: &h, peer: None };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn fixed_none_returns_anonymous() {
        let id = Identity::new(&IdentityConfig::Fixed(None));
        let h = headers(&[]);
        let r = Request { headers: &h, peer: None };
        assert!(id.resolve(&r).unwrap().is_anonymous());
    }

    #[test]
    fn forwarded_trusted_peer_resolves() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request { headers: &h, peer: Some("10.0.0.1".parse().unwrap()) };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn forwarded_untrusted_peer_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request { headers: &h, peer: Some("10.0.0.2".parse().unwrap()) };
        assert!(matches!(id.resolve(&r), Err(IdentityError::UntrustedPeer(_))));
    }

    #[test]
    fn forwarded_missing_header_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[]);
        let r = Request { headers: &h, peer: Some("10.0.0.1".parse().unwrap()) };
        assert!(matches!(id.resolve(&r), Err(IdentityError::MissingForwardedHeader)));
    }

    #[test]
    fn forwarded_no_peer_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", "alice")]);
        let r = Request { headers: &h, peer: None };
        assert!(matches!(id.resolve(&r), Err(IdentityError::UntrustedPeer(None))));
    }

    #[test]
    fn forwarded_malformed_value_rejected() {
        let id = Identity::new(&IdentityConfig::Forwarded {
            trusted: vec!["10.0.0.1".parse().unwrap()],
        });
        let h = headers(&[("x-forwarded-user", &"a".repeat(1024))]);
        let r = Request { headers: &h, peer: Some("10.0.0.1".parse().unwrap()) };
        assert!(matches!(id.resolve(&r), Err(IdentityError::MalformedForwarded(_))));
    }

    #[test]
    fn tokens_valid_resolves() {
        let mut m = HashMap::new();
        m.insert("t0ken".into(), "alice".into());
        let id = Identity::new(&IdentityConfig::Tokens(m));
        let h = headers(&[("authorization", "Bearer t0ken")]);
        let r = Request { headers: &h, peer: None };
        assert_eq!(id.resolve(&r).unwrap().id(), Some("alice"));
    }

    #[test]
    fn tokens_missing_rejected() {
        let id = Identity::new(&IdentityConfig::Tokens(HashMap::new()));
        let h = headers(&[]);
        let r = Request { headers: &h, peer: None };
        assert!(matches!(id.resolve(&r), Err(IdentityError::MissingToken)));
    }

    #[test]
    fn tokens_unknown_rejected() {
        let mut m = HashMap::new();
        m.insert("t0ken".into(), "alice".into());
        let id = Identity::new(&IdentityConfig::Tokens(m));
        let h = headers(&[("authorization", "Bearer wrong")]);
        let r = Request { headers: &h, peer: None };
        assert!(matches!(id.resolve(&r), Err(IdentityError::UnknownToken)));
    }

    #[test]
    fn bearer_case_insensitive_scheme() {
        let h = headers(&[("authorization", "bearer t0ken")]);
        assert_eq!(bearer(&h), Some("t0ken"));
    }

    #[test]
    fn subject_helpers() {
        assert!(Subject::anonymous().is_anonymous());
        assert_eq!(Subject(Some("x".into())).id(), Some("x"));
        assert!(Subject(None).id().is_none());
    }
}
```

> 注：测试里 `Request<'static>` 与借用头会冲突，实现时用 `struct Request<'a> { headers: &'a HeaderMap, peer: Option<IpAddr> }`，测试局部构造即可，不需要 `'static`。

## 依赖

```toml
[dependencies]
http = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
```

与 `guard` 共用 `http`，避免重复。

## 设计要点

- **不透明**：`Subject` 只是字符串包装，不解析、不区分类型、不带角色。
- **策略冻结**：`Identity::new` 后只读，运行期无锁。
- **显式信任**：`Forwarded` 必须配置 `trusted`；空列表由 `config` 拒绝启动。
- **常量时间比较**：`Tokens` 校验用固定时间，防侧信道。
- **拒绝优先**：所有异常路径都返回错误，不静默降级为匿名；调用方决定是否放行。
- **不引入用户模型**：没有用户表、没有角色、没有会话；语义留给应用。
- **可在 ingress 之外测试**：输入是 headers + peer，纯函数式，无 IO。
