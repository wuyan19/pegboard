# Pegboard API 契约

## 1. 通用约定

- 基址：同源。应用静态资源在 `/apps/:app_id/*`，能力 API 在 `/api/*`。
- 应用身份由接入层从路径或请求头解析，请求体不可自称 app。
- 请求与响应默认 JSON；文件上传用 multipart；下载按原始类型。
- 错误统一：

```json
{ "error": { "code": "STRING", "message": "STRING", "detail": {} } }
```

- 时间戳为 Unix 毫秒。
- 所有能力 API 需通过 guard，错误码见 §6。

## 2. 应用清单

路径：`apps/<app_id>/manifest.json`

```json
{
  "id": "ollama-chat",
  "name": "Ollama Chat",
  "entry": "dist/index.html",
  "permissions": {
    "store": true,
    "files": true,
    "net": ["http://127.0.0.1:11434"],
    "ws": [],
    "shim": true
  },
  "limits": {
    "kv_value_bytes": 262144,
    "kv_total_bytes": 10485760,
    "file_bytes": 104857600,
    "file_total_bytes": 1073741824,
    "net_rps": 20
  }
}
```

字段：
- `entry`：入口文件，相对应用根目录。
- `permissions.net` / `ws`：目标 origin 或完整 URL 白名单；为空表示禁止。
- `permissions.shim`：是否启用全局 fetch 兜底，默认 `true`。
- `limits`：键名与宿主配置一致（见数据模型 §8），缺省用宿主默认值，只能收紧；`sign_ttl_max` 不可覆盖。

## 3. 静态托管

```
GET /apps/:app_id/*
```

- 命中文件则直出，正确 MIME。
- 未命中且非文件路径时回退到 `entry`（SPA）。
- 支持 Range、ETag、Cache-Control。
- 对 text/html 响应注入 `/sdk.js`（见 §7）。
- 不经过 guard，不携带 subject。

## 4. Store API

### 4.1 KV

```
GET    /api/store/kv/:key
PUT    /api/store/kv/:key        body: { "value": <json> }
DELETE /api/store/kv/:key
GET    /api/store/kv?prefix=     -> { "items": [{ "key", "value" }] }
POST   /api/store/kv/batch       body: { "ops": [{ "op", "key", "value" }] }
```

- `key`：URL 编码字符串，长度上限由限额控制。
- `value`：任意 JSON。
- `op`：`set` | `delete`。
- 命名空间由应用自行约定，宿主不解释。
- `key` 需 URL 编码后置于路径；含 `/` 时编码为 `%2F`，服务端不得将其解码为路径分隔符。
- `list` 一次返回全部命中项，不做分页；规模受 `kv_total_bytes` 约束。

### 4.2 文件

```
POST   /api/files                multipart: file
                                 -> { "id", "name", "size", "mime" }
GET    /api/files/:id            支持 Range / ETag / Content-Disposition
GET    /api/files/:id?token=     签名访问，免 subject
DELETE /api/files/:id
GET    /api/files?prefix=&cursor=&limit=
                                 -> { "items": [...], "next": "cursor|null" }
POST   /api/files/:id/sign       body: { "ttl": 600 }
                                 -> { "url": "/api/files/:id?token=..." }
```

- 上传流式落盘，超限即断。
- 签名 token 只回答“有效吗”，不携带身份语义。
- 签名 URL 过期即失效。

## 5. Proxy API

```
ANY /api/proxy?url=<encoded>     HTTP 代理，透传 method/body 与应用请求头
GET /api/ws-proxy?url=<encoded>&app=<app_id>  WebSocket 升级
GET /api/asset?url=<encoded>     资源代理，用于 img/font/css
```

- `ws-proxy` 的 `app` 参数：浏览器 WebSocket API 无法携带自定义请求头，应用身份经查询参数传入（SDK 注入，与 `X-Pegboard-App` 头同信任级）。

