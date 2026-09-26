// M3 场景：SDK host.store 读写 KV（真实 sdk.js + 真实服务器）。
// 用法：node sdk_store_scenario.js <base-url> <sdk.js路径>
"use strict";
const fs = require("fs");

const base = process.argv[2];
const sdkPath = process.argv[3];

// 浏览器环境最小垫片：sdk.js 只依赖 window 全局与 fetch
global.window = global;
global.__PEGBOARD__ = { appId: "kvtest", shim: true };

// 浏览器相对 URL 由页面 origin 解析；node fetch 需要绝对地址
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
  // 写读回环
  await host.store.set("scenarios/m3", { hello: "world", n: 42 });
  const got = await host.store.get("scenarios/m3");
  assert(got && got.hello === "world" && got.n === 42, "roundtrip: " + JSON.stringify(got));

  // 未命中为 null，不抛错
  const missing = await host.store.get("scenarios/absent");
  assert(missing === null, "missing should be null, got " + JSON.stringify(missing));

  // 前缀列表（字典序）
  await host.store.set("scenarios/list/a", 1);
  await host.store.set("scenarios/list/b", 2);
  const items = await host.store.list("scenarios/list/");
  assert(Array.isArray(items) && items.length === 2, "list len: " + items.length);
  assert(items[0].key === "scenarios/list/a", "list order: " + JSON.stringify(items));

  // 批量：set + delete 原子生效
  await host.store.batch([
    { op: "set", key: "scenarios/batch/1", value: "x" },
    { op: "delete", key: "scenarios/m3" }
  ]);
  const after = await host.store.get("scenarios/m3");
  assert(after === null, "batch delete applied");
  const b1 = await host.store.get("scenarios/batch/1");
  assert(b1 === "x", "batch set applied");

  // 错误结构化：HostError.code（BigInt 不可 JSON 序列化）
  let err = null;
  try {
    await host.store.set("bad", 10n);
  } catch (e) {
    err = e;
  }
  assert(err && err.code === "INVALID_REQUEST", "HostError.code: " + (err && err.code));

  await host.store.delete("scenarios/batch/1");
  console.log("SDK-SCENARIO-OK " + base);
})().catch((e) => {
  console.error("FAIL: " + ((e && e.stack) || e));
  process.exit(1);
});
