# pegboard-core / audit 模块

## 接口

```rust
// crates/pegboard-core/src/audit/mod.rs

use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// 动作分类。比 guard::Action 宽，涵盖非能力路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Store,
    Files,
    Net,
    Ws,
    Static,
    Admin,
    Identity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Denied,
    Error,
}

/// 单条审计事件。字段克制：不含业务数据、不含凭据、不含文件内容。
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub ts: i64,                       // Unix 毫秒
    pub app_id: Option<String>,        // 静态路径也可能无 app
    pub subject: Option<String>,       // 不透明，可为空
    pub action: ActionKind,
    pub target: Option<String>,        // 域名 / key 前缀 / file id；不含完整 URL 查询串
    pub outcome: Outcome,
    pub error_code: Option<String>,    // 与契约错误码对齐
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
    pub limit: Option<u32>,            // 默认 200，上限 1000
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("channel closed")]
    ChannelClosed,
    #[error("serialize: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    pub log_dir: PathBuf,              // <data_root>/logs/
    pub file_name: String,             // 默认 "audit.log"
    pub max_bytes: u64,                // 单文件上限，默认 64 MiB
    pub max_files: u32,                // 保留份数，默认 8
    pub queue_capacity: usize,         // 默认 4096
}

impl Default for AuditConfig { /* 合理默认 */ }

/// 审计器。record 非阻塞；后台线程串行落盘 + 滚动。
pub struct Auditor {
    tx: mpsc::SyncSender<AuditEvent>,
    config: AuditConfig,
    _writer: std::thread::JoinHandle<()>,
}

impl Auditor {
    /// 启动后台写入线程，打开/创建日志目录。
    pub fn start(config: AuditConfig) -> Result<Self, AuditError>;

    /// 非阻塞投递。队列满时丢弃并计数（不阻塞请求路径）。
    pub fn record(&self, event: AuditEvent) -> Result<(), AuditError>;

    /// 同步优雅关闭：等待队列排空，join 写入线程。
    pub fn shutdown(self) -> Result<(), AuditError>;

    /// 查询：从滚动日志读取并过滤，按时间倒序返回。
    /// 只读最近 max_files 份，不做全量扫描。
    pub fn query(&self, filter: &Filter) -> Result<Vec<AuditEvent>, AuditError>;

    /// 队列中被丢弃的事件总数，用于自监控。
    pub fn dropped(&self) -> u64;
}

/// 事件序列化为单行 JSON。字段顺序固定，便于人工阅读。
fn encode(e: &AuditEvent) -> Result<String, AuditError>;

/// 单行解析；失败返回 None（跳过坏行，不中断查询）。
fn decode(line: &str) -> Option<AuditEvent>;

/// 滚动：若当前文件超 max_bytes，重命名并清理超份数。
fn rotate(dir: &Path, name: &str, max_bytes: u64, max_files: u32) -> Result<(), AuditError>;
```

## 落盘

```text
<data_root>/logs/
  audit.log              # 当前
  audit.log.1            # 上一份
  audit.log.2
  ...
```

- 追加写，单行一条 JSON。
- 超 `max_bytes` 时关闭当前、重命名、开新文件；超过 `max_files` 的最旧份删除。
- 写入线程独占文件句柄，无需锁。

## 事件示例

```json
{"ts":1730000000123,"app_id":"ollama-chat","subject":"alice","action":"Net","target":"127.0.0.1:11434","outcome":"Ok","error_code":null,"duration_ms":842}
{"ts":1730000000456,"app_id":"glm-quota","subject":"bob","action":"Net","target":"open.bigmodel.cn","outcome":"Denied","error_code":"TARGET_DENIED","duration_ms":0}
```

## 行为规则

- **target 脱敏**：Net/Ws 只记 host[:port]，不记完整 URL、不记查询串；Store 只记 key 前缀（前 64 字节）；Files 只记 file_id。
- **subject 可为空**：匿名部署下为 `null`。
- **无业务数据**：不记录请求体、响应体、cookie、authorization。
- **失败即降级**：队列满丢弃最旧事件并计数，不阻塞调用方；`record` 不返回错误影响主路径。
- **单行上限**：单条编码后超 4 KiB 截断 `target`，保证一行可读。
- **查询**：默认按时间倒序；`limit` 默认 200，上限 1000；超出即截断。
- **滚动**：仅写入线程触发；查询不受影响。
- **关闭**：`shutdown` 消费 self，保证 drop 前排空。

