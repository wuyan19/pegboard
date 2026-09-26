// M6 场景：SDK host.connectWS 经 ws-proxy 双向 echo（真实 sdk.js + 真实服务器 + 真实 WS echo 上游）。
// 用法：node sdk_ws_scenario.js <http-base> <ws-upstream-url> <sdk.js路径>
"use strict";
const fs = require("fs");

const base = process.argv[2];
const upstream = process.argv[3];
const sdkPath = process.argv[4];

global.window = global;
global.__PEGBOARD__ = { appId: "wstest", shim: false };
global.location = { host: base.replace(/^https?:\/\//, "") };

eval(fs.readFileSync(sdkPath, "utf8"));

function assert(cond, msg) {
  if (!cond) {
    console.error("FAIL: " + msg);
    process.exit(1);
  }
}

// WebSocket 全局（node 22 undici）事件语义与浏览器一致
function once(ws, event) {
  return new Promise((resolve) => ws.addEventListener(event, resolve, { once: true }));
}

(async () => {
  const watchdog = setTimeout(() => {
    console.error("FAIL: scenario timed out");
    process.exit(1);
  }, 20000);
  watchdog.unref?.();
  const ws = host.connectWS(upstream);
  await once(ws, "open");

  // 文本 echo
  const textReply = new Promise((resolve) =>
    ws.addEventListener("message", (e) => resolve(e.data), { once: true })
  );
  ws.send("hello-ws-proxy");
  assert((await textReply) === "hello-ws-proxy", "text echo mismatch");

  // 二进制 echo（Blob）
  const binReply = new Promise((resolve) =>
    ws.addEventListener("message", (e) => resolve(e.data), { once: true })
  );
  const payload = new Uint8Array([1, 2, 3, 255]);
  ws.send(payload);
  const echoed = new Uint8Array(await new Response(await binReply).arrayBuffer());
  assert(
    echoed.length === payload.length &&
      echoed.every((v, i) => v === payload[i]),
    "binary echo mismatch"
  );

  // 多轮往返
  for (let i = 0; i < 5; i++) {
    const reply = new Promise((resolve) =>
      ws.addEventListener("message", (e) => resolve(e.data), { once: true })
    );
    ws.send("round-" + i);
    assert((await reply) === "round-" + i, "round " + i + " mismatch");
  }

  // 关闭
  const closed = once(ws, "close");
  ws.close(1000, "done");
  await closed;

  // 非白名单目标 → 连接被拒（close 而非 open）
  const denied = host.connectWS("ws://127.0.0.1:9/x");
  const outcome = await Promise.race([
    once(denied, "open").then(() => "open"),
    once(denied, "close").then(() => "close"),
    once(denied, "error").then(() => "error"),
  ]);
  assert(outcome !== "open", "denied target should not open");

  console.log("SDK-WS-SCENARIO-OK");
})().catch((e) => {
  console.error("FAIL: " + ((e && e.stack) || e));
  process.exit(1);
});
