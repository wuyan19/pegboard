// Admin 页面真实浏览器冒烟：加载、安装、用量、过滤、加载更早、XSS 转义。
// 用法：node admin_smoke.js <base-url>
"use strict";
const assert = require("assert");
const { chromium } = require("playwright");

const BASE = process.argv[2];

(async () => {
  const browser = await chromium.launch({ channel: "chrome", headless: true });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.setDefaultTimeout(15000);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));

    await page.goto(BASE + "/admin");

    // 1. 应用列表加载（lan-share 在 fixture apps 里）
    await page.waitForFunction(() =>
      document.querySelectorAll("#apps tbody tr").length >= 1 &&
      !document.querySelector("#apps-body").textContent.includes("加载中")
    );
    // 2. server-info 状态条
    await page.waitForFunction(() =>
      document.querySelector("#server-info").textContent.includes("v0.")
    );
    // 3. 应用名链接指向 /apps/<id>/
    const href = await page
      .locator("#apps tbody a")
      .first()
      .getAttribute("href");
    assert(href && href.startsWith("/apps/"), "app link: " + href);
    // 4. 行点击 → 模态框详情（含用量）
    await page.locator("#apps-body tr.clickable").first().click();
    await page.waitForFunction(() =>
      !document.getElementById("modal").hidden &&
      document.getElementById("modal-body").textContent.includes("实际用量")
    );
    await page.click("#modal-close");
    await page.waitForFunction(() => document.getElementById("modal").hidden);
    // 5. 审计日志过滤：outcome=Denied
    await page.selectOption("#f-outcome", "Denied");
    await page.waitForFunction(() =>
      !document.querySelector("#logs-body").textContent.includes("加载中")
    );
    const outcomeTexts = await page.evaluate(() =>
      Array.from(document.querySelectorAll("#logs-body .pill")).map((p) => p.textContent)
    );
    assert(
      outcomeTexts.length >= 1 && outcomeTexts.every((t) => t === "Denied"),
      "过滤后应只剩 Denied: " + outcomeTexts
    );
    // 重置过滤 + 换页大小 20：第一页有 较旧 按钮、无 较新 按钮
    await page.selectOption("#f-outcome", "");
    await page.selectOption("#f-limit", "20");
    await page.waitForFunction(() =>
      !document.querySelector("#logs-body").textContent.includes("加载中")
    );
    assert(await page.locator("#logs-next").isEnabled(), "第一页应有较旧页");
    assert(await page.locator("#logs-prev").isDisabled(), "第一页不应有较新页");
    // 翻到第二页再翻回来
    await page.click("#logs-next");
    await page.waitForFunction(() => document.getElementById("log-page").textContent.includes("2"));
    assert(await page.locator("#logs-prev").isEnabled(), "第二页应有较新页");
    await page.click("#logs-prev");
    await page.waitForFunction(() => document.getElementById("log-page").textContent.includes("1"));

    // 6. XSS 转义：安装一个 name 带脚本的清单，页面应显示文本而非执行
    const appsDir = process.env.ADMIN_E2E_APPS_DIR;
    if (appsDir) {
      const fs = require("fs");
      const dir = require("path").join(appsDir, "xss");
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(
        require("path").join(dir, "index.html"),
        "<h1>x</h1>"
      );
      fs.writeFileSync(
        require("path").join(dir, "manifest.json"),
        JSON.stringify({
          id: "xss",
          name: '<img src=x onerror=window.__pwned=1>',
          entry: "index.html",
        })
      );
      await page.fill("#install-id", "xss");
      await page.click("#install");
      await page.waitForFunction(() =>
        document.querySelector("#apps-body").textContent.includes("img src")
      );
      const pwned = await page.evaluate(() => window.__pwned);
      assert(!pwned, "清单 name 中的 HTML 不应被执行");
      // 卸载清理
      page.once("dialog", (d) => d.accept());
      await page
        .locator('#apps-body tr', { hasText: "xss" })
        .locator('button[data-act="uninstall"]')
        .click();
      await page.waitForFunction(() =>
        !document.querySelector("#apps-body").textContent.includes("img src")
      );
    }

    // 7. 无页面脚本错误
    assert.deepStrictEqual(errors, [], "page errors: " + errors.join("; "));

    await context.close();
    console.log("ADMIN-SMOKE-OK");
    process.exit(0);
  } finally {
    await browser.close();
  }
})().catch((e) => {
  console.error("ADMIN-SMOKE-FAIL: " + ((e && e.message) || e));
  process.exit(1);
});
