// M7 场景 mock 上游：Ollama（/api/tags、/api/chat NDJSON 流）与 GLM（额度 JSON）。
// 用法：node mocks_fixture.js
"use strict";
const http = require("http");

// ---- Ollama mock ----
const ollama = http.createServer((req, res) => {
  if (req.url === "/api/tags") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ models: [{ name: "llama3" }, { name: "qwen2.5" }] }));
    return;
  }
  if (req.url === "/api/chat") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const payload = JSON.parse(body);
      const user = payload.messages.filter((m) => m.role === "user").pop();
      res.writeHead(200, { "content-type": "application/x-ndjson" });
      // 分块 NDJSON 流，模拟流式生成
      const words = ["echo: ", (user && user.content) || "", " (", payload.model || "?", ")"];
      let i = 0;
      const timer = setInterval(() => {
        if (i < words.length) {
          res.write(JSON.stringify({
            model: payload.model,
            message: { role: "assistant", content: words[i] },
            done: false,
          }) + "\n");
          i += 1;
        } else {
          res.write(JSON.stringify({ done: true, done_reason: "stop" }) + "\n");
          res.end();
          clearInterval(timer);
        }
      }, 30);
    });
    return;
  }
  res.writeHead(404).end();
});

// ---- GLM mock ----
const glm = http.createServer((req, res) => {
  if (req.url.startsWith("/api/paas/v4/usage")) {
    const auth = req.headers["authorization"] || "";
    if (!auth.startsWith("Bearer sk-test-")) {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { code: "401", message: "invalid key" } }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({
      total_usage: "1024.50",
      balance: "88.20",
      model_usage: [{ model: "glm-4", usage: "512.25" }],
    }));
    return;
  }
  res.writeHead(404).end();
});

ollama.listen(0, "127.0.0.1", () => {
  console.log("OLLAMA-READY http://127.0.0.1:" + ollama.address().port);
});
glm.listen(0, "127.0.0.1", () => {
  console.log("GLM-READY http://127.0.0.1:" + glm.address().port);
});
