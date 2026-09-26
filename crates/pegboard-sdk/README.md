# pegboard-sdk

注入前端的宿主 SDK：`sdk.js`（运行时）与 `sdk.d.ts`（类型），零依赖。
宿主在应用页面 `<head>` 最前注入引导与脚本：

```html
<script>window.__PEGBOARD__ = { "appId": "your-app", "shim": true, "subject": null }</script>
<script src="/sdk.js"></script>
```

页面即可使用 `window.host`。本地开发把 `sdk.d.ts` 放进项目即可获得类型检查；
AI 依据 `sdk.d.ts` 生成应用代码。

## 使用规则（给 AI 的约束）

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

## 示例

### 跨域拉取 + 缓存

```js
async function getQuota() {
  const cached = await host.store.get("quota");
  if (cached && Date.now() - cached.at < 60_000) return cached.data;
  const r = await host.fetch("https://open.bigmodel.cn/api/paas/v4/usage");
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

完整应用示例见仓库 `apps/`：ollama-chat（流式 + 会话隔离）、glm-quota（跨域 + 缓存）、
lan-share（上传 / 下载 / 签名分享）。
