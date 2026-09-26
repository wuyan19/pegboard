// M4 场景：SDK host.fetch 跨域拉取 + fetch shim + host.url（真实 sdk.js + 真实服务器 + 本地上游）。
// 用法：node sdk_fetch_scenario.js <base-url> <sdk.js路径> <upstream-base-url>
"use strict";
const fs = require("fs");

const base = process.argv[2];
const sdkPath = process.argv[3];
const upstreamBase = process.argv[4];

global.window = global;
global.__PEGBOARD__ = { appId: "nettest", shim: true };

const nativeFetch = global.fetch;
global.fetch = function (input, init) {
  if (typeof input === "string" && input.startsWith("/")) {
    input = base + input;
  }
  return nativeFetch.call(this, input, init);
};

eval(fs.readFileSync(sdkPath, "utf8"));

function assert(cond, msg) {
  if (!cond) {
    console.error("FAIL: " + msg);
    process.exit(1);
  }
}

(async () => {
  // host.fetch 跨域绝对 URL → 经代理成功
  const res = await host.fetch(upstreamBase + "/json", {
    headers: { "X-Custom": "abc" }
  });
  assert(res.status === 200, "host.fetch status: " + res.status);
  const data = await res.json();
  assert(data.upstream === true, "host.fetch body: " + JSON.stringify(data));

  // 目标不在白名单 → HostError TARGET_DENIED
  let denied = null;
  try {
    await host.fetch("http://127.0.0.1:9/x");
  } catch (e) {
    denied = e;
  }
  assert(denied && denied.code === "TARGET_DENIED", "denied code: " + (denied && denied.code));

  // host.fetch 请求头透传 + 响应可读文本
  const textRes = await host.fetch(upstreamBase + "/echo", { method: "POST", body: "ping" });
  const text = await textRes.text();
  assert(typeof text === "string" && text.length > 0, "echo body empty");

  // host.url：跨域资源 → /api/asset 同源地址
  const assetUrl = host.url(upstreamBase + "/json");
  assert(assetUrl.indexOf("/api/asset?url=") === 0, "asset url: " + assetUrl);
  const assetRes = await nativeFetch(base + assetUrl, { headers: { "X-Pegboard-App": "nettest" } });
  assert(assetRes.status === 200, "asset status: " + assetRes.status);

  // fetch shim：原生 fetch 跨域 URL 自动走代理（shim 开启）
  const shimmed = await fetch(upstreamBase + "/json");
  assert(shimmed.status === 200, "shim status: " + shimmed.status);
  const shimData = await shimmed.json();
  assert(shimData.upstream === true, "shim body");

  // shim：同源相对路径仍走原生（打到宿主自身静态路由）
  const rel = await fetch("/sdk.js");
  assert(rel.status === 200, "relative fetch status: " + rel.status);

  console.log("SDK-FETCH-SCENARIO-OK");
})().catch((e) => {
  console.error("FAIL: " + ((e && e.stack) || e));
  process.exit(1);
});
