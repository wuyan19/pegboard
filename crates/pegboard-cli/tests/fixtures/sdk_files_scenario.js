// M5 场景：SDK host.files 上传/下载/签名（真实 sdk.js + 真实服务器）。
// 用法：node sdk_files_scenario.js <base-url> <sdk.js路径>
"use strict";
const fs = require("fs");

const base = process.argv[2];
const sdkPath = process.argv[3];

global.window = global;
global.__PEGBOARD__ = { appId: "filestest", shim: true };

const nativeFetch = global.fetch;
global.fetch = function (input, init) {
  if (typeof input === "string" && input.startsWith("/")) {
    input = base + input;
  }
  return nativeFetch.call(this, input, init);
};

// FormData / Blob 在 node 18+ 全局可用
eval(fs.readFileSync(sdkPath, "utf8"));

function assert(cond, msg) {
  if (!cond) {
    console.error("FAIL: " + msg);
    process.exit(1);
  }
}

(async () => {
  // 上传
  const content = "hello-files-scenario-" + Date.now();
  const blob = new Blob([content], { type: "text/plain" });
  const meta = await host.files.upload(blob);
  assert(meta && typeof meta.id === "string" && meta.id.length > 0, "upload meta: " + JSON.stringify(meta));
  assert(meta.name === "blob", "upload name: " + meta.name);
  assert(meta.size === content.length, "upload size: " + meta.size);

  // 下载
  const got = await host.files.get(meta.id);
  assert(got !== null, "get returned null");
  const text = await got.text();
  assert(text === content, "roundtrip: " + text);

  // 未命中为 null
  const missing = await host.files.get("NOPE");
  assert(missing === null, "missing should be null");

  // host.url 同源地址可直接 fetch
  const urlResp = await fetch(host.files.url(meta.id));
  assert(urlResp.status === 200, "url fetch status: " + urlResp.status);

  // 签名链接：无身份头可访问
  const signed = await host.files.sign(meta.id, 600);
  assert(typeof signed === "string" && signed.indexOf("/api/files/") === 0, "sign url: " + signed);
  const shared = await nativeFetch(base + signed);
  assert(shared.status === 200, "signed status: " + shared.status);
  const sharedText = await shared.text();
  assert(sharedText === content, "signed content mismatch");

  // 列表
  const page = await host.files.list({ prefix: "" });
  assert(Array.isArray(page.items) && page.items.length >= 1, "list items");

  // 删除后再取为 null
  await host.files.delete(meta.id);
  const after = await host.files.get(meta.id);
  assert(after === null, "deleted should be null");

  console.log("SDK-FILES-SCENARIO-OK");
})().catch((e) => {
  console.error("FAIL: " + ((e && e.stack) || e));
  process.exit(1);
});
