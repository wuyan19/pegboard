# AGENTS.md

本文件面向在本仓库工作的 AI 编码代理，约定权威来源、工程红线与变更流程。人类贡献者同样适用。

## 项目一句话

Pegboard 是薄宿主单二进制（Rust / axum / tokio），托管 AI 生成的 HTML 应用：宿主提供浏览器做不到的能力（KV、文件、跨域代理、身份）与强制边界（权限、限额、SSRF、审计），应用拥有全部业务语义。

## 文档权威顺序（改契约先改文档）

`docs/` 下 9 个主文档是唯一权威：需求描述、架构设计、API 契约、SDK 设计、数据模型、模块划分、项目骨架、实施文档、项目契约。`docs/模块骨架/` 仅为参考，冲突以主文档为准。行为与文档不一致 = bug；新增端点/状态/字段 = 先改设计文档再动代码，同步记入 `docs/里程碑简报.md`。

## 代码地图

| Crate | 职责 |
|---|---|
| `pegboard-core` | config / app（清单 + host.db 注册表）/ store / files / proxy / identity / guard / audit |
| `pegboard-server` | ingress（路由/上下文/错误映射）、statics（静态托管）、admin（管理 API + 页面）、host（进程控制 + 在线升级） |
| `pegboard-cli` | 单二进制 `pegboard`：cli 装配、runtime 启动、shutdown、tray / process / console（进程编排） |
| `pegboard-sdk` | 注入应用的 `sdk.js` / `sdk.d.ts`（零依赖） |

依赖方向唯一且单向：`cli → server → core`，sdk 零依赖；core 内部 audit 为叶子，guard 只依赖基础与 audit，store/files/proxy 依赖基础。反向依赖由 `scripts/check-deps.sh` 强制，新增 use 必须过它。

## 工程红线（docs/项目契约.md，违反即返工）

- 非 test 代码禁止 `unwrap` / `expect` / `panic!`；锁不跨 await；async 上下文无阻塞 IO（同步 FS/SQLite 走 `spawn_blocking`）。
- core/server 对外错误用 thiserror，映射集中在 `ingress/error.rs`；禁 anyhow 进公共接口。
- 一切用户输入路径 canonicalize + 前缀校验；上传与代理全流式，不整文件进内存。
- tracing 记日志，不打印凭据与业务数据；禁 `println!`（`eprintln!` 仅限 main 启动失败兜底）。
- 应用数据目录 `data_root/apps_data/<id>/` 与产物目录 `apps_dir/<id>/` 分离；host.db（`app::HostDb`）是已安装注册表的唯一属主。

## 构建与门禁

```shell
cargo test --workspace                              # 全量（含 Playwright 场景，需系统 Chrome）
cargo test -p pegboard-cli --test scenarios_admin   # admin 浏览器冒烟
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
./scripts/check-deps.sh                             # 依赖方向
cargo run -p pegboard-cli -- --check                # 自检
```

## 变更流程约定

- **里程碑制**：任务与验收在 `docs/实施文档.md`（M0–M8），完成后在 `docs/里程碑简报.md` 记录实现要点与验证结果。
- **提交格式**：`<模块>: <动作>`（如 `cli: launch modes system tray and host restart control`）；文档先行单独提交（`docs: ...`）。
- **依赖冻结**：版本统一冻结在 workspace 根 `Cargo.toml`，子 crate 用 `workspace = true` + 按需特性，禁止单独指定版本；任何增删改同步 `docs/模块骨架/Cargo.toml冻结.md`。
- **admin 页面**（`server/src/admin/page.html`）经 `include_bytes!` 编译进二进制，改动需重编译；e2e 依赖其中的 ID/class/文案契约（见 `crates/pegboard-cli/tests/e2e/admin_smoke.js`）。

## 平台注意事项

- 开发实机为 macOS；Linux CI（`.github/workflows/ci.yml`）需要 `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libdbus-1-dev`（tray-icon/tao 默认特性）。
- Windows 专属代码（`console.rs`、`windows_subsystem`）CI 不编译，release 矩阵覆盖；改动时保持与 qrctrl 同款模式并自查 cfg 完整性。
- 在线升级的平台 key（`macos-aarch64-app` 等）与 release.yml、`host/update.rs::platform_key()` 是三方硬约定，改名必须同步。
- 签名私钥在 `.sign/`（已 gitignore）；公钥内嵌于 `crates/pegboard-server/assets/update-pubkey.txt`，轮换流程见 `examples/update_keygen.rs` 头注释。
