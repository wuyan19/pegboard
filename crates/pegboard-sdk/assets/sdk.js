// pegboard sdk.js — M3：host.store（KV 原语）。
// fetch / connectWS / files / url 随 M4-M6 提供。
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

  window.host = window.host || {};
  window.host.store = store;
  window.host.__pegboard = {
    appId: appId,
    shim: !boot || boot.shim !== false
  };
})();