规则：
- `url` 必须命中 `manifest.permissions.net`（asset 同）。
- 每跳重定向重新校验白名单。
- 默认拒绝内网、本机、云元数据地址；本机仅按 app + 端口显式放行。
- 请求头处理（对齐原生 fetch：默认不携带环境 cookie）：

  | 处理 | 头 |
  |---|---|
  | 出站剥离 | `Cookie`、`Host`、`X-Forwarded-*` |
  | 照常透传 | 其余全部，含应用显式设置的 `Authorization`；`Origin` / `Referer` 为应用页面真实来源，保留 |

  proxy、asset、ws-proxy 升级握手共用本表。
- 响应头清理：移除 `Set-Cookie`、`Access-Control-*`、`Content-Security-Policy`；保留 `Content-Type`、`Content-Length`、`ETag`。
- 流式响应（SSE、chunked）透传，不缓冲。
- 二进制安全。

## 6. 错误码

| code | HTTP | 含义 |
|---|---|---|
| `APP_NOT_FOUND` | 404 | 应用不存在 |
| `PERMISSION_DENIED` | 403 | 未声明该权限 |
| `TARGET_DENIED` | 403 | 目标不在白名单或命中 SSRF 规则 |
| `LIMIT_EXCEEDED` | 413 / 429 | 超出限额（字节类 413，频率类 429） |
| `NOT_FOUND` | 404 | 资源不存在 |
| `INVALID_REQUEST` | 400 | 参数或请求体非法 |
| `TOKEN_INVALID` | 401 | 签名 token 无效或过期 |
| `UPSTREAM_ERROR` | 502 | 目标服务错误 |
| `TIMEOUT` | 504 | 超时 |

- v1 身份为固定值模式，不产生 401；401 仅在启用 token 身份来源且凭证缺失或无效时出现。

## 7. 注入前端 SDK

宿主在应用页面最早注入 `/sdk.js`，暴露 `window.host`：

```ts
interface Host {
  store: {
    get(key: string): Promise<unknown | null>;
    set(key: string, value: unknown): Promise<void>;
    delete(key: string): Promise<void>;
    list(prefix?: string): Promise<{ key: string; value: unknown }[]>;
    batch(ops: { op: "set" | "delete"; key: string; value?: unknown }[]): Promise<void>;
  };
  files: {
    upload(file: Blob | File): Promise<{ id: string; name: string; size: number; mime: string }>;
    get(id: string): Promise<Blob | null>;
    url(id: string): string;
    sign(id: string, ttlSec: number): Promise<string>;
    list(opts?: { prefix?: string; cursor?: string; limit?: number }): Promise<{
      items: { id: string; name: string; size: number; mime: string; created: number }[];
      next: string | null;
    }>;
    delete(id: string): Promise<void>;
  };
  fetch(url: string, options?: RequestInit): Promise<Response>;
  connectWS(url: string): WebSocket;
  url(url: string): string;
  user?: { id: string | null };
}
```

**fetch 兜底**：当 `permissions.shim` 为真，宿主包装 `window.fetch`：
- 同源或相对路径 → 原生 fetch。
- 跨域绝对 URL → 转 `/api/proxy`。
- 显式 `host.fetch` 不再二次包装。

## 8. 管理 API（宿主元数据）

```
GET  /api/admin/status
GET  /api/admin/apps
GET  /api/admin/apps/:id
POST /api/admin/apps/:id/install     触发扫描并注册（清单校验通过）
POST /api/admin/apps/:id/uninstall   注销应用，删除数据目录与签名 token，产物保留
POST /api/admin/apps/:id/enable
POST /api/admin/apps/:id/disable
GET  /api/admin/logs?app=&action=&outcome=&since=&until=&limit=&cursor=
POST /api/admin/host/restart         重启宿主进程（升级落位后 / 配置生效）
GET  /api/admin/update/status        升级状态机：current + phase
POST /api/admin/update/check         检查更新（仅手动触发）
POST /api/admin/update/install       下载 + 校验 + 安装（仅 Available 状态可触发）
```

