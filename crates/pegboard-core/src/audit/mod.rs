//! 审计：结构化事件落盘与查询。独立于 tracing，按行 JSON 追加写。
//!
//! record 非阻塞（有界通道，满则丢弃计数）；后台线程独占文件句柄串行写入。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 动作分类。审计只覆盖能力调用；静态访问走 tracing 不入审计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    Store,
    Files,
    Net,
    Ws,
    Static,
    Admin,
    Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Ok,
    Denied,
    Error,
}

/// 单条审计事件。字段克制：不含业务数据、凭据、文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unix 毫秒
    pub ts: i64,
    pub app_id: Option<String>,
    /// 不透明 subject，可为空
    pub subject: Option<String>,
    pub action: ActionKind,
    /// 域名 / key 前缀 / file id；不含完整 URL 查询串
    pub target: Option<String>,
    pub outcome: Outcome,
    /// 与契约错误码对齐
    pub error_code: Option<String>,
    pub duration_ms: u64,
}

/// 查询过滤条件。全部可选，AND 语义。
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub app_id: Option<String>,
    pub subject: Option<String>,
    pub action: Option<ActionKind>,
    pub outcome: Option<Outcome>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// 默认 200，上限 1000
    pub limit: Option<u32>,
}

impl Filter {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(200).min(1000) as usize
    }

    fn matches(&self, e: &AuditEvent) -> bool {
        if let Some(app) = &self.app_id {
            if e.app_id.as_deref() != Some(app.as_str()) {
                return false;
            }
        }
        if let Some(sub) = &self.subject {
            if e.subject.as_deref() != Some(sub.as_str()) {
                return false;
            }
        }
        if self.action.is_some_and(|a| e.action != a) {
            return false;
        }
        if self.outcome.is_some_and(|o| e.outcome != o) {
            return false;
        }
        if self.since.is_some_and(|s| e.ts < s) {
            return false;
        }
        if self.until.is_some_and(|u| e.ts > u) {
            return false;
        }
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("channel closed")]
    ChannelClosed,
    #[error("serialize: {0}")]
    Serialize(String),
    #[error("writer thread panicked")]
    WriterPanic,
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// &lt;data_root&gt;/logs/
    pub log_dir: PathBuf,
    pub file_name: String,
    /// 单文件上限，默认 64 MiB
    pub max_bytes: u64,
    /// 保留份数（含当前文件），默认 8
    pub max_files: u32,
    /// 有界队列容量，默认 4096
    pub queue_capacity: usize,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            log_dir: PathBuf::from("./data/logs"),
            file_name: "audit.log".to_owned(),
            max_bytes: 64 * 1024 * 1024,
            max_files: 8,
            queue_capacity: 4096,
        }
    }
}

/// 审计共享核心：发送端 + 丢弃计数 + 查询配置。
/// Auditor（拥有写线程）与 AuditorHandle（轻量克隆）共用。
/// tx 锁仅短持有（取克隆），不跨 await。
struct AuditorShared {
    tx: std::sync::Mutex<Option<mpsc::SyncSender<AuditEvent>>>,
    dropped: AtomicU64,
    config: AuditConfig,
}

impl AuditorShared {
    fn sender(&self) -> Option<mpsc::SyncSender<AuditEvent>> {
        self.tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 非阻塞投递。队列满时丢弃本条并计数，不阻塞请求路径。
    fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        let Some(tx) = self.sender() else {
            return Err(AuditError::ChannelClosed);
        };
        match tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(dropped_total = n, "audit queue full, event dropped");
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(AuditError::ChannelClosed),
        }
    }
}

/// 审计器轻量句柄：可克隆，供 AppState / 各 handler 共享；支持查询。
#[derive(Clone)]
pub struct AuditorHandle {
    shared: Arc<AuditorShared>,
}

impl AuditorHandle {
    /// 非阻塞投递（见 AuditorShared::record）。
    pub fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.shared.record(event)
    }

    /// 队列中被丢弃的事件总数。
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// 查询：从滚动日志读取并过滤，按时间倒序（与 Auditor::query 同语义）。
    pub fn query(&self, filter: &Filter) -> Result<Vec<AuditEvent>, AuditError> {
        query_files(&self.shared.config, filter)
    }
}

/// 审计器。record 非阻塞；后台线程串行落盘 + 滚动。
pub struct Auditor {
    shared: Arc<AuditorShared>,
    writer: Option<std::thread::JoinHandle<()>>,
    config: AuditConfig,
}

