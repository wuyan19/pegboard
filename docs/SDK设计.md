# Pegboard SDK 设计

## 1. 注入

- 宿主在应用页面 `<head>` 最前注入 `<script src="/sdk.js"></script>`。
- 同步执行，早于应用脚本。
- 暴露 `window.host`；未声明能力时对应方法存在但调用返回 `PERMISSION_DENIED`。
- 同时提供 `/sdk.d.ts` 供本地类型检查与 AI 读取。

## 2. 设计原则

- **一个入口**：`window.host`，不污染全局。
- **对齐 Web 习惯**：`fetch` 语义对齐原生，`WebSocket` 对齐原生。
- **显式优先，兜底为辅**：`host.fetch` 为显式路径；`fetch` shim 为兼容路径。
- **错误结构化**：全部 Promise 拒绝为 `HostError`。
- **无隐藏状态**：所有能力围绕 `app_id` 由宿主解析，SDK 不缓存身份。

## 3. 类型定义

```ts
declare namespace Pegboard {
  interface HostError extends Error {
    code:
      | "APP_NOT_FOUND"
      | "PERMISSION_DENIED"
      | "TARGET_DENIED"
      | "LIMIT_EXCEEDED"
      | "NOT_FOUND"
      | "INVALID_REQUEST"
      | "TOKEN_INVALID"
      | "UPSTREAM_ERROR"
      | "TIMEOUT";
    detail?: Record<string, unknown>;
  }

  interface KVEntry {
    key: string;
    value: unknown;
  }

  interface FileMeta {
    id: string;
    name: string;
    size: number;
    mime: string;
    created: number;
  }

  interface Store {
    get(key: string): Promise<unknown | null>;
    set(key: string, value: unknown): Promise<void>;
    delete(key: string): Promise<void>;
    list(prefix?: string): Promise<KVEntry[]>;
    batch(ops: { op: "set" | "delete"; key: string; value?: unknown }[]): Promise<void>;
  }

  interface Files {
    upload(file: Blob | File): Promise<Omit<FileMeta, "created">>;
    get(id: string): Promise<Blob | null>;
    url(id: string): string;
    sign(id: string, ttlSec: number): Promise<string>;
    list(opts?: { prefix?: string; cursor?: string; limit?: number }):
      Promise<{ items: FileMeta[]; next: string | null }>;
    delete(id: string): Promise<void>;
  }

  interface Host {
    store: Store;
    files: Files;
    fetch(url: string, options?: RequestInit): Promise<Response>;
    connectWS(url: string): WebSocket;
    url(url: string): string;
    user?: { id: string | null };
  }
}

declare global {
  interface Window { host: Pegboard.Host }
}
```

## 4. 行为语义

### store
- `get` 未命中返回 `null`，不抛错。
- `set` 接受任意可 JSON 序列化的值；不可序列化抛 `INVALID_REQUEST`。
- `list` 按 key 字典序返回；`prefix` 为空返回全部。
- `batch` 原子：任一失败全部回滚。
- 超配额抛 `LIMIT_EXCEEDED`，`detail` 含当前用量与上限。

### files
- `upload` 接受 `File` 或 `Blob`；超单文件限额抛 `LIMIT_EXCEEDED`。
- `get` 不存在返回 `null`。
- `url` 返回同源地址，可直接用于 `<img>` / `<a download>`，无需 token。
- `sign` 返回带 token 的完整 URL，供外部无身份访问；`ttlSec` 上限由宿主配置。
- `list` 游标分页；`next` 为 `null` 表示结束。

### fetch
- 语义对齐原生 `fetch`：`method`、`headers`、`body`、`signal`、流式响应。
- 目标必须命中 `permissions.net`，否则 `TARGET_DENIED`。
- 不携带任何环境 cookie（宿主出站剥离 `Cookie`）；凭证用 `Authorization` 或自定义头传递，`Cookie` 为浏览器禁设头不可用。
- 响应为宿主清理后的同源响应。

### connectWS
- 返回原生 `WebSocket` 兼容对象。
- 目标必须命中 `permissions.ws`。
- 二进制、ping/pong、子协议透传。

### url
- 用于 `img`、`font`、CSS `url()`、`script` 等无法走 fetch 的资源。
- 目标必须命中 `permissions.net`。
- 返回同源地址，避免 canvas 污染。

### user
- 可能为 `undefined` 或 `{ id: null }`。
- 应用不得假设其存在；仅用于命名空间前缀等辅助用途。

## 5. fetch shim

- 当 `permissions.shim` 为真，宿主包装 `window.fetch`：
  - 同源或相对路径 → 原生 fetch。
  - 跨域绝对 URL → 转 `/api/proxy`。
  - 已是 `/api/*` 或 `host.fetch` 调用 → 不包装。
- 保留 `Request`、`Response`、`AbortController`、流式 body 语义。
- 声明 `shim: false` 时完全关闭，应用只能用 `host.fetch`。

## 6. 错误处理约定

- 所有失败以 `Promise.reject(HostError)` 返回。
- `HostError.code` 稳定；`message` 面向人；`detail` 面向程序。
- 应用应至少处理 `PERMISSION_DENIED` 与 `LIMIT_EXCEEDED`。

## 7. 示例

### 跨域拉取 + 缓存
```js
async function getQuota() {
  const cached = await host.store.get("quota");
  if (cached && Date.now() - cached.at < 60_000) return cached.data;
  const r = await host.fetch("https://open.bigmodel.cn/api/paas/v4/quota");
  const data = await r.json();
  await host.store.set("quota", { at: Date.now(), data });
  return data;
}
```

### 流式对话
```js
const r = await host.fetch("http://127.0.0.1:11434/api/chat", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ model: "llama3", messages, stream: true })
});
const reader = r.body.getReader();
const dec = new TextDecoder();
while (true) {
  const { done, value } = await reader.read();
  if (done) break;
  render(dec.decode(value));
}
```

### 文件上传与分享
```js
const meta = await host.files.upload(file);
const url  = await host.files.sign(meta.id, 600);
showLink(url); // 外部可访问，10 分钟有效
```

### WebSocket
```js
const ws = host.connectWS("wss://example.com/live");
ws.onmessage = e => render(e.data);
```

## 8. 使用约束（给 AI 的规则）

```text
同源请求用 fetch('/...')
跨域数据用 host.fetch(url, options)
跨域资源用 host.url(url)
WebSocket 用 host.connectWS(url)
存储用 host.store / host.files，key 自行按业务分区
多用户场景用 host.user?.id 拼 key，不假设宿主懂用户
跨域目标必须在 manifest.permissions 中声明
不要自行处理 CORS，不要使用第三方 cors 代理
所有失败按 HostError 处理，至少覆盖 PERMISSION_DENIED 与 LIMIT_EXCEEDED
```

## 9. 兼容

- `window.host` 只增不改；新增能力以新字段挂载。
- 已发布方法签名不变；废弃先标记，后移除。
- `sdk.d.ts` 与 `sdk.js` 同版本发布，AI 依据前者生成代码。
