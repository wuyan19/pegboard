# pegboard-core / files 模块

## 接口

```rust
// crates/pegboard-core/src/files/mod.rs

use std::path::{Path, PathBuf};
use crate::config::Limits;

/// 文件元数据。
#[derive(Debug, Clone)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: i64,
}

/// 分页结果。
#[derive(Debug, Clone)]
pub struct Page {
    pub items: Vec<FileMeta>,
    pub next: Option<String>,   // 游标，None 表示结束
}

/// 上传来源：流式读取。
pub trait ByteSource: std::io::Read {}

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("file too large: {size} > {max}")]
    TooLarge { size: u64, max: u64 },
    #[error("quota exceeded: used {used} + {add} > {max}")]
    QuotaExceeded { used: u64, add: u64, max: u64 },
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("token invalid or expired")]
    TokenInvalid,
}

/// 单应用的文件存储。
pub struct AppFiles {
    conn: rusqlite::Connection,
    root: PathBuf,           // apps/<app_id>/files/
    tmp: PathBuf,            // apps/<app_id>/tmp/
    limits: Limits,
    app_id: String,
}

impl AppFiles {
    pub fn open(dir: &Path, app_id: &str, limits: Limits) -> Result<Self, FilesError>;

    /// 流式上传：边读边计数、算 sha256、写 tmp，成功后再落位。
    /// 超单文件或总量配额立即中断，不留残留。
    pub fn upload<R: std::io::Read>(
        &self,
        name: &str,
        mime: &str,
        src: R,
        signer: &Signer,     // 见下方签名服务
    ) -> Result<FileMeta, FilesError>;

    /// 打开内容读取。返回的句柄支持 Range 由上层处理。
    pub fn open(&self, id: &str) -> Result<Option<(FileMeta, std::fs::File)>, FilesError>;

    pub fn meta(&self, id: &str) -> Result<Option<FileMeta>, FilesError>;

    pub fn delete(&self, id: &str) -> Result<(), FilesError>;

    /// 游标分页；cursor 为空从头部开始。
    pub fn list(&self, cursor: Option<&str>, limit: u32) -> Result<Page, FilesError>;

    /// 用量：文件总字节与数量。
    pub fn usage(&self) -> Result<(u64, u64), FilesError>;

    /// 清理：孤儿内容（无元数据引用）、孤儿元数据（内容缺失）。
    pub fn gc(&self) -> Result<GcReport, FilesError>;

    /// 清空该应用全部文件与元数据。
    pub fn clear(&self) -> Result<(), FilesError>;
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub orphan_contents: u64,
    pub missing_contents: u64,
}

/// 签名 token 服务，独立于文件数据，存 host.db。
pub struct Signer {
    conn: rusqlite::Connection,
}

impl Signer {
    pub fn open(db_path: &Path) -> Result<Self, FilesError>;

    /// 为 (app_id, file_id) 签发 ttl 秒有效的 token，返回明文 token。
    /// 明文只在此刻返回；库里存哈希。
    pub fn sign(&self, app_id: &str, file_id: &str, ttl_sec: u64,
                ttl_max: u64) -> Result<String, FilesError>;

    /// 校验：存在、未过期、app 与 file 匹配。
    pub fn verify(&self, token: &str, app_id: &str, file_id: &str)
        -> Result<(), FilesError>;

    /// 清理过期 token。
    pub fn gc(&self) -> Result<u64, FilesError>;
}

/// 按 app_id 管理 AppFiles，惰性打开，缓存句柄。
pub struct FilesManager {
    root: PathBuf,
    default_limits: Limits,
    signer: std::sync::Arc<Signer>,
    stores: std::sync::Mutex<std::collections::HashMap<String, AppFiles>>,
}

impl FilesManager {
    pub fn new(root: PathBuf, default_limits: Limits, signer: std::sync::Arc<Signer>) -> Self;

    pub fn with<F, R>(&self, app_id: &str, limits: &Limits, f: F) -> Result<R, FilesError>
    where F: FnOnce(&AppFiles) -> Result<R, FilesError>;

    pub fn drop_app(&self, app_id: &str) -> Result<(), FilesError>;
}

/// 生成 ULID。
fn new_id() -> String;
```