## 单元测试骨架

```rust
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
        let f = Filter { app_id: Some("a".into()), ..Default::default() };
        assert_eq!(a.query(&f).unwrap().len(), 2);
        let f = Filter { action: Some(ActionKind::Net), ..Default::default() };
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
        let f = Filter { outcome: Some(Outcome::Denied), ..Default::default() };
        assert_eq!(a.query(&f).unwrap().len(), 1);
    }

    #[test]
    fn query_returns_desc_by_time() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        let mut e1 = ev("a", ActionKind::Net, Outcome::Ok); e1.ts = 1;
        let mut e2 = ev("a", ActionKind::Net, Outcome::Ok); e2.ts = 2;
        a.record(e1).unwrap();
        a.record(e2).unwrap();
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let items = a.query(&Filter::default()).unwrap();
        assert!(items[0].ts >= items[1].ts);
    }

    #[test]
    fn limit_applied() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        for i in 0..10 {
            let mut e = ev("a", ActionKind::Net, Outcome::Ok); e.ts = i;
            a.record(e).unwrap();
        }
        a.shutdown().unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let f = Filter { limit: Some(3), ..Default::default() };
        assert_eq!(a.query(&f).unwrap().len(), 3);
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

        let files: Vec<_> = std::fs::read_dir(d.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("audit.log"))
            .collect();
        assert!(files.len() <= 3);
    }

    #[test]
    fn bad_lines_skipped() {
        let d = TempDir::new().unwrap();
        let a = Auditor::start(cfg(&d)).unwrap();
        a.record(ev("a", ActionKind::Net, Outcome::Ok)).unwrap();
        a.shutdown().unwrap();

        // 手工追加一行垃圾
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true)
            .open(d.path().join("audit.log")).unwrap();
        writeln!(f, "not json").unwrap();

        let a = Auditor::start(cfg(&d)).unwrap();
        let items = a.query(&Filter::default()).unwrap();
        assert_eq!(items.len(), 1);   // 垃圾行被跳过
    }

    #[test]
    fn queue_full_drops_without_blocking() {
        let d = TempDir::new().unwrap();
        let mut c = cfg(&d);
        c.queue_capacity = 4;
        let a = Auditor::start(c).unwrap();
        for _ in 0..1000 {
            // 不应 panic、不应阻塞
            let _ = a.record(ev("a", ActionKind::Net, Outcome::Ok));
        }
        a.shutdown().unwrap();
        // 至少记录了部分
        let a = Auditor::start(cfg(&d)).unwrap();
        assert!(a.query(&Filter::default()).unwrap().len() > 0);
    }

    #[test]
    fn target_truncated() {
        let mut e = ev("a", ActionKind::Net, Outcome::Ok);
        e.target = Some("x".repeat(10_000));
        let s = encode(&e).unwrap();
        assert!(s.len() <= 4096);
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
```

## 依赖

```toml
[dependencies]
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

无网络、无异步依赖，纯 std + serde。

## 设计要点

- **非阻塞**：`record` 只投递到有界通道；队列满丢弃并计数，绝不阻塞请求路径。审计的可用性服从主路径。
- **单线程写**：一个后台线程独占文件，避免锁与并发滚动。
- **滚动简单**：按大小切分，保留固定份数；不按天、不压缩。内部工具够用。
- **查询朴素**：只读最近 `max_files` 份，逐行解析，跳过坏行；不做索引、不做全量扫描。
- **脱敏优先**：target 只记 host / key 前缀 / file id；禁止业务数据、凭据。
- **与契约对齐**：`error_code` 用契约错误码字符串，便于日志与响应互相印证。
- **关闭显式**：`shutdown` 消费 self，确保落盘；进程退出路径必须调用。
- **不依赖 audit 之外**：core 其余模块只调用 `record`，不读 `query`。`query` 供 admin 使用。
