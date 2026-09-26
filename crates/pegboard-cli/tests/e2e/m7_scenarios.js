// M7 场景测试（Playwright headless，驱动真实页面；使用系统 Chrome）。
// 用法：node m7_scenarios.js <pegboard-base> <ollama-mock-base> <glm-mock-base>
"use strict";
const assert = require("assert");
const { chromium } = require("playwright");

const BASE = process.argv[2];
const OLLAMA = process.argv[3];
const GLM = process.argv[4];

async function newPage(browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  if (process.env.E2E_DEBUG) {
    page.on("console", (m) => console.error("[page console]", m.type(), m.text()));
    page.on("pageerror", (e) => console.error("[page error]", e.message));
    page.on("requestfailed", (r) => console.error("[request failed]", r.url(), r.failure()?.errorText));
  }
  return { context, page };
}

// 场景 1：多人 Ollama Chat，会话隔离（两个浏览器上下文 = 两个用户）
async function scenarioOllamaChat() {
  const browser = await chromium.launch({ channel: "chrome", headless: true });
  try {
    const alice = await newPage(browser);
    const bob = await newPage(browser);

    await alice.page.goto(BASE + "/apps/ollama-chat/?api=" + encodeURIComponent(OLLAMA));
    await alice.page.locator("#name").fill("alice");
    await alice.page.locator("#name").blur();
    await alice.page.waitForNavigation().catch(() => {});
    await alice.page.goto(BASE + "/apps/ollama-chat/?api=" + encodeURIComponent(OLLAMA));
    await alice.page.locator("#text").fill("你好，我是 alice");
    await alice.page.locator("#send").click();
    await alice.page.waitForFunction(
      () => document.querySelectorAll(".msg.bot").length >= 1 &&
             document.querySelector(".msg.bot").textContent.includes("echo:")
    );

    await bob.page.goto(BASE + "/apps/ollama-chat/?api=" + encodeURIComponent(OLLAMA));
    await bob.page.locator("#name").fill("bob");
    await bob.page.locator("#name").blur();
    await bob.page.waitForNavigation().catch(() => {});
    await bob.page.goto(BASE + "/apps/ollama-chat/?api=" + encodeURIComponent(OLLAMA));
    await bob.page.locator("#text").fill("bob 的消息");
    await bob.page.locator("#send").click();
    await bob.page.waitForFunction(
      () => document.querySelectorAll(".msg.bot").length >= 1
    );

    // 隔离：bob 看不到 alice 的消息
    const bobLog = await bob.page.evaluate(() =>
      Array.from(document.querySelectorAll(".msg")).map((d) => d.textContent)
    );
    assert(!bobLog.some((t) => t.includes("alice")), "bob 不应看到 alice 的消息: " + bobLog);
    const aliceLog = await alice.page.evaluate(() =>
      Array.from(document.querySelectorAll(".msg")).map((d) => d.textContent)
    );
    assert(!aliceLog.some((t) => t.includes("bob 的消息")), "alice 不应看到 bob 的消息");

    // 模型列表经代理拉取成功
    const models = await alice.page.evaluate(() =>
      Array.from(document.querySelectorAll("#model option")).map((o) => o.value)
    );
    assert(models.includes("llama3"), "模型列表应含 llama3: " + models);

    await alice.context.close();
    await bob.context.close();
    console.log("SCENARIO-OLLAMA-OK");
  } finally {
    await browser.close();
  }
}

// 场景 2：GLM 额度查询（跨域经代理 + Key 存 KV + 缓存）
async function scenarioGlmQuota() {
  const browser = await chromium.launch({ channel: "chrome", headless: true });
  try {
    const { context, page } = await newPage(browser);
    await page.goto(BASE + "/apps/glm-quota/?api=" + encodeURIComponent(GLM));
    await page.locator("#key").fill("sk-test-123");
    await page.locator("#save-key").click();
    await page.locator("#refresh").click();
    await page.waitForFunction(() =>
      document.querySelector("#result") &&
      document.querySelector("#result").textContent.includes("total_usage")
    );
    // 重载页面 → loadKey 命中缓存提示（缓存持久于 KV）
    await page.reload();
    await page.waitForFunction(() =>
      (document.querySelector("#cached") || {}).textContent &&
      document.querySelector("#cached").textContent.includes("缓存") &&
      document.querySelector("#result").textContent.includes("total_usage")
    );
    await context.close();
    console.log("SCENARIO-GLM-OK");
  } finally {
    await browser.close();
  }
}

// 场景 3：局域网文件共享（上传 / 列表 / 下载 / 签名分享 / 删除）
async function scenarioLanShare() {
  const browser = await chromium.launch({ channel: "chrome", headless: true });
  try {
    const { context, page } = await newPage(browser);
    await page.goto(BASE + "/apps/lan-share/");
    const picker = await page.locator("#picker");
    await picker.setInputFiles({
      name: "share-me.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("lan-share-scenario-content"),
    });
    await page.waitForFunction(() =>
      document.querySelectorAll("#files tbody tr").length >= 1 &&
      document.querySelector("#files tbody tr").textContent.includes("share-me.txt")
    );
    // 签名分享
    await page.locator("#files tbody tr:first-child button", { hasText: "签名分享" }).click();
    await page.waitForSelector("#files .share a");
    const shareUrl = await page.locator("#files .share a").getAttribute("href");
    assert(shareUrl && shareUrl.includes("/api/files/"), "share url: " + shareUrl);
    // 签名链接可被无身份访问（新上下文，无 SDK / 无应用身份）
    const anon = await browser.newContext();
    const anonPage = await anon.newPage();
    const resp = await anonPage.request.get(BASE + shareUrl);
    assert(resp.status() === 200, "signed url status: " + resp.status());
    const text = await resp.text();
    assert(text === "lan-share-scenario-content", "signed content mismatch");
    // 下载链接（应用内 url）
    const href = await page.locator("#files tbody a").first().getAttribute("href");
    const dl = await page.request.get(BASE + href);
    assert(dl.status() === 200, "download status: " + dl.status() + " url=" + href);
    // 删除
    await page.locator("#files tbody tr:first-child button", { hasText: "删除" }).click();
    await page.waitForFunction(() => document.querySelectorAll("#files tbody tr").length === 0);
    await context.close();
    await anon.close();
    console.log("SCENARIO-LANSHARE-OK");
  } finally {
    await browser.close();
  }
}

(async () => {
  await scenarioOllamaChat();
  await scenarioGlmQuota();
  await scenarioLanShare();
  console.log("M7-SCENARIOS-OK");
  process.exit(0);
})().catch((e) => {
  console.error("M7-SCENARIO-FAIL: " + ((e && e.message) || e));
  process.exit(1);
});
