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
    "store_bytes": 10485760,
    "file_bytes": 104857600,
    "net_rps": 20
  }
}
```

字段：
- `entry`：入口文件，相对应用根目录。
- `permissions.net` / `ws`：目标 origin 或完整 URL 白名单；为空表示禁止。
- `permissions.shim`：是否启用全局 fetch 兜底，默认 `true`。
- `limits`：缺省时使用宿主默认值。

## 3. 静态托管

```
GET /apps/:app_id/*
```

- 命中文件则直出，正确 MIME。
- 未命中且非文件路径时回退到 `entry`（SPA）。
- 支持 Range、ETag、Cache-Control。
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
ANY /api/proxy?url=<encoded>     HTTP 代理，透传 method/headers/body
GET /api/ws-proxy?url=<encoded>  WebSocket 升级
GET /api/asset?url=<encoded>     资源代理，用于 img/font/css
```

规则：
- `url` 必须命中 `manifest.permissions.net`（asset 同）。
- 每跳重定向重新校验白名单。
- 默认拒绝内网、本机、云元数据地址；本机仅按 app + 端口显式放行。
- 响应头清理：移除 `Set-Cookie`、`Access-Control-*`、`Content-Security-Policy`；保留 `Content-Type`、`Content-Length`、`ETag`。
- 流式响应（SSE、chunked）透传，不缓冲。
- 二进制安全。

## 6. 错误码

| code | 含义 |
|---|---|
| `APP_NOT_FOUND` | 应用不存在 |
| `PERMISSION_DENIED` | 未声明该权限 |
| `TARGET_DENIED` | 目标不在白名单或命中 SSRF 规则 |
| `LIMIT_EXCEEDED` | 超出限额 |
| `NOT_FOUND` | 资源不存在 |
| `INVALID_REQUEST` | 参数或请求体非法 |
| `TOKEN_INVALID` | 签名 token 无效或过期 |
| `UPSTREAM_ERROR` | 目标服务错误 |
| `TIMEOUT` | 超时 |

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
GET  /api/admin/apps
GET  /api/admin/apps/:id
POST /api/admin/apps/:id/enable
POST /api/admin/apps/:id/disable
GET  /api/admin/logs?app=&since=&limit=
```

- 只操作宿主元数据，不读写应用业务数据。
- 是否鉴权由部署配置决定。

## 9. 版本与兼容

- 契约一经发布，新增字段向后兼容；破坏性变更需新版本路径或清单字段。
- 能力面只增不改，旧应用不因宿主升级失效。
