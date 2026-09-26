// pegboard sdk.js — M6：store / files / fetch / url / connectWS 全量。fetch shim 随 permissions.shim。
// 注入约定：宿主在本脚本之前注入
//   <script>window.__PEGBOARD__ = { appId: "...", shim: true }</script>
(function () {
  "use strict";

  var boot = typeof window !== "undefined" ? window.__PEGBOARD__ : null;
  var appId = boot && boot.appId ? String(boot.appId) : null;

  function fail(code, message, detail) {
    var err = new Error(message || code);
    err.code = code;
    if (detail) err.detail = detail;
    return err;
  }

  function apiHeaders(extra) {
    var h = extra || {};
    if (appId) h["X-Pegboard-App"] = appId;
    return h;
  }

  function apiFetch(path, options) {
    options = options || {};
    var headers = {};
    if (options.headers) {
      Object.keys(options.headers).forEach(function (k) { headers[k] = options.headers[k]; });
    }
    if (appId) headers["X-Pegboard-App"] = appId;
    return fetch("/api" + path, {
      method: options.method || "GET",
      headers: headers,
      body: options.body
    }).then(function (res) {
      if (res.ok) return res;
      return res
        .json()
        .catch(function () { return {}; })
        .then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
    });
  }

  function encodeKey(key) {
    // API 契约 §4.1：key URL 编码后置于路径，含 / 时编码为 %2F
    return encodeURIComponent(String(key));
  }

  var store = {
    get: function (key) {
      return apiFetch("/store/kv/" + encodeKey(key)).then(function (res) {
        return res.json();
      }).then(function (body) {
        return body.value === undefined ? null : body.value;
      });
    },
    set: function (key, value) {
      var text;
      try {
        text = JSON.stringify({ value: value === undefined ? null : value });
      } catch (e) {
        return Promise.reject(fail("INVALID_REQUEST", "value 不可序列化"));
      }
      return apiFetch("/store/kv/" + encodeKey(key), {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: text
      }).then(function () { return undefined; });
    },
    delete: function (key) {
      return apiFetch("/store/kv/" + encodeKey(key), { method: "DELETE" })
        .then(function () { return undefined; });
    },
    list: function (prefix) {
      var q = prefix ? "?prefix=" + encodeURIComponent(String(prefix)) : "";
      return apiFetch("/store/kv" + q).then(function (res) {
        return res.json();
      }).then(function (body) {
        return body.items || [];
      });
    },
    batch: function (ops) {
      var payload = (ops || []).map(function (op) {
        if (op.op === "delete") return { op: "delete", key: String(op.key) };
        return { op: "set", key: String(op.key), value: op.value === undefined ? null : op.value };
      });
      return apiFetch("/store/kv/batch", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ops: payload })
      }).then(function () { return undefined; });
    }
  };

  // ---- host.fetch：语义对齐原生 fetch（API 契约 §7 / SDK 设计 §4）----

  function sameOrigin(url) {
    if (typeof location !== "undefined" && location.origin) {
      return url.indexOf(location.origin + "/") === 0;
    }
    return false; // 无 location 环境（测试垫片）视绝对地址为跨域
  }

  function proxyUrl(target) {
    return "/api/proxy?url=" + encodeURIComponent(target);
  }

  // SDK 在所有请求上带应用身份头（ingress 应用识别依赖它）
  function withAppHeader(headers) {
    var out = {};
    if (headers) {
      Object.keys(headers).forEach(function (k) {
        if (k.toLowerCase() !== "x-pegboard-app") out[k] = headers[k];
      });
    }
    if (appId) out["X-Pegboard-App"] = appId;
    return out;
  }

  function hostFetch(url, options) {
    options = options || {};
    var target = String(url);
    if (target.indexOf("/") === 0 || sameOrigin(target)) {
      // 同源 / 相对路径 → 原生 fetch（不再二次包装）
      return fetch(target, options);
    }
    // 跨域绝对 URL → 代理；请求头透传（宿主出站清理 Cookie 等）
    var init = {
      method: options.method || "GET",
      headers: withAppHeader(options.headers),
      body: options.body,
      signal: options.signal
    };
    return fetch(proxyUrl(target), init).then(function (res) {
      if (res.ok) return res;
      return res.json().catch(function () { return {}; }).then(function (body) {
        var e = (body && body.error) || {};
        throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
      });
    });
  }

  // ---- host.url：img / font / CSS url() 等资源代理地址（/api/asset）----

  function hostUrl(target) {
    var t = String(target);
    if (t.indexOf("/") === 0 || sameOrigin(t)) {
      return t;
    }
    return "/api/asset?url=" + encodeURIComponent(t);
  }

  // ---- fetch shim（permissions.shim 时包装 window.fetch；SDK 设计 §5）----

  var nativeFetch = typeof fetch === "function" ? fetch : null;

  function installShim() {
    if (!nativeFetch) return;
    window.fetch = function (input, init) {
      // Request 对象展开为 init（需补充身份头）
      var isRequest = typeof input === "object" && input && typeof input.url === "string";
      var target = isRequest ? input.url : input;
      var t = String(target);
      var merged = init || {};
      if (isRequest) {
        merged = {
          method: input.method,
          headers: withAppHeader(headersToObject(input.headers)),
          body: input.body,
          signal: input.signal
        };
      } else if (t.indexOf("/api/") === 0 || t.indexOf("/") === 0) {
        merged.headers = withAppHeader(merged.headers);
      }
      if (t.indexOf("/api/") === 0 || t.indexOf("/") === 0 || sameOrigin(t)) {
        return nativeFetch(isRequest ? t : input, merged);
      }
      merged.headers = withAppHeader(merged.headers);
      return nativeFetch(proxyUrl(t), merged);
    };
  }

  function headersToObject(headers) {
    if (!headers) return {};
    if (typeof headers.forEach === "function") {
      var out = {};
      headers.forEach(function (v, k) { out[k] = v; });
      return out;
    }
    return headers;
  }

  // ---- host.files：Blob 原语与签名（API 契约 §4.2）----

  var files = {
    upload: function (file) {
      if (!file) return Promise.reject(fail("INVALID_REQUEST", "upload 需要文件"));
      var name = file.name || "blob";
      var mime = file.type || "application/octet-stream";
      var form = new FormData();
      form.append("file", file, name);
      var path = "/api/files";
      if (appId) {
        // FormData 请求头由浏览器设置；身份头单独附加
      }
      return fetch(path, {
        method: "POST",
        headers: withAppHeader(null),
        body: form
      }).then(function (res) {
        if (res.ok) return res.json();
        return res.json().catch(function () { return {}; }).then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
      });
    },
    get: function (id) {
      return fetch("/api/files/" + encodeURIComponent(String(id)), {
        headers: withAppHeader(null)
      }).then(function (res) {
        if (res.status === 404) return null;
        if (res.ok) return res.blob();
        return res.json().catch(function () { return {}; }).then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
      });
    },
    url: function (id) {
      // 原生导航（<a>/<img>）无法携带身份头：app 经查询参数（与 ws-proxy 同法）
      var base = "/api/files/" + encodeURIComponent(String(id));
      return appId ? base + "?app=" + encodeURIComponent(appId) : base;
    },
    sign: function (id, ttlSec) {
      return fetch("/api/files/" + encodeURIComponent(String(id)) + "/sign", {
        method: "POST",
        headers: withAppHeader({ "Content-Type": "application/json" }),
        body: JSON.stringify({ ttl: ttlSec })
      }).then(function (res) {
        if (res.ok) return res.json();
        return res.json().catch(function () { return {}; }).then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
      }).then(function (body) { return body.url; });
    },
    list: function (opts) {
      opts = opts || {};
      var q = [];
      if (opts.prefix !== undefined && opts.prefix !== null) q.push("prefix=" + encodeURIComponent(String(opts.prefix)));
      if (opts.cursor) q.push("cursor=" + encodeURIComponent(String(opts.cursor)));
      if (opts.limit) q.push("limit=" + encodeURIComponent(Number(opts.limit)));
      var qs = q.length ? "?" + q.join("&") : "";
      return fetch("/api/files" + qs, {
        headers: withAppHeader(null)
      }).then(function (res) {
        if (res.ok) return res.json();
        return res.json().catch(function () { return {}; }).then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
      });
    },
    delete: function (id) {
      return fetch("/api/files/" + encodeURIComponent(String(id)), {
        method: "DELETE",
        headers: withAppHeader(null)
      }).then(function (res) {
        if (res.ok || res.status === 204) return undefined;
        return res.json().catch(function () { return {}; }).then(function (body) {
          var e = (body && body.error) || {};
          throw fail(e.code || "UPSTREAM_ERROR", e.message || res.statusText, e.detail);
        });
      });
    }
  };

  // ---- host.connectWS：返回原生 WebSocket（API 契约 §7）----
  // 浏览器 WS 无法携带自定义头：应用身份经查询参数（服务端 ws-proxy 支持）。

  function connectWS(target) {
    var t = String(target);
    if (t.indexOf("/") === 0) {
      return new WebSocket(t);
    }
    var scheme = t.indexOf("wss://") === 0 ? "wss" : "ws";
    // 同源 ws(s) 代理端点；跨端口场景按当前页面协议
    var base = (location && location.host)
      ? scheme + "://" + location.host
      : scheme + "://localhost";
    var q = "?url=" + encodeURIComponent(t);
    if (appId) q += "&app=" + encodeURIComponent(appId);
    return new WebSocket(base + "/api/ws-proxy" + q);
  }

  window.host = window.host || {};
  window.host.store = store;
  window.host.files = files;
  window.host.fetch = hostFetch;
  window.host.url = hostUrl;
  window.host.connectWS = connectWS;
  // host.user：不透明调用者标识，可为 null（fixed 匿名）或 undefined（宿主未提供）
  if (boot && boot.subject !== undefined) {
    window.host.user = { id: boot.subject === null ? null : String(boot.subject) };
  }
  window.host.__pegboard = {
    appId: appId,
    shim: !boot || boot.shim !== false
  };
  if (window.host.__pegboard.shim) {
    installShim();
  }
})();
