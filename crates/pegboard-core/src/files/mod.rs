//! Blob 原语与签名：流式落盘、元数据与内容分离、sha256 去重、签名 token。
//!
//! 签名只回答「链接有效吗」，不携带身份语义（数据模型 §5）。
//! token 存取经 app 模块的 HostDb 接口（本模块不直接打开 host.db）。
//! 同步 API；server 侧 spawn_blocking 包装。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::app::HostDb;
use crate::config::Limits;

/// 文件名长度上限。
const NAME_MAX: usize = 255;

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
    /// 游标，None 表示结束
    pub next: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("host db: {0}")]
    HostDb(String),
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
    #[error("entropy unavailable: {0}")]
    Entropy(String),
}

/// 单应用的文件存储。元数据在 app.db 的 files 表，内容在 files/ 下按哈希分片。
pub struct AppFiles {
    conn: Mutex<rusqlite::Connection>,
    /// apps_data/<app_id>/
    root: PathBuf,
    /// data_root/tmp/（与最终存储同盘，保证 rename 原子）
    tmp: PathBuf,
    limits: Limits,
    #[allow(dead_code)]
    app_id: String,
}

impl AppFiles {
    /// `dir` 为该应用数据目录（apps_data/<app_id>），`tmp` 为数据根共享暂存目录。
    pub fn open(dir: &Path, tmp: &Path, app_id: &str, limits: Limits) -> Result<Self, FilesError> {
        std::fs::create_dir_all(dir.join("files"))?;
        std::fs::create_dir_all(tmp)?;
        let conn = rusqlite::Connection::open(dir.join("app.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
               id         TEXT PRIMARY KEY,
               name       TEXT NOT NULL,
               mime       TEXT NOT NULL,
               size       INTEGER NOT NULL,
               sha256     TEXT NOT NULL,
               path       TEXT NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_files_created ON files(created_at);
             CREATE INDEX IF NOT EXISTS idx_files_sha ON files(sha256);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            root: dir.to_owned(),
            tmp: tmp.to_owned(),
            limits,
            app_id: app_id.to_owned(),
        })
    }

    /// 生效限额（清单收紧后由 FilesManager 比对触发重开）。
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// 流式上传：边读边计数、算 sha256、写 tmp，成功后再落位。
    /// 超单文件或总量配额立即中断，不留残留。
    pub fn upload<R: Read>(&self, name: &str, mime: &str, src: R) -> Result<FileMeta, FilesError> {
        validate_name(name)?;
        let id = new_id();
        let staged = self.tmp.join(format!("{id}.part"));
        // 边读边算：sha256 + 大小；超单文件上限立即中断
        let mut hasher = Sha256::new();
        let mut writer = std::io::BufWriter::new(std::fs::File::create(&staged)?);
        let mut reader = std::io::BufReader::new(src);
        let mut size: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > self.limits.file_bytes {
                let _ = std::fs::remove_file(&staged);
                return Err(FilesError::TooLarge {
                    size,
                    max: self.limits.file_bytes,
                });
            }
            hasher.update(&buf[..n]);
            std::io::Write::write_all(&mut writer, &buf[..n])?;
        }
        std::io::Write::flush(&mut writer)?;
        drop(writer);