- 只操作宿主元数据，不读写应用业务数据。
- `status` 返回宿主自身状态：version、exposure（lan = listen 非环回 / local = 仅本机，由 listen 派生）、listen、data_root、apps_dir、uptime_secs、audit_dropped。
- 应用视图含 `state`（enabled | disabled）与 `enabled`；详情另附 `root`（产物目录）与 `data_dir`（数据目录）。
- `GET /api/admin/apps/:id` 详情在应用视图外附 `usage` 实际用量：`{ "kv": { "bytes", "keys" }, "files": { "bytes", "count" } }`；未使用过的应用为 0。
- `logs` 双向分页：条目含 `seq`（进程内单调序号）；`next`/`prev` 为 `"ts:seq"` 游标（更早/更新），分别经 `before`/`after` 参数传回续页，`null` 表示到底；两者不可同时使用。每页条数 `limit` 上限 1000。
- 访问密码（v2）：管理台设置密码后（Argon2id 哈希存 host.db），除 `auth` 组外全部管理 API 要求有效会话 cookie（`pegboard_session`，HttpOnly 30 天），否则 401 `TOKEN_INVALID`；未设密码时不鉴权（环回部署默认态）。auth 端点豁免鉴权：
  - `GET /api/admin/auth/state` → `{ has_password, lan_exposed }`（前端渲染登录/设密界面）
  - `POST /api/admin/auth/setup` `{password}`（≥8 字符）→ 首次设密并下发会话；已设返回 409
  - `POST /api/admin/auth/login` `{password}`；`POST /api/admin/auth/change` `{old,new}`（吊销全部旧会话）；`POST /api/admin/auth/logout`
  - 应用页面与能力 API（store/files/proxy/ws）不受密码影响。
- 应用包安装约束：zip 解压后总量 ≤ 256 MiB、条目 ≤ 10000、压缩体 ≤ 100 MiB；包路径经 zip-slip 防护；应用 id 取自包内 manifest.json 且与既有目录/注册表冲突时拒绝。
- 在线配置（v2）：
  - `GET /api/admin/config` → `{ config_path, has_config_file, listen, lan_exposed, limits, storage }`
  - `PUT /api/admin/config` `{listen?, limits?, storage?}`：limits 变更校验通过后热生效（逐应用重载重算生效限额，任一应用清单越权则 409 且整体不变）；listen 变更先 preflight 试绑定（被占 409）、局域网开放要求已设访问密码，写回配置文件后返回 `restart_required: true`，经 `host/restart` 生效；storage 变更仅接受绝对路径（相对路径重定基随启动方式而异）、两目录不得相同、保存前预建目录（不可创建即 409），仅落盘不换内存快照（目录随进程打开），重启生效。无配置文件（全默认启动）时 PUT 返回 409。
- `POST /api/admin/host/restart`：spawn 当前可执行文件（透传启动参数 + 重试窗口环境变量）后本进程优雅退出；托盘模式下由托盘事件循环负责 spawn，无托盘模式由服务端直接 spawn。
- 在线升级：更新源默认指向 GitHub Releases 的 `latest/download/update-manifest.json` 稳定端点（仓库名见 server/src/host/update.rs 的 `GITHUB_REPO` 常量），更新源写死于 `GITHUB_REPO` 常量（不可配置，信任锚是编译期内嵌公钥）；manifest 为信封结构 `{payload, signature}`，payload（版本 + 平台资产表）与资产字节均需通过编译期内嵌公钥的 minisign（Ed25519）验签，资产另验 sha256 与声明大小。`update/status` 的 `phase` 为状态机：`idle | checking | up_to_date | available{version,notes_url,size} | downloading{version,bytes,total} | installing{version} | restart_pending{version} | failed{error}`。下载仅手动触发、不自动轮询；`restart_pending` 后经 `host/restart` 切换新版本。检查/安装进行中重复触发返回 400。

## 9. 版本与兼容

- 契约一经发布，新增字段向后兼容；破坏性变更需新版本路径或清单字段。
- 能力面只增不改，旧应用不因宿主升级失效。
