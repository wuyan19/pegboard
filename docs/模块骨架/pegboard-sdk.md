# pegboard-sdk / 构建与产物

## 文档职责范围

- 定义 SDK 的源文件组织、产物形态、构建方式、嵌入与消费方式。
- 只描述构建与打包，不重复 SDK API（见 SDK 设计文档）。
- 不规定实现细节；只定边界与产物契约。

**结论先行**：SDK 不需要构建工具链。两个手写静态文件，用 `include_bytes!` 编译期内嵌。

## 产物

```
pegboard-sdk/
  assets/
    sdk.js           运行时，浏览器侧
    sdk.d.ts         类型定义，供 AI 与开发者
  src/
    lib.rs           暴露 const 与元信息
  Cargo.toml
```

两个产物均为**手写源文件**，不经过 TypeScript / 打包器 / minifier：

- 体积小（预计 sdk.js < 10 KiB），无依赖，无可读性损失。
- 避免引入 node / npm 工具链，保持单语言构建。
- 升级 SDK 即编辑源文件，无编译步骤。

## 嵌入方式

`pegboard-sdk/src/lib.rs`：

```rust
//! SDK 静态产物。

/// 运行时脚本。
pub const SDK_JS: &[u8] = include_bytes!("../assets/sdk.js");

/// 类型定义。
pub const SDK_DTS: &[u8] = include_bytes!("../assets/sdk.d.ts");

/// 产物版本，与 host 版本对齐。
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 按路径取产物。供 statics 模块直接调用。
pub fn asset(name: &str) -> Option<(&'static [u8], &'static str)> {
    match name {
        "sdk.js"   => Some((SDK_JS, "application/javascript; charset=utf-8")),
        "sdk.d.ts" => Some((SDK_DTS, "application/typescript; charset=utf-8")),
        _          => None,
    }
}
```

`pegboard-server/src/statics/mod.rs` 直接调用 `pegboard_sdk::asset(name)`，无需 `build.rs`、无需 `OUT_DIR`。

## 应用识别注入

SDK 无需宿主在 HTML 中注入变量。app_id 由 SDK 从路径自行解析：

```js
const APP_ID = (location.pathname.match(/^\/apps\/([a-z0-9_-]+)\//) || [])[1] || null;
```

- 应用始终从 `/apps/<id>/...` 加载，路径可靠。
- API 请求统一带 `X-Pegboard-App: <id>` 头。
- 未识别到 app_id 时，能力调用统一返回 `APP_NOT_FOUND`；SDK 不隐藏此错误。

这样应用产物无需被宿主改写，dist 可原样放置。

## sdk.js 结构

单文件 IIFE，无模块、无依赖、无副作用泄漏：

```js
(function () {
  "use strict";

  const APP_ID = parseAppId();
  const NATIVE_FETCH = window.fetch.bind(window);

  // ---- HostError ----
  class HostError extends Error { /* code, detail */ }

  // ---- 内部请求工具 ----
  function api(path, init) { /* 加 X-Pegboard-App，映射非 2xx 为 HostError */ }

  // ---- store ----
  const store = { get, set, delete: del, list, batch };

  // ---- files ----
  const files = { upload, get, url, sign, list, delete: del };

  // ---- fetch ----
  function hostFetch(url, options) { /* 走 /api/proxy */ }

  // ---- WebSocket ----
  function connectWS(url) { /* 走 /api/ws-proxy，返回原生兼容对象 */ }

  // ---- 资源 ----
  function hostUrl(url) { /* /api/asset?url=... */ }

  // ---- user ----
  const user = readUser(); // 由宿主以 __pegboard_user 注入；缺失则 undefined

  // ---- 导出 ----
  window.host = { store, files, fetch: hostFetch, connectWS, url: hostUrl, user };

  // ---- fetch shim ----
  if (shouldShim()) {
    window.fetch = function (input, init) { /* 同源原生；跨域走 host.fetch */ };
  }

  // 内部辅助
  function parseAppId() { /* ... */ }
  function shouldShim() { /* 读取 __pegboard_shim 或默认 true */ }
})();
```

约束：
- 不污染全局除 `window.host` 外的任何名字。
- 不抛同步异常；所有失败走 `Promise.reject(HostError)`。
- 不缓存身份；每次请求从头拼 `X-Pegboard-App`。
- shim 只包装一层，不重复包装（标记位）。

## sdk.d.ts 结构

与 SDK 设计文档中的类型定义一致。以 `declare namespace Pegboard` 起头，末尾 `declare global { interface Window { host: Pegboard.Host } }`。

内容稳定面：
- `HostError`、`KVEntry`、`FileMeta`、`Store`、`Files`、`Host`。
- 不含实现细节、不含宿主内部结构。

## 版本对齐

- `SDK_VERSION` 与 `pegboard-server` 版本一致（workspace 统一版本）。
- 契约破坏性变更 → 主版本号变；SDK 只增不改 → 次版本号变。
- 宿主升级时 SDK 同步升级；`sdk.d.ts` 与 `sdk.js` 同源发布，无独立版本。

## 测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_present() {
        assert!(SDK_JS.len() > 0);
        assert!(SDK_DTS.len() > 0);
    }

    #[test]
    fn asset_lookup() {
        let (b, ct) = asset("sdk.js").unwrap();
        assert!(b.starts_with(b"(function"));
        assert!(ct.contains("javascript"));

        let (b, ct) = asset("sdk.d.ts").unwrap();
        assert!(ct.contains("typescript"));
    }

    #[test]
    fn asset_lookup_unknown() {
        assert!(asset("nope").is_none());
    }

    #[test]
    fn sdk_exposes_window_host() {
        let s = std::str::from_utf8(SDK_JS).unwrap();
        assert!(s.contains("window.host"));
    }

    #[test]
    fn dts_declares_host() {
        let s = std::str::from_utf8(SDK_DTS).unwrap();
        assert!(s.contains("interface Host"));
        assert!(s.contains("interface Window"));
    }
}
```

浏览器侧行为测试放在 `tests/` 场景验证中，不在此 crate。

## 依赖

```toml
[dependencies]
# 无
```

`pegboard-sdk` 是零依赖 crate，只包含两个 `include_bytes!` 和常量。server 依赖它，其余组件不感知。

## 设计要点

- **无工具链**：不引入 node / TypeScript / 打包器；保持单语言构建，降低协作与 CI 成本。
- **编译期内嵌**：`include_bytes!` 把产物打进二进制，运行时无文件依赖；升级宿主即升级 SDK。
- **产物手写**：体积小、可读、可审计；不做压缩、不做摇树，避免黑盒。
- **app_id 由 SDK 解析**：宿主无需改写应用 HTML，dist 原样可用。
- **单入口**：只暴露 `window.host`；shim 为可选，不改变显式路径。
- **版本与宿主对齐**：SDK 不独立演进，契约变则版本变，避免版本矩阵。
