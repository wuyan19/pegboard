#!/usr/bin/env bash
# 打包与分发（实施文档 7.6 / 8.7）：单二进制 + apps/ + 默认 config.toml。
# 产物：dist/pegboard-<version>/（可直接 tar 分发；升级替换二进制即可）。
# macOS 额外产出 dist/Pegboard.app：双击启动（LSUIElement 无 Dock 图标，托盘常驻），
# 也是在线升级（macOS 整包替换 .app）的发布形态。
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys;print(json.load(sys.stdin)["packages"][0]["version"])')"
OUT="dist/pegboard-${VERSION}"

echo "==> cargo build --release"
cargo build --release -p pegboard-cli

echo "==> 组装 ${OUT}"
rm -rf "${OUT}"
mkdir -p "${OUT}"

cp target/release/pegboard "${OUT}/pegboard"
chmod +x "${OUT}/pegboard"

# 应用目录（示例与用户应用）
cp -R apps "${OUT}/apps"

# 默认配置（拷贝示例）
cp config.example.toml "${OUT}/config.toml"

echo "==> 校验产物可运行"
"${OUT}/pegboard" --version

# ---- macOS .app bundle（双击启动形态；托盘常驻，无 Dock 图标）----
if [[ "$(uname -s)" == "Darwin" ]]; then
  APP="dist/Pegboard.app"
  echo "==> 组装 ${APP}"
  rm -rf "${APP}"
  mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

  cp target/release/pegboard "${APP}/Contents/MacOS/pegboard"
  chmod +x "${APP}/Contents/MacOS/pegboard"

  # 图标：assets/icon.png → icns（需要 iconutil + sips，macOS 自带）
  if command -v iconutil >/dev/null 2>&1 && command -v sips >/dev/null 2>&1 && [[ -f assets/icon.png ]]; then
    ICONSET="dist/Pegboard.iconset"
    rm -rf "${ICONSET}"
    mkdir -p "${ICONSET}"
    for spec in "16 icon_16x16" "32 icon_16x16@2x" "32 icon_32x32" "64 icon_32x32@2x" \
                "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256" "512 icon_256x256@2x" "512 icon_512x512"; do
      sips -z "${spec%% *}" "${spec%% *}" assets/icon.png --out "${ICONSET}/${spec#* }.png" >/dev/null
    done
    sips -z 1024 1024 assets/icon.png --out "${ICONSET}/icon_512x512@2x.png" >/dev/null
    iconutil -c icns "${ICONSET}" -o "${APP}/Contents/Resources/Pegboard.icns"
    rm -rf "${ICONSET}"
  fi

  cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>Pegboard</string>
    <key>CFBundleDisplayName</key>       <string>Pegboard</string>
    <key>CFBundleIdentifier</key>        <string>dev.pegboard.host</string>
    <key>CFBundleVersion</key>           <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
    <key>CFBundleExecutable</key>        <string>pegboard</string>
    <key>CFBundleIconFile</key>          <string>Pegboard</string>
    <key>LSMinimumSystemVersion</key>    <string>11.0</string>
    <!-- 菜单栏应用：无 Dock 图标，双击直接进入托盘 -->
    <key>LSUIElement</key>               <true/>
    <key>NSHighResolutionCapable</key>   <true/>
</dict>
</plist>
PLIST

  # 应用随包：.app 与裸二进制共用一份 apps/（相对 cwd 解析，见 config 说明）
  cp -R apps "${APP}/Contents/MacOS/apps"
  cp config.example.toml "${APP}/Contents/MacOS/config.toml"

  # ad-hoc 签名：本机双击运行即可（对外分发需 Developer ID + 公证）
  codesign --force --deep --sign - "${APP}" 2>/dev/null || true
  echo "==> .app 完成（codesign ad-hoc）"

  # 升级资产形态（在线升级 macOS 资产 = 整包 .app.zip，ditto 保持 bundle 结构）
  ditto -c -k --keepParent "${APP}" "dist/Pegboard.app.zip"
  echo "==> dist/Pegboard.app.zip（在线升级资产）"
fi

echo "==> 完成：${OUT}"
ls -lh "${OUT}"
[[ -d dist/Pegboard.app ]] && ls -lh dist/Pegboard.app/Contents
