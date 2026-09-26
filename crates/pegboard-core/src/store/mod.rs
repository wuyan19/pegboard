//! KV 原语：按应用物理隔离（独立 app.db），配额在写入前执行。
//!
//! 同步 API（rusqlite）；server 侧用 spawn_blocking 包装。
//! 数据路径：`<data_root>/apps_data/<app_id>/app.db`（与产物目录 apps/ 分离）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::app::validate_id;
use crate::config::Limits;

/// key 长度上限。
const KEY_MAX: usize = 512;

/// 单条 KV 记录。
#[derive(Debug, Clone)]
pub struct KvEntry {
    pub key: String,
    /// JSON 字节，store 不解释
    pub value: Vec<u8>,
}

/// 批量操作项。
#[derive(Debug, Clone)]
pub enum KvOp {
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
}

/// 应用用量快照，用于配额判断与展示。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreUsage {
    pub bytes: u64,
    pub keys: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open db: {0}")]
    Open(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid app id: {0}")]
    InvalidApp(String),
    #[error("key too long: {len} > {max}")]
    KeyTooLong { len: usize, max: usize },
    #[error("value too large: {len} > {max}")]
    ValueTooLarge { len: u64, max: u64 },
    #[error("quota exceeded: used {used} + {add} > {max}")]
    QuotaExceeded { used: u64, add: u64, max: u64 },
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// 单应用的 KV 存储。
pub struct AppStore {
    conn: Mutex<rusqlite::Connection>,
    limits: Limits,
    #[allow(dead_code)]
    app_id: String,
}

impl AppStore {
    /// 打开或创建 app.db，执行初始化（WAL + 建表）。
    pub fn open(db_path: &Path, app_id: &str, limits: Limits) -> Result<Self, StoreError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = open_conn(db_path)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            limits,
            app_id: app_id.to_owned(),
        })
    }

    /// 生效限额（清单收紧后由 StoreManager 比对触发重开）。
    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        get_row(&conn, key)
    }

    /// 写入；先校验单值与总量配额，再落库。
    pub fn set(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        set_row(&conn, key, value, &self.limits)
    }

    pub fn delete(&self, key: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.prepare("DELETE FROM kv WHERE key = ?1")?
            .execute([key])?;
        Ok(())
    }

    /// 前缀查询，按 key 字典序。prefix 为空返回全部。
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<KvEntry>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        list_rows(&conn, prefix)
    }

    /// 原子批量：事务内执行，任一失败全部回滚。
    pub fn batch(&self, ops: &[KvOp]) -> Result<(), StoreError> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        // 预检：合并中间态的配额判断
        let usage = usage_of(&tx)?;
        let mut projected = usage.bytes as i64;
        let mut sizes: HashMap<&str, i64> = HashMap::new();
        for op in ops {
            match op {
                KvOp::Set { key, value } => {
                    check_key(key)?;
                    if value.len() as u64 > self.limits.kv_value_bytes {
                        return Err(StoreError::ValueTooLarge {
                            len: value.len() as u64,
                            max: self.limits.kv_value_bytes,
                        });
                    }
                    let old = match sizes.get(key.as_str()) {
                        Some(cached) => Some(*cached),
                        None => size_of(&tx, key)?.map(i64::from),
                    };
                    let old = old.unwrap_or(0);
                    projected += value.len() as i64 - old;
                    if projected < 0 {
                        projected = 0;
                    }
                    if projected as u64 > self.limits.kv_total_bytes {
                        return Err(StoreError::QuotaExceeded {
                            used: usage.bytes,
                            add: projected.unsigned_abs(),
                            max: self.limits.kv_total_bytes,
                        });
                    }
                    sizes.insert(key, value.len() as i64);
                }
                KvOp::Delete { key } => {
                    check_key(key)?;
                    let old = match sizes.get(key.as_str()) {
                        Some(cached) => Some(*cached),
                        None => size_of(&tx, key)?.map(i64::from),
                    };
                    if let Some(old_len) = old {
                        projected -= old_len;
                        if projected < 0 {
                            projected = 0;
                        }
                    }
                    sizes.insert(key, 0);
                }
            }
        }
        for op in ops {
            match op {
                KvOp::Set { key, value } => {
                    let now = now_ms();
                    tx.execute(
                        "INSERT INTO kv(key, value, size, updated_at) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(key) DO UPDATE SET value = ?2, size = ?3, updated_at = ?4",
                        rusqlite::params![key, value, value.len() as i64, now],
                    )?;
                }
                KvOp::Delete { key } => {
                    tx.execute("DELETE FROM kv WHERE key = ?1", [key])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 当前用量。
    pub fn usage(&self) -> Result<StoreUsage, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        usage_of(&conn)
    }

    /// 清空该应用所有 KV（卸载时用）。不删库文件。
    pub fn clear(&self) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute("DELETE FROM kv", [])?;
        Ok(())
    }
}

