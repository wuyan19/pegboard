#!/usr/bin/env bash
# 打包与分发（实施文档 7.6）：单二进制 + apps/ + 默认 config.toml。
# 产物：dist/pegboard-<version>/（可直接 tar 分发；升级替换二进制即可）。
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
echo "==> 完成：${OUT}"
ls -lh "${OUT}"
