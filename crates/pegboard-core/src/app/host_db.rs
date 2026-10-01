//! host.db 唯一属主：schema 迁移、apps 表、签名 token 存取接口。
//! 数据模型 §6：注册表由扫描产生、库中为派生；signed_tokens 经本接口读写，
//! files 的 Signer 不直接打开该库。

use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum HostDbError {
    #[error("open host.db: {0}")]
    Open(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// 一条签名 token 记录（哈希存储，明文不落库）。
#[derive(Debug, Clone)]
pub struct SignedTokenRow {
    pub app_id: String,
    pub file_id: String,
    pub expires_at: i64,
}

/// 宿主元数据库。
pub struct HostDb {
    conn: Mutex<rusqlite::Connection>,
}

impl HostDb {
    /// 打开或创建 host.db，执行迁移。
    pub fn open(path: &Path) -> Result<Self, HostDbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS apps (
               id           TEXT PRIMARY KEY,
               name         TEXT NOT NULL,
               path         TEXT NOT NULL,
               manifest     BLOB NOT NULL,
               enabled      INTEGER NOT NULL DEFAULT 1,
               installed_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS signed_tokens (
               token_hash TEXT PRIMARY KEY,
               app_id     TEXT NOT NULL,
               file_id    TEXT NOT NULL,
               expires_at INTEGER NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_tokens_expires ON signed_tokens(expires_at);
             CREATE TABLE IF NOT EXISTS admin_auth (
               id           INTEGER PRIMARY KEY CHECK (id = 1),
               password_hash TEXT NOT NULL,
               updated_at   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS admin_sessions (
               token_hash TEXT PRIMARY KEY,
               expires_at INTEGER NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_admin_sessions_expires ON admin_sessions(expires_at);
             CREATE TABLE IF NOT EXISTS schema_migrations (
               version    INTEGER PRIMARY KEY,
               applied_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 写入签名 token（哈希）。
    pub fn insert_token(
        &self,
        token_hash: &str,
        app_id: &str,
        file_id: &str,
        expires_at: i64,
    ) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO signed_tokens(token_hash, app_id, file_id, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![token_hash, app_id, file_id, expires_at, now_ms()],
        )?;
        Ok(())
    }

    /// 按 token 哈希查找（存在即返回，过期判断由调用方做）。
    pub fn lookup_token(&self, token_hash: &str) -> Result<Option<SignedTokenRow>, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn.prepare(
            "SELECT app_id, file_id, expires_at FROM signed_tokens WHERE token_hash = ?1",
        )?;
        let mut rows = stmt.query([token_hash])?;
        match rows.next()? {
            Some(row) => Ok(Some(SignedTokenRow {
                app_id: row.get(0)?,
                file_id: row.get(1)?,
                expires_at: row.get(2)?,
            })),
            None => Ok(None),
        }
    }

    /// 清理过期 token，返回删除数。
    pub fn gc_tokens(&self) -> Result<u64, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let n = conn.execute(
            "DELETE FROM signed_tokens WHERE expires_at <= ?1",
            [now_ms()],
        )?;
        Ok(n as u64)
    }

    /// 删除某应用的全部 token（卸载时）。
    pub fn delete_app_tokens(&self, app_id: &str) -> Result<u64, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let n = conn.execute("DELETE FROM signed_tokens WHERE app_id = ?1", [app_id])?;
        Ok(n as u64)
    }

    /// 注册 / 刷新应用行（安装时；enabled 保持既有值，首次默认 1）。
    pub fn upsert_app(
        &self,
        id: &str,
        name: &str,
        path: &str,
        manifest: &[u8],
    ) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO apps(id, name, path, manifest, enabled, installed_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(id) DO UPDATE SET name = ?2, path = ?3, manifest = ?4",
            rusqlite::params![id, name, path, manifest, now_ms()],
        )?;
        Ok(())
    }

    /// 应用行状态：(enabled, installed_at)。
    pub fn app_states(
        &self,
    ) -> Result<std::collections::HashMap<String, (bool, i64)>, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn.prepare("SELECT id, enabled, installed_at FROM apps")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (id, enabled, installed_at) = row?;
            out.insert(id, (enabled, installed_at));
        }
        Ok(out)
    }

    /// 设置启用状态（幂等）。
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "UPDATE apps SET enabled = ?2 WHERE id = ?1",
            rusqlite::params![id, enabled as i64],
        )?;
        Ok(())
    }

    /// 删除应用行（卸载时）。
    pub fn remove_app(&self, id: &str) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute("DELETE FROM apps WHERE id = ?1", [id])?;
        Ok(())
    }

    // ===== 管理访问密码与会话（明文密码 / 明文 token 均不落库） =====

    /// 当前管理密码哈希（None = 未设置）。
    pub fn get_password_hash(&self) -> Result<Option<String>, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn.prepare("SELECT password_hash FROM admin_auth WHERE id = 1")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// 设置（或修改）管理密码哈希；单行表，幂等 upsert。
    pub fn set_password_hash(&self, password_hash: &str) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO admin_auth(id, password_hash, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET password_hash = ?1, updated_at = ?2",
            rusqlite::params![password_hash, now_ms()],
        )?;
        Ok(())
    }

    /// 登记会话（token 哈希）。
    pub fn insert_session(&self, token_hash: &str, expires_at: i64) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO admin_sessions(token_hash, expires_at, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![token_hash, expires_at, now_ms()],
        )?;
        Ok(())
    }

    /// 按会话 token 哈希查找；存在且未过期才返回 Some(expires_at)。
    pub fn lookup_session(&self, token_hash: &str) -> Result<Option<i64>, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt =
            conn.prepare("SELECT expires_at FROM admin_sessions WHERE token_hash = ?1")?;
        let mut rows = stmt.query([token_hash])?;
        match rows.next()? {
            Some(row) => {
                let expires_at: i64 = row.get(0)?;
                Ok((expires_at > now_ms()).then_some(expires_at))
            }
            None => Ok(None),
        }
    }

    /// 注销会话。
    pub fn delete_session(&self, token_hash: &str) -> Result<(), HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "DELETE FROM admin_sessions WHERE token_hash = ?1",
            [token_hash],
        )?;
        Ok(())
    }

    /// 改密后吊销全部会话。
    pub fn delete_all_sessions(&self) -> Result<u64, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let n = conn.execute("DELETE FROM admin_sessions", [])?;
        Ok(n as u64)
    }

    /// 清理过期会话，返回删除数。
    pub fn gc_sessions(&self) -> Result<u64, HostDbError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let n = conn.execute(
            "DELETE FROM admin_sessions WHERE expires_at <= ?1",
            [now_ms()],
        )?;
        Ok(n as u64)
    }
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
    fn token_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        db.insert_token("hash1", "app", "f1", now_ms() + 60_000)
            .unwrap();
        let row = db.lookup_token("hash1").unwrap().unwrap();
        assert_eq!(row.app_id, "app");
        assert_eq!(row.file_id, "f1");
        assert!(db.lookup_token("missing").unwrap().is_none());
    }

    #[test]
    fn gc_removes_expired() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        db.insert_token("old", "app", "f1", now_ms() - 1).unwrap();
        db.insert_token("new", "app", "f2", now_ms() + 60_000)
            .unwrap();
        assert_eq!(db.gc_tokens().unwrap(), 1);
        assert!(db.lookup_token("old").unwrap().is_none());
        assert!(db.lookup_token("new").unwrap().is_some());
    }

    #[test]
    fn delete_app_tokens() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        db.insert_token("t1", "a", "f1", now_ms() + 60_000).unwrap();
        db.insert_token("t2", "b", "f2", now_ms() + 60_000).unwrap();
        assert_eq!(db.delete_app_tokens("a").unwrap(), 1);
        assert!(db.lookup_token("t1").unwrap().is_none());
        assert!(db.lookup_token("t2").unwrap().is_some());
    }

    #[test]
    fn app_rows_lifecycle() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        db.upsert_app("a", "A", "/apps/a", b"{}").unwrap();
        let states = db.app_states().unwrap();
        assert_eq!(states.get("a").map(|s| s.0), Some(true));
        db.set_enabled("a", false).unwrap();
        assert_eq!(db.app_states().unwrap().get("a").map(|s| s.0), Some(false));
        // 重复 upsert 不重置 enabled
        db.upsert_app("a", "A2", "/apps/a", b"{}").unwrap();
        assert_eq!(db.app_states().unwrap().get("a").map(|s| s.0), Some(false));
        db.remove_app("a").unwrap();
        assert!(!db.app_states().unwrap().contains_key("a"));
    }

    #[test]
    fn password_hash_upsert() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        assert!(db.get_password_hash().unwrap().is_none());
        db.set_password_hash("hash-a").unwrap();
        assert_eq!(db.get_password_hash().unwrap().as_deref(), Some("hash-a"));
        db.set_password_hash("hash-b").unwrap();
        assert_eq!(db.get_password_hash().unwrap().as_deref(), Some("hash-b"));
    }

    #[test]
    fn session_lifecycle() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        assert!(db.lookup_session("s1").unwrap().is_none());
        db.insert_session("s1", now_ms() + 60_000).unwrap();
        assert!(db.lookup_session("s1").unwrap().is_some());
        // 过期会话不可见
        db.insert_session("s2", now_ms() - 1).unwrap();
        assert!(db.lookup_session("s2").unwrap().is_none());
        // 注销单个 / gc 过期
        db.delete_session("s1").unwrap();
        assert!(db.lookup_session("s1").unwrap().is_none());
        assert_eq!(db.gc_sessions().unwrap(), 1);
        assert!(db.lookup_session("s2").unwrap().is_none());
    }

    #[test]
    fn delete_all_sessions_revokes_everything() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = HostDb::open(&dir.path().join("host.db")).unwrap();
        db.insert_session("s1", now_ms() + 60_000).unwrap();
        db.insert_session("s2", now_ms() + 60_000).unwrap();
        assert_eq!(db.delete_all_sessions().unwrap(), 2);
        assert!(db.lookup_session("s1").unwrap().is_none());
        assert!(db.lookup_session("s2").unwrap().is_none());
    }
}