        let cleanup = |p: &Path| {
            let _ = std::fs::remove_file(p);
        };
        let sha256 = hex::encode(hasher.finalize());
        // 总量配额：写入前校验
        let (used, _count) = self.usage()?;
        if used + size > self.limits.file_total_bytes {
            cleanup(&staged);
            return Err(FilesError::QuotaExceeded {
                used,
                add: size,
                max: self.limits.file_total_bytes,
            });
        }
        // 去重：同 sha256 已有内容则复用，否则 rename 落位
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let existing: Option<String> = conn
            .query_row(
                "SELECT path FROM files WHERE sha256 = ?1 LIMIT 1",
                [&sha256],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let rel_path = match existing {
            Some(existing_path) => {
                cleanup(&staged);
                existing_path
            }
            None => {
                let rel = format!("{}/{}/{}", &sha256[0..2], &sha256[2..4], id);
                let dest = self.root.join("files").join(&rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if dest.exists() {
                    cleanup(&staged);
                } else {
                    std::fs::rename(&staged, &dest)?;
                }
                rel
            }
        };
        let created_at = now_ms();
        conn.execute(
            "INSERT INTO files(id, name, mime, size, sha256, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, name, mime, size as i64, sha256, rel_path, created_at],
        )?;
        Ok(FileMeta {
            id,
            name: name.to_owned(),
            mime: mime.to_owned(),
            size,
            sha256,
            created_at,
        })
    }

    /// 打开内容读取。Range 解析与 206 由 server 侧处理。
    pub fn open_content(&self, id: &str) -> Result<Option<(FileMeta, std::fs::File)>, FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let Some(meta) = meta_of(&conn, id)? else {
            return Ok(None);
        };
        drop(conn);
        let path = self.root.join("files").join(&self.rel_path_of(&meta)?);
        match std::fs::File::open(&path) {
            Ok(file) => Ok(Some((meta, file))),
            Err(_) => Ok(None),
        }
    }