fn check_key(key: &str) -> Result<(), StoreError> {
    if key.len() > KEY_MAX {
        return Err(StoreError::KeyTooLong {
            len: key.len(),
            max: KEY_MAX,
        });
    }
    Ok(())
}

fn get_row(conn: &rusqlite::Connection, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

fn size_of(conn: &rusqlite::Connection, key: &str) -> Result<Option<u32>, StoreError> {
    let mut stmt = conn.prepare("SELECT size FROM kv WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

fn set_row(
    conn: &rusqlite::Connection,
    key: &str,
    value: &[u8],
    limits: &Limits,
) -> Result<(), StoreError> {
    check_key(key)?;
    if value.len() as u64 > limits.kv_value_bytes {
        return Err(StoreError::ValueTooLarge {
            len: value.len() as u64,
            max: limits.kv_value_bytes,
        });
    }
    let usage = usage_of(conn)?;
    let old = size_of(conn, key)?.unwrap_or(0);
    let add = value.len() as i64 - i64::from(old);
    if add > 0 && usage.bytes + add.unsigned_abs() > limits.kv_total_bytes {
        return Err(StoreError::QuotaExceeded {
            used: usage.bytes,
            add: add.unsigned_abs(),
            max: limits.kv_total_bytes,
        });
    }
    let now = now_ms();
    conn.execute(
        "INSERT INTO kv(key, value, size, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET value = ?2, size = ?3, updated_at = ?4",
        rusqlite::params![key, value, value.len() as i64, now],
    )?;
    Ok(())
}

fn list_rows(
    conn: &rusqlite::Connection,
    prefix: Option<&str>,
) -> Result<Vec<KvEntry>, StoreError> {
    let prefix = prefix.unwrap_or("");
    let escaped = escape_like(prefix);
    let mut stmt =
        conn.prepare("SELECT key, value FROM kv WHERE key LIKE ?1 ESCAPE '\\' ORDER BY key")?;
    let rows = stmt.query_map([&escaped], |row| {
        Ok(KvEntry {
            key: row.get(0)?,
            value: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn usage_of(conn: &rusqlite::Connection) -> Result<StoreUsage, StoreError> {
    conn.query_row(
        "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM kv",
        [],
        |row| {
            Ok(StoreUsage {
                bytes: row.get::<_, i64>(0)?.max(0) as u64,
                keys: row.get::<_, i64>(1)?.max(0) as u64,
            })
        },
    )
    .map_err(StoreError::from)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// LIKE 转义：`\` `%` `_`。
fn escape_like(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// 打开连接：WAL、busy_timeout。
fn open_conn(path: &Path) -> Result<rusqlite::Connection, StoreError> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

/// 建表或迁移。
fn migrate(conn: &rusqlite::Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kv (
           key        TEXT PRIMARY KEY,
           value      BLOB NOT NULL,
           size       INTEGER NOT NULL,
           updated_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_kv_updated ON kv(updated_at);
         CREATE TABLE IF NOT EXISTS schema_migrations (
           version    INTEGER PRIMARY KEY,
           applied_at INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

/// 按 app_id 管理多个 AppStore，惰性打开，缓存连接。
/// 数据根：<data_root>/apps_data/。
pub struct StoreManager {
    root: PathBuf,
    stores: Mutex<HashMap<String, Arc<StoreSlot>>>,
}

struct StoreSlot {
    store: Mutex<AppStore>,
}

impl StoreManager {
    pub fn new(root: PathBuf, default_limits: Limits) -> Self {
        let _ = default_limits; // 限额由 AppMeta 逐次传入，此处不再需要默认值
        Self {
            root,
            stores: Mutex::new(HashMap::new()),
        }
    }

    /// 获取或打开某应用的 store 并执行 f。limits 来自 AppMeta；
    /// 清单收紧后与缓存不一致时重开。
    pub fn with<F, R>(&self, app_id: &str, limits: &Limits, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&AppStore) -> Result<R, StoreError>,
    {
        validate_id(app_id).map_err(|e| StoreError::InvalidApp(e.to_string()))?;
        let slot = {
            let mut map = self.stores.lock().unwrap_or_else(|p| p.into_inner());
            match map.get(app_id) {
                Some(slot) => Arc::clone(slot),
                None => {
                    let store = AppStore::open(&self.db_path(app_id), app_id, *limits)?;
                    let slot = Arc::new(StoreSlot {
                        store: Mutex::new(store),
                    });
                    map.insert(app_id.to_owned(), Arc::clone(&slot));
                    slot
                }
            }
        };
        // 限额变更（清单收紧）→ 重开
        {
            let store = slot.store.lock().unwrap_or_else(|p| p.into_inner());
            if store.limits() != *limits {
                drop(store);
                let fresh = AppStore::open(&self.db_path(app_id), app_id, *limits)?;
                let mut guard = slot.store.lock().unwrap_or_else(|p| p.into_inner());
                *guard = fresh;
            }
        }
        let store = slot.store.lock().unwrap_or_else(|p| p.into_inner());
        f(&store)
    }

    /// 应用卸载时删除其数据目录。
    pub fn drop_app(&self, app_id: &str) -> Result<(), StoreError> {
        validate_id(app_id).map_err(|e| StoreError::InvalidApp(e.to_string()))?;
        let removed = self
            .stores
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(app_id);
        drop(removed);
        let dir = self.root.join(app_id);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    fn db_path(&self, app_id: &str) -> PathBuf {
        self.root.join(app_id).join("app.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, AppStore) {
        let dir = TempDir::new().unwrap();
        let s = AppStore::open(&dir.path().join("app.db"), "t", Limits::default()).unwrap();
        (dir, s)
    }

    #[test]
    fn set_get_roundtrip() {
        let (_d, s) = store();
        s.set("k", b"{\"a\":1}").unwrap();
        assert_eq!(s.get("k").unwrap().unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn get_missing_returns_none() {
        let (_d, s) = store();
        assert!(s.get("nope").unwrap().is_none());
    }

    #[test]
    fn delete_is_idempotent() {
        let (_d, s) = store();
        s.delete("k").unwrap();
        s.set("k", b"1").unwrap();
        s.delete("k").unwrap();
        s.delete("k").unwrap();
        assert!(s.get("k").unwrap().is_none());
    }

    #[test]
    fn list_by_prefix_sorted() {
        let (_d, s) = store();
        s.set("u:a:2", b"2").unwrap();
        s.set("u:a:1", b"1").unwrap();
        s.set("u:b:1", b"3").unwrap();
        let items = s.list(Some("u:a:")).unwrap();
        let keys: Vec<_> = items.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["u:a:1", "u:a:2"]);
    }

    #[test]
    fn list_empty_prefix_returns_all() {
        let (_d, s) = store();
        s.set("a", b"1").unwrap();
        s.set("b", b"2").unwrap();
        assert_eq!(s.list(None).unwrap().len(), 2);
        assert_eq!(s.list(Some("")).unwrap().len(), 2);
    }

    #[test]
    fn list_prefix_escapes_wildcards() {
        let (_d, s) = store();
        s.set("a%b", b"1").unwrap();
        s.set("aXb", b"2").unwrap();
        let items = s.list(Some("a%")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "a%b");
    }

    #[test]
    fn key_with_slash_roundtrip() {
        let (_d, s) = store();
        s.set("ns/a/b", b"1").unwrap();
        assert_eq!(s.get("ns/a/b").unwrap().unwrap(), b"1");
        let items = s.list(Some("ns/")).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn key_too_long_rejected() {
        let (_d, s) = store();
        let key = "k".repeat(KEY_MAX + 1);
        assert!(matches!(
            s.set(&key, b"1"),
            Err(StoreError::KeyTooLong { .. })
        ));
    }

    #[test]
    fn value_too_large_rejected() {
        let (_d, s) = store();
        let big = vec![0u8; (Limits::default().kv_value_bytes + 1) as usize];
        assert!(matches!(
            s.set("k", &big),
            Err(StoreError::ValueTooLarge { .. })
        ));
    }

    #[test]
    fn quota_exceeded_on_total() {
        let (_d, s) = store();
        let mut limits = Limits::default();
        limits.kv_value_bytes = 100;
        limits.kv_total_bytes = 150;
        let s = AppStore::open(&_d.path().join("q.db"), "t", limits).unwrap();
        s.set("a", &vec![0u8; 100]).unwrap();
        assert!(matches!(
            s.set("b", &vec![0u8; 100]),
            Err(StoreError::QuotaExceeded { .. })
        ));
    }

    #[test]
    fn overwrite_uses_delta() {
        let (_d, s) = store();
        s.set("k", &vec![0u8; 100]).unwrap();
        s.set("k", &vec![0u8; 50]).unwrap();
        assert_eq!(s.usage().unwrap().bytes, 50);
    }

    #[test]
    fn batch_is_atomic() {
        let (_d, s) = store();
        s.set("k1", b"1").unwrap();
        let mut limits = Limits::default();
        limits.kv_value_bytes = 64;
        let s = AppStore::open(&_d.path().join("b.db"), "t", limits).unwrap();
        let ops = vec![
            KvOp::Set {
                key: "k2".into(),
                value: b"2".to_vec(),
            },
            KvOp::Set {
                key: "k3".into(),
                value: vec![0u8; 65],
            },
        ];
        assert!(s.batch(&ops).is_err());
        assert!(s.get("k2").unwrap().is_none());
    }

    #[test]
    fn batch_set_delete_applies() {
        let (_d, s) = store();
        s.set("k1", b"1").unwrap();
        s.set("k2", b"2").unwrap();
        let ops = vec![
            KvOp::Set {
                key: "k3".into(),
                value: b"3".to_vec(),
            },
            KvOp::Delete { key: "k1".into() },
        ];
        s.batch(&ops).unwrap();
        assert!(s.get("k1").unwrap().is_none());
        assert_eq!(s.get("k3").unwrap().unwrap(), b"3");
        assert_eq!(s.usage().unwrap().keys, 2);
    }

    #[test]
    fn usage_counts_bytes_and_keys() {
        let (_d, s) = store();
        s.set("a", b"12345").unwrap();
        s.set("b", b"12").unwrap();
        let u = s.usage().unwrap();
        assert_eq!(u.bytes, 7);
        assert_eq!(u.keys, 2);
    }

    #[test]
    fn clear_removes_all() {
        let (_d, s) = store();
        s.set("a", b"1").unwrap();
        s.set("b", b"2").unwrap();
        s.clear().unwrap();
        assert_eq!(s.usage().unwrap().keys, 0);
    }

    #[test]
    fn manager_opens_per_app() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        let l = Limits::default();
        m.with("a", &l, |s| s.set("k", b"1")).unwrap();
        m.with("b", &l, |s| s.set("k", b"2")).unwrap();
        let va = m.with("a", &l, |s| s.get("k")).unwrap().unwrap();
        let vb = m.with("b", &l, |s| s.get("k")).unwrap().unwrap();
        assert_ne!(va, vb);
    }

    #[test]
    fn manager_isolates_apps() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        let l = Limits::default();
        m.with("a", &l, |s| s.set("k", b"only-a")).unwrap();
        let missing = m.with("b", &l, |s| s.get("k")).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn manager_rejects_invalid_id() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        assert!(matches!(
            m.with("../evil", &Limits::default(), |_| Ok(())),
            Err(StoreError::InvalidApp(_))
        ));
    }

    #[test]
    fn manager_reopens_on_tightened_limits() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        let l = Limits::default();
        m.with("a", &l, |s| s.set("k", b"1")).unwrap();
        let mut tight = l;
        tight.kv_value_bytes = 8;
        assert!(matches!(
            m.with("a", &tight, |s| s.set("k", &vec![0u8; 16])),
            Err(StoreError::ValueTooLarge { .. })
        ));
    }

    #[test]
    fn drop_app_removes_dir() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        let l = Limits::default();
        m.with("a", &l, |s| s.set("k", b"1")).unwrap();
        m.drop_app("a").unwrap();
        assert!(!dir.path().join("a").exists());
    }
}
