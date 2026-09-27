# Pegboard 根 Cargo.toml（版本冻结）

```toml
[workspace]
resolver = "2"
members = [
    "crates/pegboard-core",
    "crates/pegboard-server",
    "crates/pegboard-cli",
    "crates/pegboard-sdk",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.78"
license = "MIT OR Apache-2.0"
repository = ""

# 全 workspace 统一版本。子 crate 通过 `workspace = true` 引用，禁止单独指定。
[workspace.dependencies]
# ---- 异步运行时 ----
tokio = { version = "1.52.1", default-features = false }
tokio-util = { version = "0.7.18", default-features = false }

# ---- HTTP 服务 ----
axum = { version = "0.8.9", default-features = false }
tower = { version = "0.5.3", default-features = false }
tower-http = { version = "0.7.1", default-features = false }
http = "1.3.1"
http-body = "1.0.1"
bytes = "1.10.1"

# ---- HTTP 客户端（代理转发） ----
# reqwest 0.13 将 rustls-tls 重命名为 rustls，注意特性名
reqwest = { version = "0.13.4", default-features = false }

# ---- WebSocket ----
tokio-tungstenite = { version = "0.28.0", default-features = false }

# ---- 存储 ----
rusqlite = { version = "0.40.2", default-features = false }

# ---- 序列化 ----
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
toml = "0.8"

# ---- 错误处理 ----
thiserror = "2.0.20"

# ---- URL / 编码 ----
url = "2.5.8"
percent-encoding = "2.3.2"
mime_guess = "2.0.5"

# ---- 加密 / 哈希 ----
sha2 = "0.11.0"
hex = "0.4.3"

# ---- ID ----
ulid = "1.1.3"

# ---- CLI ----
# help/usage/error-context/suggestions：关闭 default-features 后需显式开启，
# 否则 -h/--help 不可用且错误信息退化（如 "unexpected argument found"）
clap = { version = "4.6.1", default-features = false, features = ["std", "help", "usage", "error-context", "suggestions"] }

# ---- 日志 ----
tracing = "0.1.44"
# registry：无终端模式下 stdout 层 + 文件层双挂
tracing-subscriber = { version = "0.3.23", default-features = false, features = ["registry"] }

# ---- 异步流 ----
futures-core = "0.3.32"
futures-util = { version = "0.3.32", default-features = false }

# ---- 测试 ----
tempfile = "3.20.0"

# ---- 压缩包（应用包安装 + macOS 升级整包解压） ----
zip = { version = "4.6.1", default-features = false, features = ["deflate"] }

# ---- 系统托盘（进程编排，仅 cli）----
# tray-icon + tao 为 Tauri 生态标准组合：tao 事件循环必须跑主线程（macOS
# NSApplication 约束），tokio 服务跑子线程。Linux 由 crate 默认特性带 gtk。
tray-icon = "0.24"
tao = "0.35"

# ---- 无终端模式的文件日志（仅 cli）----
tracing-appender = "0.2"

# ---- 在线升级（放 server：管理 API 需触达）----
semver = "1.0"
self-replace = "1.2"
minisign-verify = "0.2"
# minisign（完整实现：生成密钥/签名）只在单测与 examples 编译，不进 release 依赖树
minisign = "0.9"

# ---- Windows 控制台重接（无终端双击启动，仅 windows 目标编译）----
[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = [
    "Win32_Foundation",
    "Win32_Storage_FileSystem",
    "Win32_System_Console",
] }
```

## 各 crate 的 `Cargo.toml`

### `crates/pegboard-core/Cargo.toml`

```toml
[package]
name = "pegboard-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
tokio = { workspace = true, features = ["net", "time"] }
reqwest = { workspace = true, features = ["stream", "rustls", "json"] }
tokio-tungstenite = { workspace = true, features = ["connect", "handshake", "rustls-tls-webpki-roots"] }
rusqlite = { workspace = true, features = ["bundled"] }
serde = { workspace = true }
serde_json = { workspace = true }
toml = { workspace = true }
thiserror = { workspace = true }
url = { workspace = true }
http = { workspace = true }
bytes = { workspace = true }
sha2 = { workspace = true }
hex = { workspace = true }
ulid = { workspace = true }
futures-core = { workspace = true }
futures-util = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "process"] }
```

### `crates/pegboard-server/Cargo.toml`