## 落盘

```text
<data_root>/apps/<app_id>/
  app.db                    # KV + files 元数据 + schema_migrations
  files/<aa>/<bb>/<file_id> # 内容，aa/bb 为 sha256 前 4 位
  tmp/<file_id>.part        # 上传暂存
```

元数据表（与 kv 同库）：

```sql
CREATE TABLE IF NOT EXISTS files (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  mime       TEXT NOT NULL,
  size       INTEGER NOT NULL,
  sha256     TEXT NOT NULL,
  path       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  refcount   INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_files_created ON files(created_at);
CREATE INDEX IF NOT EXISTS idx_files_sha     ON files(sha256);
```

签名 token 表（host.db）：

```sql
CREATE TABLE IF NOT EXISTS signed_tokens (
  token_hash TEXT PRIMARY KEY,
  app_id     TEXT NOT NULL,
  file_id    TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tokens_expires ON signed_tokens(expires_at);
```

## 上传流程

1. 校验文件名：非空、不含路径分隔符、长度上限。
2. 写 `tmp/<id>.part`：边读边算 sha256、累计 size；超单文件上限立即中断并删暂存。
3. 读完后查总量配额：`usage.bytes + size > file_total_bytes` → 中断并删暂存。
4. 查 `files.sha256` 是否已有：有则复用内容路径，`refcount += 1`；无则 `rename` 到 `files/<aa>/<bb>/<id>`。
5. 事务插入元数据，提交。
6. 失败任一步：删暂存文件，回滚事务。

## 行为规则

- **id**：ULID，单调、可排序、全局唯一。
- **name**：保留原始名，仅做展示；不参与路径。
- **path**：相对 `files/`，由宿主生成；禁止应用指定。
- **去重**：同 `sha256` 复用内容，元数据各自 `refcount`。
- **删除**：`refcount` 减一；归零删元数据，内容由 `gc` 异步删。
- **list**：按 `created_at DESC, id DESC`；cursor 为上一页最后一条的 `(created_at, id)`。
- **签名**：`token` 明文返回一次；库里存 `sha256(token)`；`verify` 用固定时间比较。
- **上传 mime**：由上层从 multipart 头传入；不做嗅探。
- **Range**：`open` 只给元数据 + 文件句柄；`Range` 解析与 `206` 由 server 侧处理。

## 并发

- 同一 app 的 `AppFiles` 单连接 + 单写者；`Mutex` 由 `FilesManager` 保证。
- 内容去重时存在竞态：两个上传同 sha256 同时落位。用目标路径 `rename`（原子）后写元数据；后到者发现已存在则复用，不重写。
- `gc` 与上传并发：`gc` 只删“无元数据引用”的内容；用元数据表判定，避免误删。

