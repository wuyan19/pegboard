//! 身份解析：把请求映射为不透明 subject。
//!
//! v2 简化：不再有可配置的身份来源（fixed/forwarded/tokens 已删）。
//! 访问控制分层为「网络边界（listen 绑址）+ 管理密码（host.db）」，
//! 能力 API 的 subject 固定为内部值——它只用于审计归因，不做准入。
//! 不建用户表、不做角色、不做登录。

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

/// 能力 API 的固定 subject（所有调用方同一身份；鉴权不在此层）。
pub const LOCAL_SUBJECT: &str = "local";

/// 身份解析器。
#[derive(Debug, Default, Clone, Copy)]
pub struct Identity;

impl Identity {
    pub fn new() -> Self {
        Self
    }

    /// 解析 subject。恒定返回固定内部值，不会失败。
    pub fn resolve(&self) -> Subject {
        Subject(Some(LOCAL_SUBJECT.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_fixed_local() {
        let id = Identity::new();
        assert_eq!(id.resolve().id(), Some(LOCAL_SUBJECT));
        assert!(!id.resolve().is_anonymous());
    }

    #[test]
    fn subject_helpers() {
        assert!(Subject::anonymous().is_anonymous());
        assert_eq!(Subject(Some("x".into())).id(), Some("x"));
        assert!(Subject(None).id().is_none());
    }
}
