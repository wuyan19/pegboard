// 场景上游：本地 HTTP 服务（/json、/echo、/sse），打印实际端口后保持运行。
// 用法：node upstream_fixture.js
"use strict";
const http = require("http");

const server = http.createServer((req, res) => {
  const url = req.url.split("?")[0];
  if (url === "/json") {
    res.writeHead(200, { "content-type": "application/json", "set-cookie": "leak=1" });
    res.end(JSON.stringify({ upstream: true }));
  } else if (url === "/echo") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "text/plain" });
      res.end("echo:" + body);
    });
  } else if (url === "/sse") {
    // 每 100ms 一条事件，共 20 条（约 2s），用于流式首字节与透传判定
    res.writeHead(200, { "content-type": "text/event-stream" });
    let i = 0;
    const timer = setInterval(() => {
      i += 1;
      res.write("data: event-" + i + "\n\n");
      if (i >= 20) {
        clearInterval(timer);
        res.end();
      }
    }, 100);
    req.on("close", () => clearInterval(timer));
  } else {
    res.writeHead(404, { "content-type": "text/plain" });
    res.end("nope");
  }
});

server.listen(0, "127.0.0.1", () => {
  const addr = server.address();
  console.log("UPSTREAM-READY http://127.0.0.1:" + addr.port);
});
