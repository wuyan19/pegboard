// Admin 页面真实浏览器冒烟：加载、详情路由、用量、过滤、加载更早、XSS 转义。
// 页面契约见 crates/pegboard-server/src/admin/page.html 与 Admin页面优化方案.md §14。
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

    // 1. 应用墙卡片加载（lan-share 在 fixture apps 里）
    await page.waitForFunction(() =>
      document.querySelectorAll("#apps .app-card").length >= 1 &&
      !document.querySelector("#apps-body").textContent.includes("加载中")
    );
    // 2. server-info 状态条
    await page.waitForFunction(() =>
      document.querySelector("#server-info").textContent.includes("v0.")
    );
    // 3. 应用名链接指向 /apps/<id>/
    const href = await page
      .locator("#apps .app-card a")
      .first()
      .getAttribute("href");
    assert(href && href.startsWith("/apps/"), "app link: " + href);
    // 4. 卡片点击 → 详情路由（概览含实际用量），返回应用墙
    await page.locator("#apps .app-card").first().click();
    await page.waitForFunction(() =>
      location.hash.startsWith("#/apps/") &&
      document.getElementById("app-pane").textContent.includes("实际用量")
    );
    await page.goBack();
    await page.waitForFunction(() => !document.getElementById("view-wall").hidden);

    // 5. 审计过滤：outcome=Denied
    await page.click('.nav a[href="#/audit"]');
    await page.waitForFunction(() =>
      !document.querySelector("#logs-body").textContent.includes("加载中")
    );
    await page.selectOption("#f-outcome", "Denied");
    await page.waitForFunction(() => {
      const pills = Array.from(document.querySelectorAll("#logs-body .pill"));
      return pills.length >= 1 && pills.every((p) => p.textContent === "Denied");
    });
    assert(
      (await page.evaluate(() =>
        Array.from(document.querySelectorAll("#logs-body .pill")).every((p) => p.textContent === "Denied")
      )),
      "过滤后应只剩 Denied"
    );
    // 重置过滤 + 换页大小 20：第一页有 较旧 按钮、无 较新 按钮
    await page.selectOption("#f-outcome", "");
    await page.selectOption("#f-limit", "20");
    await page.waitForFunction(() => !document.getElementById("logs-next").disabled);
    assert(await page.locator("#logs-prev").isDisabled(), "第一页不应有较新页");
    // 翻到第二页再翻回来
    await page.click("#logs-next");
    await page.waitForFunction(() => document.getElementById("log-page").textContent.includes("2"));
    assert(await page.locator("#logs-prev").isEnabled(), "第二页应有较新页");
    await page.click("#logs-prev");
    await page.waitForFunction(() => document.getElementById("log-page").textContent.includes("1"));

    // 回应用墙做安装 / 卸载 / 删除
    await page.click('.nav a[href="#/"]');
    await page.waitForFunction(() => !document.getElementById("view-wall").hidden);

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
      // 卸载 → 未安装态；删除 → 彻底移除
      page.once("dialog", (d) => d.accept());
      await page
        .locator("#apps-body .app-card", { hasText: "xss" })
        .locator('button[data-act="uninstall"]')
        .click();
      await page.waitForFunction(() =>
        document.querySelector("#apps-body").textContent.includes("未安装")
      );
      page.once("dialog", (d) => d.accept());
      await page
        .locator("#apps-body .app-card", { hasText: "xss" })
        .locator('button[data-act="delete"]')
        .click();
      await page.waitForFunction(() =>
        !document.querySelector("#apps-body").textContent.includes("xss")
      );
    }

    // 7. zip 上传安装（页面 UI 走 /api/admin/apps/package）
    const JSZip = null; // 无依赖：用 Rust 端已验证；此处用 node 原生构造最小 zip（stored）
    const crcTable = (() => {
      const t = new Uint32Array(256);
      for (let n = 0; n < 256; n++) {
        let c = n;
        for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
        t[n] = c >>> 0;
      }
      return t;
    })();
    function crc32(buf) {
      let c = 0xffffffff;
      for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
      return (c ^ 0xffffffff) >>> 0;
    }
    function buildZip(entries) {
      const chunks = [];
      const central = [];
      let offset = 0;
      for (const [name, data] of entries) {
        const nameBytes = Buffer.from(name);
        const crc = crc32(data);
        const local = Buffer.alloc(30);
        local.writeUInt32LE(0x04034b50, 0);
        local.writeUInt16LE(20, 4);
        local.writeUInt16LE(0, 6);
        local.writeUInt16LE(0, 8);
        local.writeUInt16LE(0, 10);
        local.writeUInt16LE(0, 12);
        local.writeUInt32LE(crc, 14);
        local.writeUInt32LE(data.length, 18);
        local.writeUInt32LE(data.length, 22);
        local.writeUInt16LE(nameBytes.length, 26);
        local.writeUInt16LE(0, 28);
        chunks.push(local, nameBytes, data);
        central.push({ nameBytes, crc, size: data.length, offset });
        offset += 30 + nameBytes.length + data.length;
      }
      const centralStart = offset;
      for (const e of central) {
        const h = Buffer.alloc(46);
        h.writeUInt32LE(0x02014b50, 0);
        h.writeUInt16LE(20, 4);
        h.writeUInt16LE(20, 6);
        h.writeUInt16LE(0, 8);
        h.writeUInt16LE(0, 10);
        h.writeUInt16LE(0, 12);
        h.writeUInt16LE(0, 14);
        h.writeUInt32LE(e.crc, 16);
        h.writeUInt32LE(e.size, 20);
        h.writeUInt32LE(e.size, 24);
        h.writeUInt16LE(e.nameBytes.length, 28);
        h.writeUInt32LE(e.offset, 42);
        chunks.push(h, e.nameBytes);
        offset += 46 + e.nameBytes.length;
      }
      const end = Buffer.alloc(22);
      end.writeUInt32LE(0x06054b50, 0);
      end.writeUInt16LE(central.length, 8);
      end.writeUInt16LE(central.length, 10);
      end.writeUInt32LE(offset - centralStart, 12);
      end.writeUInt32LE(centralStart, 16);
      chunks.push(end);
      return Buffer.concat(chunks);
    }
    const pkgZip = buildZip([
      ["manifest.json", Buffer.from(JSON.stringify({ id: "ui-pkg", name: "UI Pkg", entry: "index.html" }))],
      ["index.html", Buffer.from("<h1>ui-pkg</h1>")],
    ]);
    await page.setInputFiles("#package", {
      name: "ui-pkg.zip",
      mimeType: "application/zip",
      buffer: pkgZip,
    });
    await page.waitForFunction(() =>
      document.getElementById("install-msg").textContent.includes("已安装 ui-pkg")
    );
    // 上传的应用已注册并可访问
    const pkgResp = await page.request.get(BASE + "/apps/ui-pkg/");
    assert(pkgResp.status() === 200, "packaged app status: " + pkgResp.status());
    // 清理
    page.once("dialog", (d) => d.accept());
    await page
      .locator("#apps-body .app-card", { hasText: "ui-pkg" })
      .locator('button[data-act="delete"]')
      .click();
    await page.waitForFunction(() =>
      !document.querySelector("#apps-body").textContent.includes("ui-pkg")
    );

    // 8. 无页面脚本错误
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
