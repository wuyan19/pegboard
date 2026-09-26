// pegboard sdk.js — 占位版本（M2）。
// 完整 host 命名空间（store/files/fetch/connectWS/url/user）随 M3-M6 逐步提供。
// 注入约定：宿主在本脚本之前注入
//   <script>window.__PEGBOARD__ = { appId: "...", shim: true }</script>
(function () {
  "use strict";
  var boot = typeof window !== "undefined" ? window.__PEGBOARD__ : null;
  window.host = window.host || {};
  window.host.__pegboard = {
    appId: boot && boot.appId ? String(boot.appId) : null,
    shim: !boot || boot.shim !== false
  };
})();
