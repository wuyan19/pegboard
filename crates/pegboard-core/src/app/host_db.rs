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
}