```toml
[package]
name = "pegboard-server"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
pegboard-core = { path = "../pegboard-core" }
pegboard-sdk = { path = "../pegboard-sdk" }
axum = { workspace = true, features = ["macros", "ws", "multipart", "http1", "tokio", "json", "query"] }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "fs", "io-util"] }
tokio-util = { workspace = true, features = ["io"] }
tower = { workspace = true }
tower-http = { workspace = true, features = ["set-header", "trace", "limit"] }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
http = { workspace = true }
http-body = { workspace = true }
bytes = { workspace = true }
mime_guess = { workspace = true }
percent-encoding = { workspace = true }
url = { workspace = true }
futures-util = { workspace = true }
tracing = { workspace = true }
zip = { workspace = true }
# 在线升级（host 模块）：reqwest 复用代理同款 rustls 客户端栈
reqwest = { workspace = true, features = ["rustls", "stream"] }
semver = { workspace = true }
self-replace = { workspace = true }
minisign-verify = { workspace = true }
sha2 = { workspace = true }

[dev-dependencies]
tower = { workspace = true, features = ["util"] }
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "process"] }
# 单测内生成/签发测试密钥对（生产运行时只依赖 minisign-verify）
minisign = { workspace = true }
```

### `crates/pegboard-cli/Cargo.toml`

```toml
[package]
name = "pegboard-cli"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[[bin]]
name = "pegboard"
path = "src/main.rs"

[dependencies]
pegboard-core = { path = "../pegboard-core" }
pegboard-server = { path = "../pegboard-server" }
clap = { workspace = true, features = ["derive", "env"] }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "net", "signal"] }
axum = { workspace = true }
tracing = { workspace = true }
# ansi：with_ansi 在特性关闭时运行期 panic（fmt_layer.rs 断言），必须显式开启
tracing-subscriber = { workspace = true, features = ["env-filter", "fmt", "ansi"] }
tracing-appender = { workspace = true }
thiserror = { workspace = true }
# 托盘：tao 事件循环主线程 + tray-icon；托盘图标为原生 RGBA 资产（免 image 解码依赖）
tao = { workspace = true }
tray-icon = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
reqwest = { workspace = true, features = ["rustls", "json", "stream"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "process"] }
futures-util = { workspace = true }
url = { workspace = true }
tokio-tungstenite = { workspace = true, features = ["connect", "handshake", "rustls-tls-webpki-roots"] }

# examples/update_keygen.rs / update_sign.rs：升级签名工具（开发机专用，
# 不进 release 依赖树；客户端运行时只依赖 server 侧的 minisign-verify）
minisign = { workspace = true }
serde_json = { workspace = true }
sha2 = { workspace = true }
```

### `crates/pegboard-sdk/Cargo.toml`

```toml
[package]
name = "pegboard-sdk"
version.workspace = true
edition.workspace = true
rust-version.workspace = true

[dependencies]
# 零依赖

[dev-dependencies]
# 无
```

## 版本决策说明

| 依赖 | 版本 | 关键决策 |
|---|---|---|
| `axum` | 0.8.9 | 0.8 分支当前稳定；macros / ws / multipart 特性按需开启 |
| `tokio` | 1.52.1 | 全 workspace 统一；各 crate 按需开特性，不统一用 `full` |
| `reqwest` | 0.13.4 | **0.13 重命名了 rustls 特性**：`rustls-tls` → `rustls`；`rustls-tls-native-roots` → `rustls-native-certs` |
| `rusqlite` | 0.40.2 | `bundled` 特性编译内置 SQLite，避免系统依赖，单二进制分发必需 |
| `tokio-tungstenite` | 0.28.0 | WS 代理；需同时开 `rustls` 与 `tokio-rustls` |
| `tower-http` | 0.7.1 | `ServeDir` 的 `ignore_multi_range_requests()` 与我们的 Range 策略一致 |
| `serde` | 1.0.229 | 全 workspace 开 `derive` |
| `thiserror` | 2.0.20 | 2.x 分支；core 错误用 `thiserror`，server 映射为 `ApiError` |
| `ulid` | 1.1.3 | 文件 ID，单调可排序 |
| `sha2` | 0.11.0 | 内容哈希去重 |
| `clap` | 4.6.1 | CLI；`derive` + `env` |
| `tracing-subscriber` | 0.3.23 | `env-filter` + `fmt` |
| `tray-icon` / `tao` | 0.24 / 0.35 | Tauri 生态托盘标准组合；事件循环主线程，服务子线程 |
| `tracing-appender` | 0.2 | 无终端模式文件日志（non-blocking 写线程，避免 async 上下文阻塞 IO） |
| `semver` | 1.0 | 升级版本比较（容忍 `v` 前缀；预发布按语义序） |
| `self-replace` | 1.2 | 替换运行中的自身二进制（Windows rename dance / Unix inode 语义） |
| `minisign-verify` | 0.2 | 升级验签（Ed25519，verify-only；与 Tauri updater 同款） |
| `minisign` | 0.9 | 仅 dev/test：密钥生成与签名工具 |

## 同步 + 异步的依赖边界

core 内部**混合同步与异步**：

- `store` / `files` 是同步 API（`rusqlite` 不异步），server 侧用 `spawn_blocking` 包装。
- `proxy` 是异步 API（`reqwest` / `tokio-tungstenite`）。

因此 core 的 `tokio` 特性只需 `["net", "time"]`，不需要 `rt` / `macros`（测试与 server 才需要）。