impl Auditor {
    /// 启动后台写入线程，打开/创建日志目录。
    pub fn start(config: AuditConfig) -> Result<Self, AuditError> {
        std::fs::create_dir_all(&config.log_dir)?;
        let (tx, rx) = mpsc::sync_channel::<AuditEvent>(config.queue_capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_config = config.clone();
        let writer = std::thread::Builder::new()
            .name("pegboard-audit".to_owned())
            .spawn(move || writer_loop(rx, &writer_config))?;
        let _ = dropped; // 计数并入共享核心
        Ok(Self {
            shared: Arc::new(AuditorShared {
                tx: std::sync::Mutex::new(Some(tx)),
                dropped: AtomicU64::new(0),
                config: config.clone(),
            }),
            writer: Some(writer),
            config,
        })
    }

    /// 轻量句柄：与本体共享发送端与计数器。
    pub fn handle(&self) -> AuditorHandle {
        AuditorHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// 非阻塞投递（等价 handle().record）。
    pub fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.shared.record(event)
    }

    /// 同步优雅关闭：drop 发送端 → 写线程排空退出 → join。
    pub fn shutdown(mut self) -> Result<(), AuditError> {
        if let Ok(mut guard) = self.shared.tx.lock() {
            *guard = None;
        }
        if let Some(writer) = self.writer.take() {
            writer.join().map_err(|_| AuditError::WriterPanic)?;
        }
        Ok(())
    }

    /// 查询：从滚动日志读取并过滤，按时间倒序返回。
    /// 只读最近 max_files 份，不做全量扫描。
    pub fn query(&self, filter: &Filter) -> Result<Vec<AuditEvent>, AuditError> {
        query_files(&self.config, filter)
    }

    /// 队列中被丢弃的事件总数，用于自监控。
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

fn writer_loop(rx: mpsc::Receiver<AuditEvent>, config: &AuditConfig) {
    let mut file: Option<File> = None;
    for event in rx {
        let outcome = write_event(config, &mut file, &event);
        if let Err(err) = outcome {
            tracing::error!(error = %err, "audit write failed");
        }
    }
}

fn write_event(
    config: &AuditConfig,
    file: &mut Option<File>,
    event: &AuditEvent,
) -> Result<(), AuditError> {
    let line = encode(event)?;
    if file.is_none() {
        *file = Some(open_append(&config.log_dir.join(&config.file_name))?);
    }
    if let Some(handle) = file.as_mut() {
        handle.write_all(line.as_bytes())?;
        handle.write_all(b"\n")?;
        handle.flush()?;
        if handle.metadata()?.len() >= config.max_bytes {
            rotate(&config.log_dir, &config.file_name, config.max_files)?;
            *file = None;
        }
    }
    Ok(())
}

fn open_append(path: &Path) -> Result<File, AuditError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

/// 滚动：audit.log → audit.log.1 → …，总共保留 max_files 份（含当前）。
fn rotate(dir: &Path, name: &str, max_files: u32) -> Result<(), AuditError> {
    let keep = max_files.max(1);
    let rotated = keep.saturating_sub(1);
    let current = dir.join(name);
    if rotated == 0 {
        // 只保留当前份：滚动即清空最旧（当前）份
        if current.is_file() {
            std::fs::remove_file(&current)?;
        }
        return Ok(());
    }
    // 删除最旧的 rotated 份，再从旧到新逐级上移，避免覆盖
    let overflow = dir.join(format!("{name}.{rotated}"));
    if overflow.is_file() {
        std::fs::remove_file(&overflow)?;
    }
    for i in (1..rotated).rev() {
        let from = dir.join(format!("{name}.{i}"));
        if from.is_file() {
            let to = dir.join(format!("{name}.{}", i + 1));
            std::fs::rename(&from, &to)?;
        }
    }
    if current.is_file() {
        let to = dir.join(format!("{name}.1"));
        std::fs::rename(&current, &to)?;
    }
    Ok(())
}

/// 文件级查询实现：读最近 max_files 份，过滤后按 ts 降序截断。
fn query_files(config: &AuditConfig, filter: &Filter) -> Result<Vec<AuditEvent>, AuditError> {
    let limit = filter.limit();
    let mut files = vec![config.log_dir.join(&config.file_name)];
    for i in 1..config.max_files.max(1) {
        files.push(config.log_dir.join(format!("{}.{}", config.file_name, i)));
    }
    files.retain(|p| p.is_file());
    let mut out = Vec::new();
    for path in files {
        let file = File::open(&path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if let Some(event) = decode(&line) {
                if filter.matches(&event) {
                    out.push(event);
                }
            }
        }
    }
    // 文件序列新 → 旧，文件内时间升序；整体按 ts 降序（稳定排序保留新文件优先）
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out.truncate(limit);
    Ok(out)
}

/// 单条编码上限 4 KiB：超长截断 target。
const MAX_LINE_BYTES: usize = 4096;

fn encode(e: &AuditEvent) -> Result<String, AuditError> {
    let probe = serde_json::to_string(e).map_err(|err| AuditError::Serialize(err.to_string()))?;
    if probe.len() <= MAX_LINE_BYTES {
        return Ok(probe);
    }
    let target_len = e.target.as_ref().map_or(0, |t| t.len());
    let overhead = probe.len() - target_len;
    let budget = MAX_LINE_BYTES.saturating_sub(overhead + 16).max(64);
    let mut trimmed = e.clone();
    if let Some(t) = trimmed.target.take() {
        trimmed.target = Some(t.chars().take(budget).collect::<String>());
    }
    serde_json::to_string(&trimmed).map_err(|err| AuditError::Serialize(err.to_string()))
}

/// 单行解析；失败返回 None（跳过坏行，不中断查询）。
fn decode(line: &str) -> Option<AuditEvent> {
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg(dir: &TempDir) -> AuditConfig {
        AuditConfig {
            log_dir: dir.path().into(),
            ..Default::default()
        }
    }

    fn ev(app: &str, action: ActionKind, outcome: Outcome) -> AuditEvent {
        AuditEvent {
            ts: 0,
            app_id: Some(app.into()),
            subject: Some("u".into()),
            action,
            target: Some("x.com".into()),
            outcome,
            error_code: None,
            duration_ms: 1,
        }
    }

    #[test]
    fn records_and_queries() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        a.record(ev("app", ActionKind::Net, Outcome::Ok)).unwrap();
        a.record(ev("app", ActionKind::Store, Outcome::Ok)).unwrap();
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let items = a.query(&Filter::default()).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn filters_by_app_and_action() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        a.record(ev("a", ActionKind::Net, Outcome::Ok)).unwrap();
        a.record(ev("b", ActionKind::Net, Outcome::Ok)).unwrap();
        a.record(ev("a", ActionKind::Store, Outcome::Ok)).unwrap();
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let f = Filter {
            app_id: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(a.query(&f).unwrap().len(), 2);
        let f = Filter {
            action: Some(ActionKind::Net),
            ..Default::default()
        };
        assert_eq!(a.query(&f).unwrap().len(), 2);
    }

    #[test]
    fn filters_by_outcome() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        a.record(ev("a", ActionKind::Net, Outcome::Ok)).unwrap();
        a.record(ev("a", ActionKind::Net, Outcome::Denied)).unwrap();
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let f = Filter {
            outcome: Some(Outcome::Denied),
            ..Default::default()
        };
        assert_eq!(a.query(&f).unwrap().len(), 1);
    }

    #[test]
    fn query_returns_desc_by_time() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        let mut e1 = ev("a", ActionKind::Net, Outcome::Ok);
        e1.ts = 1;
        let mut e2 = ev("a", ActionKind::Net, Outcome::Ok);
        e2.ts = 2;
        a.record(e1).unwrap();
        a.record(e2).unwrap();
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let items = a.query(&Filter::default()).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].ts >= items[1].ts);
    }

    #[test]
    fn limit_applied() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        for i in 0..10u64 {
            let mut e = ev("a", ActionKind::Net, Outcome::Ok);
            e.ts = i as i64;
            a.record(e).unwrap();
        }
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let f = Filter {
            limit: Some(3),
            ..Default::default()
        };
        assert_eq!(a.query(&f).unwrap().len(), 3);
    }

    #[test]
    fn since_until_filter() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        for i in 0..10u64 {
            let mut e = ev("a", ActionKind::Net, Outcome::Ok);
            e.ts = i as i64;
            a.record(e).unwrap();
        }
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let f = Filter {
            since: Some(3),
            until: Some(6),
            ..Default::default()
        };
        let items = a.query(&f).unwrap();
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|e| (3..=6).contains(&e.ts)));
    }

    #[test]
    fn rotation_limits_files() {
        let d = TempDir::new().unwrap();
        let mut c = cfg(&d);
        c.max_bytes = 256;
        c.max_files = 3;
        let a = Auditor::start(c).unwrap();
        for _ in 0..200 {
            a.record(ev("a", ActionKind::Net, Outcome::Ok)).unwrap();
        }
        a.shutdown().unwrap();

        let files: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("audit.log"))
            .collect();
        assert!(files.len() <= 3, "files: {files:?}");
    }

    #[test]
    fn bad_lines_skipped() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        a.record(ev("a", ActionKind::Net, Outcome::Ok)).unwrap();
        a.shutdown().unwrap();

        use std::io::Write;
        let mut f = OpenOptions::new()
            .append(true)
            .open(d.path().join("audit.log"))
            .unwrap();
        writeln!(f, "not json").unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let items = a.query(&Filter::default()).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn queue_full_drops_without_blocking() {
        let d = TempDir::new().unwrap();
        let mut c = cfg(&d);
        c.queue_capacity = 4;
        let a = Auditor::start(c).unwrap();
        for _ in 0..1000 {
            let _ = a.record(ev("a", ActionKind::Net, Outcome::Ok));
        }
        assert!(a.dropped() > 0);
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        assert!(!a.query(&Filter::default()).unwrap().is_empty());
    }

    #[test]
    fn target_truncated() {
        let mut e = ev("a", ActionKind::Net, Outcome::Ok);
        e.target = Some("x".repeat(10_000));
        let s = encode(&e).unwrap();
        assert!(s.len() <= MAX_LINE_BYTES);
    }

    #[test]
    fn roundtrip_encode_decode() {
        let e = ev("a", ActionKind::Net, Outcome::Denied);
        let s = encode(&e).unwrap();
        let d = decode(&s).unwrap();
        assert_eq!(d.app_id, e.app_id);
        assert_eq!(d.action, e.action);
        assert_eq!(d.outcome, e.outcome);
    }
}
