# pegboard-core / store 模块

## 接口

```rust
// crates/pegboard-core/src/store/mod.rs

use std::path::{Path, PathBuf};
use crate::config::Limits;

/// 单条 KV 记录。
#[derive(Debug, Clone)]
pub struct KvEntry {
    pub key: String,
    pub value: Vec<u8>,       // JSON 字节，store 不解释
}

/// 批量操作项。
#[derive(Debug, Clone)]
pub enum KvOp {
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
}

/// 应用用量快照，用于配额判断与展示。
#[derive(Debug, Clone, Copy, Default)]
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
    #[error("key too long: {len} > {max}")]
    KeyTooLong { len: usize, max: usize },
    #[error("value too large: {len} > {max}")]
    ValueTooLarge { len: u64, max: u64 },
    #[error("quota exceeded: used {used} + {add} > {max}")]
    QuotaExceeded { used: u64, add: u64, max: u64 },
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// 单应用的 KV 存储。持有独立连接，非 Send 使用场景下由外层加锁。
pub struct AppStore {
    conn: rusqlite::Connection,
    limits: Limits,
    app_id: String,
}

impl AppStore {
    /// 打开或创建 apps/<id>/app.db，执行迁移。
    pub fn open(db_path: &Path, app_id: &str, limits: Limits) -> Result<Self, StoreError>;

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// 写入；先校验单值与总量配额，再落库。
    pub fn set(&self, key: &str, value: &[u8]) -> Result<(), StoreError>;

    pub fn delete(&self, key: &str) -> Result<(), StoreError>;

    /// 前缀查询，按 key 字典序。
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<KvEntry>, StoreError>;

    /// 原子批量：事务内执行，任一失败全部回滚。
    pub fn batch(&self, ops: &[KvOp]) -> Result<(), StoreError>;

    /// 当前用量。
    pub fn usage(&self) -> Result<StoreUsage, StoreError>;

    /// 清空该应用所有 KV（卸载时用）。
    pub fn clear(&self) -> Result<(), StoreError>;
}

/// 按 app_id 管理多个 AppStore，惰性打开，缓存连接。
pub struct StoreManager {
    root: PathBuf,               // data_root/apps/
    default_limits: Limits,
    stores: std::sync::Mutex<std::collections::HashMap<String, AppStore>>,
}

impl StoreManager {
    pub fn new(root: PathBuf, default_limits: Limits) -> Self;

    /// 获取或打开某应用的 store。limits 由 AppMeta 传入。
    pub fn with<F, R>(&self, app_id: &str, limits: &Limits, f: F) -> Result<R, StoreError>
    where F: FnOnce(&AppStore) -> Result<R, StoreError>;

    /// 应用卸载时删除其 store 目录。
    pub fn drop_app(&self, app_id: &str) -> Result<(), StoreError>;
}

/// 打开连接时的通用初始化：WAL、外键、busy_timeout。
fn open_conn(path: &Path) -> Result<rusqlite::Connection, StoreError>;

/// 建表或迁移。
fn migrate(conn: &rusqlite::Connection) -> Result<(), StoreError>;

/// key 长度上限常量。
const KEY_MAX: usize = 512;
```

