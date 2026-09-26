#!/usr/bin/env bash
# 依赖方向检查：docs/项目契约.md「依赖方向」。
# 规则：core 不依赖 server/cli/sdk；server 不依赖 cli；sdk 零依赖。
# 另查非测试代码中的 unwrap/expect。反向依赖即失败。
set -euo pipefail

cd "$(dirname "$0")/.."
status=0

check_dep() {
    local crate="$1"
    local dep="$2"
    if grep -Eq "^${dep} *= " "crates/${crate}/Cargo.toml"; then
        echo "FAIL: ${crate} 不允许依赖 ${dep}"
        status=1
    fi
}

# core 不依赖 server / cli / sdk
for dep in pegboard-server pegboard-cli pegboard-sdk; do
    check_dep pegboard-core "${dep}"
done

# server 不依赖 cli
check_dep pegboard-server pegboard-cli

# sdk 零依赖
if sed -n '/\[dependencies\]/,/^\[/p' crates/pegboard-sdk/Cargo.toml | grep -Eq '^[a-zA-Z0-9_-]+ *='; then
    echo "FAIL: pegboard-sdk 必须零依赖"
    status=1
fi

# 源码内不允许 use 反向路径
if grep -rq "pegboard_server\|pegboard_cli" crates/pegboard-core/src; then
    echo "FAIL: core 源码引用了 server/cli"
    status=1
fi
if grep -rq "pegboard_cli" crates/pegboard-server/src; then
    echo "FAIL: server 源码引用了 cli"
    status=1
fi

# 非测试代码禁止 unwrap/expect：只检查每个文件 #[cfg(test)] 之前的部分
while IFS= read -r file; do
    hits=$(awk '/#\[cfg\(test\)\]/{exit} /[\.\( ](unwrap|expect)\(/{print FILENAME":"FNR": "$0}' "${file}")
    if [ -n "${hits}" ]; then
        echo "FAIL: 非测试 unwrap/expect"
        echo "${hits}"
        status=1
    fi
done < <(find crates -path '*/src/*' -name '*.rs')

if [ "${status}" -ne 0 ]; then
    exit 1
fi
echo "deps: OK"
