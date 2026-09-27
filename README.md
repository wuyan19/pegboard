# Pegboard

薄宿主单二进制：托管 AI 生成的 HTML 应用。宿主提供浏览器不具备的能力与边界，应用拥有全部业务语义。

```
Browser ──▶ Pegboard Host (单二进制)
             ├─ 静态托管   apps/<id>/ 产物、SPA 回退、Range/缓存
             ├─ 能力层     store / files / proxy / user
             ├─ 治理层     权限 · 限额 · SSRF 防护 · 审计
             └─ 管理台     /admin 应用生命周期 · 用量 · 日志
```

## 特性

- **能力面**（SDK 注入 `window.host`）：KV 存储、Blob 文件（签名直链）、跨域代理（HTTP/WS/SSE 流式）、不透明 subject。
- **治理面**：应用清单声明权限与限额（只能收紧），SSRF 白名单逐跳校验，审计日志双向分页。
- **管理台**：应用生命周期（安装 / 卸载 / 启停 / 删除 / zip 包上传安装）、实际用量、运行日志，无需登录即可读元数据（可选 Bearer 鉴权）。
- **进程形态**：终端 CLI / macOS 托盘 .app（双击启动）/ Windows 无终端；在线升级基于 GitHub Releases，Ed25519 签名验签。
- **隔离**：应用之间数据物理隔离（独立库 / 目录），由宿主保证而非应用自律。

## 快速开始

```shell
git clone https://github.com/wuyan19/pegboard && cd pegboard
cargo run --release -p pegboard-cli
```

打开 `http://127.0.0.1:8787`（管理页在 `/admin`）。

配置发现链：`--config` / `$PEGBOARD_CONFIG` → `./config.toml` → 平台配置目录 → 全默认。数据与应用目录默认落在平台数据区（macOS `~/Library/Application Support/Pegboard`、Linux XDG、Windows `%APPDATA%`）；cwd 放置 `config.toml`（复制 `config.example.toml`）则保持开发树惯例（相对路径相对 cwd）。

常用 CLI 参数：

| 参数 | 说明 |
|---|---|
| `--config <路径>` | 配置文件（缺省查找 `$PEGBOARD_CONFIG` → `./config.toml`） |
| `--listen` / `--data-root` / `--apps-dir` | 覆盖监听地址 / 数据根 / 应用产物目录 |
| `--check` | 只校验配置与应用清单，不启动服务（退出码 0/1/2） |
| `--no-tray` | 无头服务模式（ssh / 开机自启；有 GUI 时默认尝试托盘） |

macOS 打包出 `dist/Pegboard.app`（托盘常驻、双击启动）：

```shell
./scripts/package.sh
```

## 写一个应用

`apps/<id>/` 放一个 `manifest.json` 与静态产物即可，例如：

```json
{ "id": "demo", "name": "Demo", "entry": "index.html", "permissions": { "store": true, "net": ["api.example.com"] } }
```

页面里直接用注入的 `window.host`：`host.store` / `host.files` / `host.fetch`（跨域自动走宿主代理）/ `host.connectWS` / `host.user`。完整类型见 `crates/pegboard-sdk/assets/sdk.d.ts`，可运行示例见 `apps/` 下的三个应用。

## 发布与在线升级

push `v*` tag 触发 GitHub Actions：四平台构建（macOS 双架构整包 .app.zip、Windows/Linux 裸二进制）→ 用 minisign 私钥签出 `update-manifest.json` → 附到 release。已安装的宿主在管理台「检查更新」即可升级（信封验签 + sha256 双校验，macOS 整包替换 .app，Windows/Linux 原子替换二进制）。签名密钥与 secrets 配置见 `.github/workflows/release.yml` 头部说明。

## 文档

设计文档在 `docs/`：[需求描述](docs/需求描述.md) · [架构设计](docs/架构设计.md) · [API 契约](docs/API契约.md) · [SDK 设计](docs/SDK设计.md) · [数据模型](docs/数据模型.md) · [模块划分](docs/模块划分.md) · [项目骨架](docs/项目骨架.md) · [实施文档](docs/实施文档.md) · [项目契约](docs/项目契约.md)。

## License

MIT OR Apache-2.0