## 落盘

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS kv (
  key        TEXT PRIMARY KEY,
  value      BLOB NOT NULL,
  size       INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_kv_updated ON kv(updated_at);

CREATE TABLE IF NOT EXISTS schema_migrations (
  version    INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL
);
```

路径：`<data_root>/apps/<app_id>/app.db`。

## 行为规则

- **key**：UTF-8，长度 ≤ `KEY_MAX`；空字符串允许；不做归一化，应用自负语义。
- **value**：任意字节，store 不解释；单值上限 `limits.kv_value_bytes`。
- **set**：先算 `add = value.len()`，再查 `usage.bytes + add` 是否超 `kv_total_bytes`；超则 `QuotaExceeded`。
- **覆盖写**：目标 key 已存在时，`add = new_len - old_len`（可为负），据此判断配额。
- **list**：`prefix` 为空或 `None` 返回全部；用 SQL `LIKE prefix || '%'`，需转义 `%` `_` `\`。
- **batch**：单事务；预检全部 op 的配额（合并中间态），再逐条执行。
- **delete**：不存在则静默成功（幂等）。
- **usage**：`SELECT COALESCE(SUM(size),0), COUNT(*) FROM kv`。
- **clear**：`DELETE FROM kv`；不删库文件。

## 并发

- `AppStore` 内部单连接，调用方加锁（`StoreManager` 用 `Mutex` 保护 map）。
- WAL 允许多读单写；同一 app 的写串行化由 `Mutex` 保证。
- core 为同步 API；server 侧用 `spawn_blocking` 包装，避免阻塞 tokio worker。

## 单元测试骨架

```rust
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
        s.set("u:a:1", b"1").unwrap();
        s.set("u:a:2", b"2").unwrap();
        s.set("u:b:1", b"3").unwrap();
        let items = s.list(Some("u:a:")).unwrap();
        let keys: Vec<_> = items.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["u:a:1", "u:a:2"]);
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
    fn value_too_large_rejected() {
        let (_d, s) = store();
        let big = vec![0u8; (Limits::default().kv_value_bytes + 1) as usize];
        assert!(matches!(s.set("k", &big), Err(StoreError::ValueTooLarge { .. })));
    }

    #[test]
    fn quota_exceeded_on_total() {
        let (_d, s) = store();
        // 连续写入直到超 kv_total_bytes，断言 QuotaExceeded
    }

    #[test]
    fn overwrite_uses_delta() {
        let (_d, s) = store();
        s.set("k", &vec![0u8; 100]).unwrap();
        s.set("k", &vec![0u8; 50]).unwrap();  // 覆盖减小，不应触发配额
        assert_eq!(s.usage().unwrap().bytes, 50);
    }

    #[test]
    fn batch_is_atomic() {
        let (_d, s) = store();
        s.set("k1", b"1").unwrap();
        let ops = vec![
            KvOp::Set { key: "k2".into(), value: b"2".to_vec() },
            KvOp::Set { key: "k3".into(), value: vec![0u8; u64::MAX as usize] }, // 必然失败
        ];
        assert!(s.batch(&ops).is_err());
        // k2 不应存在
        assert!(s.get("k2").unwrap().is_none());
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
    fn drop_app_removes_dir() {
        let dir = TempDir::new().unwrap();
        let m = StoreManager::new(dir.path().into(), Limits::default());
        let l = Limits::default();
        m.with("a", &l, |s| s.set("k", b"1")).unwrap();
        m.drop_app("a").unwrap();
        assert!(!dir.path().join("a").exists());
    }
}
```

## 依赖

```toml
[dependencies]
rusqlite = { workspace = true, features = ["bundled"] }
thiserror = { workspace = true }
serde_json = { workspace = true }   # 若在 core 层做 JSON 校验；否则可省

[dev-dependencies]
tempfile = { workspace = true }
```

`bundled` 特性避免系统 SQLite 依赖，单二进制分发必需。

## 设计要点

- **同步核心，异步外壳**：core 不引入 tokio；server 用 `spawn_blocking` 调用。这样 core 可独立测试、无运行时依赖。
- **配额在写入前**：先算后写，避免写一半失败留脏数据。
- **覆盖写用增量**：`new_len - old_len`，避免“原地改小”被误判超限。
- **批量原子**：预检合并中间态，再事务执行；失败全回滚。
- **前缀转义**：`%` `_` `\` 必须转义，否则语义错误。
- **连接缓存**：`StoreManager` 避免每请求开库；应用卸载时显式释放。
- **WAL + busy_timeout**：单写多读，写入串行由 `Mutex` 兜底。
- **不解释 value**：JSON 校验留给上层；store 只管字节与配额。