## 单元测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, AppFiles, std::sync::Arc<Signer>) {
        let d = TempDir::new().unwrap();
        let signer = std::sync::Arc::new(Signer::open(&d.path().join("host.db")).unwrap());
        let f = AppFiles::open(&d.path().join("a"), "a", Limits::default()).unwrap();
        (d, f, signer)
    }

    #[test]
    fn upload_and_open_roundtrip() {
        let (_d, f, s) = setup();
        let m = f.upload("a.txt", "text/plain", &b"hello"[..], &s).unwrap();
        let (meta, mut file) = f.open(&m.id).unwrap().unwrap();
        let mut buf = String::new();
        use std::io::Read;
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello");
        assert_eq!(meta.size, 5);
    }

    #[test]
    fn sha256_dedup_shares_content() {
        let (_d, f, s) = setup();
        let a = f.upload("a", "text/plain", &b"same"[..], &s).unwrap();
        let b = f.upload("b", "text/plain", &b"same"[..], &s).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.sha256, b.sha256);
        // 内容只有一份
    }

    #[test]
    fn too_large_rejected_and_no_residue() {
        let (_d, f, s) = setup();
        let big = vec![0u8; (Limits::default().file_bytes + 1) as usize];
        assert!(matches!(
            f.upload("big", "x", &big[..], &s),
            Err(FilesError::TooLarge { .. })
        ));
        assert_eq!(f.usage().unwrap().0, 0);
        // tmp 目录为空
    }

    #[test]
    fn total_quota_exceeded() {
        // 连续上传直到超 file_total_bytes，断言 QuotaExceeded
    }

    #[test]
    fn delete_refcount_and_gc() {
        let (_d, f, s) = setup();
        let a = f.upload("a", "x", &b"same"[..], &s).unwrap();
        let b = f.upload("b", "x", &b"same"[..], &s).unwrap();
        f.delete(&a.id).unwrap();
        assert!(f.meta(&a.id).unwrap().is_none());
        // b 仍可读
        assert!(f.open(&b.id).unwrap().is_some());
        f.delete(&b.id).unwrap();
        let r = f.gc().unwrap();
        assert_eq!(r.orphan_contents, 1);
    }

    #[test]
    fn list_paginates() {
        let (_d, f, s) = setup();
        for i in 0..5 {
            f.upload(&format!("f{i}"), "x", format!("{i}").as_bytes(), &s).unwrap();
        }
        let p1 = f.list(None, 2).unwrap();
        assert_eq!(p1.items.len(), 2);
        assert!(p1.next.is_some());
        let p2 = f.list(p1.next.as_deref(), 10).unwrap();
        assert_eq!(p2.items.len(), 3);
        assert!(p2.next.is_none());
    }

    #[test]
    fn invalid_name_rejected() {
        let (_d, f, s) = setup();
        assert!(matches!(
            f.upload("../etc/passwd", "x", &b""[..], &s),
            Err(FilesError::InvalidName(_))
        ));
    }

    #[test]
    fn sign_verify_and_expire() {
        let d = TempDir::new().unwrap();
        let s = Signer::open(&d.path().join("host.db")).unwrap();
        let t = s.sign("a", "f1", 60, 3600).unwrap();
        assert!(s.verify(&t, "a", "f1").is_ok());
        assert!(matches!(s.verify(&t, "b", "f1"), Err(FilesError::TokenInvalid)));
        assert!(matches!(s.verify(&t, "a", "f2"), Err(FilesError::TokenInvalid)));
        // 过期
        let t2 = s.sign("a", "f1", 0, 3600).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(matches!(s.verify(&t2, "a", "f1"), Err(FilesError::TokenInvalid)));
    }

    #[test]
    fn sign_ttl_capped() {
        let d = TempDir::new().unwrap();
        let s = Signer::open(&d.path().join("host.db")).unwrap();
        assert!(s.sign("a", "f1", 999_999, 60).is_err());
    }

    #[test]
    fn signer_gc_removes_expired() {
        // 签发过期 token，gc 后 verify 失败
    }

    #[test]
    fn manager_isolates_apps() {
        // 两个 app 同名文件互不可见
    }
}
```

## 依赖

```toml
[dependencies]
rusqlite = { workspace = true, features = ["bundled"] }
sha2 = { workspace = true }
ulid = { workspace = true }
thiserror = { workspace = true }
hex = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

## 设计要点

- **流式**：上传边读边算，绝不整文件入内存；超限即断。
- **原子落位**：先写 `tmp/`，`rename` 到目标。同盘保证原子。
- **去重**：sha256 复用内容，元数据各自 refcount；删除延迟回收。
- **签名与数据分离**：token 存 `host.db`，只回答“有效吗”，不携带身份。
- **token 存哈希**：明文只返回一次；固定时间比较，防时序侧信道。
- **路径由宿主生成**：应用只给 name 与字节，不给路径。
- **GC 独立**：回收孤儿内容与缺失元数据，不阻塞主路径。
- **同步核心**：与 store 一致，server 侧 `spawn_blocking`。
