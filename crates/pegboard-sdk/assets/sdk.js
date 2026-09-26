// pegboard sdk.js — M4：host.store + host.fetch + fetch shim + host.url。
// connectWS / files 随 M5-M6 提供。
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

  window.host = window.host || {};
  window.host.store = store;
  window.host.fetch = hostFetch;
  window.host.url = hostUrl;
  window.host.__pegboard = {
    appId: appId,
    shim: !boot || boot.shim !== false
  };
  if (window.host.__pegboard.shim) {
    installShim();
  }
})();