    /// 元数据查询（不触碰内容）。
    pub fn meta(&self, id: &str) -> Result<Option<FileMeta>, FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        meta_of(&conn, id)
    }

    fn rel_path_of(&self, meta: &FileMeta) -> Result<String, FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row("SELECT path FROM files WHERE id = ?1", [&meta.id], |r| {
            r.get(0)
        })
        .map_err(FilesError::Db)
    }

    /// 删除：删元数据行；按 sha256 统计剩余引用，归零回收内容（延迟回收由 gc 兜底）。
    pub fn delete(&self, id: &str) -> Result<(), FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let row: Option<(String, String)> = conn
            .query_row("SELECT sha256, path FROM files WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((sha256, rel_path)) = row else {
            return Ok(()); // 幂等
        };
        conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE sha256 = ?1",
            [&sha256],
            |r| r.get(0),
        )?;
        if remaining == 0 {
            let _ = std::fs::remove_file(self.root.join("files").join(&rel_path));
        }
        Ok(())
    }

    /// 游标分页 + 名称前缀；按 created_at DESC, id DESC。cursor 为上一页 next。
    pub fn list(
        &self,
        prefix: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page, FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let limit = limit.clamp(1, 200);
        let prefix = prefix.unwrap_or("");
        let escaped = escape_like(prefix);
        let mut sql = String::from(
            "SELECT id, name, mime, size, sha256, created_at FROM files WHERE name LIKE ?1 ESCAPE '\\'",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(escaped)];
        if let Some(cursor) = cursor {
            // cursor = "created_at:id"
            if let Some((created, id)) = cursor.split_once(':') {
                sql.push_str(" AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))");
                let created: i64 = created
                    .parse()
                    .map_err(|_| FilesError::NotFound(format!("游标非法: {cursor}")))?;
                params.push(Box::new(created));
                params.push(Box::new(id.to_owned()));
            }
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?N");
        // 手动绑定位置参数
        let placeholders: Vec<String> = (1..=params.len() + 1).map(|i| format!("?{i}")).collect();
        let np = placeholders.last().cloned().unwrap_or_default();
        sql = sql.replace("?N", &np);
        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|p| p.as_ref())
            .chain([(&limit as &dyn rusqlite::ToSql)])
            .collect();
        let rows = stmt.query_map(params_refs.as_slice(), row_to_meta)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        let next = if items.len() as u32 >= limit {
            items.last().map(|m| format!("{}:{}", m.created_at, m.id))
        } else {
            None
        };
        Ok(Page { items, next })
    }

    /// 用量：文件总字节与数量。
    pub fn usage(&self) -> Result<(u64, u64), FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row(
            "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM files",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?.max(0) as u64,
                    r.get::<_, i64>(1)?.max(0) as u64,
                ))
            },
        )
        .map_err(FilesError::Db)
    }

    /// 清理：孤儿内容（无元数据引用）、孤儿元数据（内容缺失，标记删除）。
    pub fn gc(&self) -> Result<GcReport, FilesError> {
        let mut report = GcReport::default();
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // 引用的相对路径集合
        let mut stmt = conn.prepare("SELECT path FROM files")?;
        let referenced: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);
        // 元数据存在但内容缺失 → 删除元数据行
        let mut stmt = conn.prepare("SELECT id, path FROM files")?;
        let all: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);
        for (id, rel) in &all {
            if !self.root.join("files").join(rel).is_file() {
                conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
                report.missing_contents += 1;
            }
        }
        // files/ 中存在但无引用 → 删除
        let files_root = self.root.join("files");
        let mut stack = vec![files_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(&files_root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if !referenced.contains(&rel) && std::fs::remove_file(&path).is_ok() {
                        report.orphan_contents += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// 清空该应用全部文件与元数据。
    pub fn clear(&self) -> Result<(), FilesError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute("DELETE FROM files", [])?;
        drop(conn);
        let files_root = self.root.join("files");
        if files_root.exists() {
            std::fs::remove_dir_all(&files_root)?;
            std::fs::create_dir_all(&files_root)?;
        }
        Ok(())
    }
}

fn row_to_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileMeta> {
    Ok(FileMeta {
        id: row.get(0)?,
        name: row.get(1)?,
        mime: row.get(2)?,
        size: row.get::<_, i64>(3)?.max(0) as u64,
        sha256: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn meta_of(conn: &rusqlite::Connection, id: &str) -> Result<Option<FileMeta>, FilesError> {
    let mut stmt =
        conn.prepare("SELECT id, name, mime, size, sha256, created_at FROM files WHERE id = ?1")?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_meta(row)?)),
        None => Ok(None),
    }
}

fn validate_name(name: &str) -> Result<(), FilesError> {
    if name.is_empty()
        || name.len() > NAME_MAX
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(FilesError::InvalidName(name.to_owned()));
    }
    Ok(())
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

#[derive(Debug, Default)]
pub struct GcReport {
    pub orphan_contents: u64,
    pub missing_contents: u64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 生成 ULID。
fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// 签名 token 服务。存取经 app::HostDb 接口；只回答「有效吗」。
pub struct Signer {
    host: Arc<HostDb>,
}

impl Signer {
    pub fn new(host: Arc<HostDb>) -> Self {
        Self { host }
    }

    /// 为 (app_id, file_id) 签发 ttl 秒有效的 token，返回明文。
    /// 明文只在此刻返回；库里存哈希。ttl 超过 ttl_max 拒绝（sign_ttl_max 不可覆盖）。
    pub fn sign(
        &self,
        app_id: &str,
        file_id: &str,
        ttl_sec: u64,
        ttl_max: u64,
    ) -> Result<String, FilesError> {
        if ttl_sec == 0 || ttl_sec > ttl_max {
            return Err(FilesError::InvalidName(format!(
                "ttl 须在 1..={ttl_max} 秒"
            )));
        }
        let token = random_token()?;
        let hash = token_hash(&token);
        let expires_at = now_ms() + (ttl_sec as i64) * 1000;
        self.host
            .insert_token(&hash, app_id, file_id, expires_at)
            .map_err(|e: crate::app::HostDbError| FilesError::HostDb(e.to_string()))?;
        Ok(token)
    }

    /// 校验：存在、未过期、app 与 file 匹配。
    pub fn verify(&self, token: &str, app_id: &str, file_id: &str) -> Result<(), FilesError> {
        let hash = token_hash(token);
        let row = self
            .host
            .lookup_token(&hash)
            .map_err(|e: crate::app::HostDbError| FilesError::HostDb(e.to_string()))?;
        let Some(row) = row else {
            return Err(FilesError::TokenInvalid);
        };
        // 固定时间比较（防时序侧信道）
        let app_ok = fixed_time_eq(row.app_id.as_bytes(), app_id.as_bytes());
        let file_ok = fixed_time_eq(row.file_id.as_bytes(), file_id.as_bytes());
        if !app_ok || !file_ok || row.expires_at <= now_ms() {
            return Err(FilesError::TokenInvalid);
        }
        Ok(())
    }

    /// 清理过期 token，返回删除数。
    pub fn gc(&self) -> Result<u64, FilesError> {
        self.host
            .gc_tokens()
            .map_err(|e: crate::app::HostDbError| FilesError::HostDb(e.to_string()))
    }

    /// 按 token + file_id 解析归属应用（签名路径用：token → app_id）。
    /// 存在、未过期、file 匹配才返回 Some(app_id)。
    pub fn resolve(&self, token: &str, file_id: &str) -> Result<Option<String>, FilesError> {
        let hash = token_hash(token);
        let row = self
            .host
            .lookup_token(&hash)
            .map_err(|e: crate::app::HostDbError| FilesError::HostDb(e.to_string()))?;
        match row {
            Some(row)
                if row.expires_at > now_ms()
                    && fixed_time_eq(row.file_id.as_bytes(), file_id.as_bytes()) =>
            {
                Ok(Some(row.app_id))
            }
            _ => Ok(None),
        }
    }
}

/// 服务层暂存文件名（multipart 先落 data_root/tmp，再交 core 落位）。
pub fn new_staging_name() -> String {
    format!("{}.part", ulid::Ulid::new())
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

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

/// 256 位随机 token（URL 安全 hex）。熵源：/dev/urandom（unix）。
fn random_token() -> Result<String, FilesError> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")
        .map_err(|e| FilesError::Entropy(format!("/dev/urandom: {e}")))?;
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf)
        .map_err(|e| FilesError::Entropy(e.to_string()))?;
    Ok(hex::encode(buf))
}

/// 按 app_id 管理文件存储，惰性打开，缓存句柄。
pub struct FilesManager {
    /// apps_data/
    root: PathBuf,
    /// data_root/tmp/
    tmp: PathBuf,
    files: Mutex<HashMap<String, Arc<FilesSlot>>>,
}

struct FilesSlot {
    files: Mutex<AppFiles>,
}

impl FilesManager {
    pub fn new(root: PathBuf, tmp: PathBuf) -> Self {
        Self {
            root,
            tmp,
            files: Mutex::new(HashMap::new()),
        }
    }

    /// 获取或打开某应用的文件存储并执行 f。limits 来自 AppMeta；清单收紧后重开。
    pub fn with<F, R>(&self, app_id: &str, limits: &Limits, f: F) -> Result<R, FilesError>
    where
        F: FnOnce(&AppFiles) -> Result<R, FilesError>,
    {
        crate::app::validate_id(app_id)
            .map_err(|e| FilesError::InvalidName(format!("app id: {e}")))?;
        let slot = {
            let mut map = self.files.lock().unwrap_or_else(|p| p.into_inner());
            match map.get(app_id) {
                Some(slot) => Arc::clone(slot),
                None => {
                    let app_files =
                        AppFiles::open(&self.root.join(app_id), &self.tmp, app_id, *limits)?;
                    let slot = Arc::new(FilesSlot {
                        files: Mutex::new(app_files),
                    });
                    map.insert(app_id.to_owned(), Arc::clone(&slot));
                    slot
                }
            }
        };
        {
            let guard = slot.files.lock().unwrap_or_else(|p| p.into_inner());
            if guard.limits() != *limits {
                drop(guard);
                let fresh = AppFiles::open(&self.root.join(app_id), &self.tmp, app_id, *limits)?;
                let mut guard = slot.files.lock().unwrap_or_else(|p| p.into_inner());
                *guard = fresh;
            }
        }
        let guard = slot.files.lock().unwrap_or_else(|p| p.into_inner());
        f(&guard)
    }

    /// 应用卸载时删除其数据目录。
    pub fn drop_app(&self, app_id: &str) -> Result<(), FilesError> {
        let removed = self
            .files
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, AppFiles, Arc<Signer>) {
        let d = TempDir::new().unwrap();
        let host = Arc::new(HostDb::open(&d.path().join("host.db")).unwrap());
        let signer = Arc::new(Signer::new(Arc::clone(&host)));
        let f = AppFiles::open(
            &d.path().join("apps_data/a"),
            &d.path().join("tmp"),
            "a",
            Limits::default(),
        )
        .unwrap();
        (d, f, signer)
    }

    #[test]
    fn upload_and_open_roundtrip() {
        let (_d, f, _s) = setup();
        let m = f.upload("a.txt", "text/plain", &b"hello"[..]).unwrap();
        let (meta, mut file) = f.open_content(&m.id).unwrap().unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello");
        assert_eq!(meta.size, 5);
        assert_eq!(meta.mime, "text/plain");
        assert_eq!(meta.sha256, hex::encode(Sha256::digest(b"hello")));
    }

    #[test]
    fn sha256_dedup_shares_content() {
        let (d, f, _s) = setup();
        let a = f.upload("a", "text/plain", &b"same"[..]).unwrap();
        let b = f.upload("b", "text/plain", &b"same"[..]).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.sha256, b.sha256);
        // 内容只有一份：usage 计两行元数据（size 各计），磁盘内容唯一由 gc 验证
        let (bytes, count) = f.usage().unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, 8);
        let report = f.gc().unwrap();
        assert_eq!(report.orphan_contents, 0);
        // tmp 无残留
        let tmp_left: Vec<_> = std::fs::read_dir(d.path().join("tmp"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(tmp_left.is_empty(), "{tmp_left:?}");
    }

    #[test]
    fn too_large_rejected_and_no_residue() {
        let (_d, f, _s) = setup();
        let big = vec![0u8; (Limits::default().file_bytes + 1) as usize];
        assert!(matches!(
            f.upload("big", "x", &big[..]),
            Err(FilesError::TooLarge { .. })
        ));
        assert_eq!(f.usage().unwrap().0, 0);
    }

    #[test]
    fn total_quota_exceeded() {
        let d = TempDir::new().unwrap();
        let limits = Limits {
            file_bytes: 100,
            file_total_bytes: 150,
            ..Limits::default()
        };
        let f = AppFiles::open(&d.path().join("a"), &d.path().join("tmp"), "a", limits).unwrap();
        f.upload("a", "x", &[0u8; 100][..]).unwrap();
        assert!(matches!(
            f.upload("b", "x", &[0u8; 100][..]),
            Err(FilesError::QuotaExceeded { .. })
        ));
    }

    #[test]
    fn delete_refcount_and_gc() {
        let (_d, f, _s) = setup();
        let a = f.upload("a", "x", &b"same"[..]).unwrap();
        let b = f.upload("b", "x", &b"same"[..]).unwrap();
        f.delete(&a.id).unwrap();
        assert!(f.meta(&a.id).unwrap().is_none());
        // b 仍可读（内容仍被引用）
        assert!(f.open_content(&b.id).unwrap().is_some());
        f.delete(&b.id).unwrap();
        // 引用归零：内容立即回收
        let report = f.gc().unwrap();
        assert_eq!(report.orphan_contents, 0);
        let files_dir = _d.path().join("apps_data/a/files");
        let remaining: Vec<_> = walk_files(&files_dir);
        assert!(remaining.is_empty(), "{remaining:?}");
    }

    #[test]
    fn delete_is_idempotent() {
        let (_d, f, _s) = setup();
        let m = f.upload("a", "x", &b"data"[..]).unwrap();
        f.delete(&m.id).unwrap();
        f.delete(&m.id).unwrap();
        assert!(f.meta(&m.id).unwrap().is_none());
    }

    #[test]
    fn gc_removes_orphan_content() {
        let (d, f, _s) = setup();
        let m = f.upload("a", "x", &b"data"[..]).unwrap();
        // 模拟孤儿：手工放置一个无引用内容文件
        let orphan = d.path().join("apps_data/a/files/ab/cd/orphan");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"junk").unwrap();
        let report = f.gc().unwrap();
        assert_eq!(report.orphan_contents, 1);
        assert!(f.open_content(&m.id).unwrap().is_some());
        assert!(!orphan.exists());
    }

    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_owned()];
        while let Some(d) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&d) {
                for e in entries.filter_map(Result::ok) {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn list_paginates() {
        let (_d, f, _s) = setup();
        for i in 0..5 {
            f.upload(&format!("f{i}"), "x", format!("{i}").as_bytes())
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let p1 = f.list(None, None, 2).unwrap();
        assert_eq!(p1.items.len(), 2);
        assert!(p1.next.is_some());
        let p2 = f.list(None, p1.next.as_deref(), 10).unwrap();
        assert_eq!(p2.items.len(), 3);
        assert!(p2.next.is_none());
    }

    #[test]
    fn list_filters_by_name_prefix() {
        let (_d, f, _s) = setup();
        f.upload("img-1", "x", &b"1"[..]).unwrap();
        f.upload("img-2", "x", &b"2"[..]).unwrap();
        f.upload("doc-1", "x", &b"3"[..]).unwrap();
        let p = f.list(Some("img-"), None, 10).unwrap();
        assert_eq!(p.items.len(), 2);
        assert!(p.items.iter().all(|m| m.name.starts_with("img-")));
    }

    #[test]
    fn invalid_name_rejected() {
        let (_d, f, _s) = setup();
        assert!(matches!(
            f.upload("../etc/passwd", "x", &b""[..]),
            Err(FilesError::InvalidName(_))
        ));
        assert!(matches!(
            f.upload("", "x", &b""[..]),
            Err(FilesError::InvalidName(_))
        ));
        assert!(matches!(
            f.upload("a/b", "x", &b""[..]),
            Err(FilesError::InvalidName(_))
        ));
    }

    #[test]
    fn sign_verify_and_expire() {
        let d = TempDir::new().unwrap();
        let host = Arc::new(HostDb::open(&d.path().join("host.db")).unwrap());
        let s = Signer::new(Arc::clone(&host));
        let t = s.sign("a", "f1", 60, 3600).unwrap();
        assert!(s.verify(&t, "a", "f1").is_ok());
        assert!(matches!(
            s.verify(&t, "b", "f1"),
            Err(FilesError::TokenInvalid)
        ));
        assert!(matches!(
            s.verify(&t, "a", "f2"),
            Err(FilesError::TokenInvalid)
        ));
        // 过期
        let t2 = s.sign("a", "f1", 1, 3600).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(matches!(
            s.verify(&t2, "a", "f1"),
            Err(FilesError::TokenInvalid)
        ));
    }

    #[test]
    fn sign_ttl_capped() {
        let d = TempDir::new().unwrap();
        let s = Signer::new(Arc::new(HostDb::open(&d.path().join("host.db")).unwrap()));
        assert!(s.sign("a", "f1", 999_999, 60).is_err());
        assert!(s.sign("a", "f1", 0, 60).is_err());
    }

    #[test]
    fn signer_gc_removes_expired() {
        let d = TempDir::new().unwrap();
        let s = Signer::new(Arc::new(HostDb::open(&d.path().join("host.db")).unwrap()));
        let t = s.sign("a", "f1", 1, 3600).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(s.gc().unwrap(), 1);
        assert!(matches!(
            s.verify(&t, "a", "f1"),
            Err(FilesError::TokenInvalid)
        ));
    }

    #[test]
    fn token_is_random_and_url_safe() {
        let d = TempDir::new().unwrap();
        let s = Signer::new(Arc::new(HostDb::open(&d.path().join("host.db")).unwrap()));
        let a = s.sign("a", "f1", 60, 3600).unwrap();
        let b = s.sign("a", "f2", 60, 3600).unwrap();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn manager_isolates_apps() {
        let d = TempDir::new().unwrap();
        let m = FilesManager::new(d.path().join("apps_data"), d.path().join("tmp"));
        let l = Limits::default();
        m.with("a", &l, |f| f.upload("shared-name", "x", &b"only-a"[..]))
            .unwrap();
        let missing = m.with("b", &l, |f| f.meta("whatever")).unwrap();
        assert!(missing.is_none());
        // b 列表为空
        let page = m.with("b", &l, |f| f.list(None, None, 10)).unwrap();
        assert!(page.items.is_empty());
    }

    #[test]
    fn manager_rejects_invalid_id() {
        let d = TempDir::new().unwrap();
        let m = FilesManager::new(d.path().join("apps_data"), d.path().join("tmp"));
        assert!(m.with("../evil", &Limits::default(), |_| Ok(())).is_err());
    }

    #[test]
    fn clear_removes_all() {
        let (_d, f, _s) = setup();
        f.upload("a", "x", &b"1"[..]).unwrap();
        f.upload("b", "x", &b"2"[..]).unwrap();
        f.clear().unwrap();
        let (bytes, count) = f.usage().unwrap();
        assert_eq!((bytes, count), (0, 0));
    }
}
