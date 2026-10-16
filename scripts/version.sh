#!/bin/sh
# 打印当前版本号。
#
# 唯一来源是 Cargo.toml 里 [workspace.package] 的 version——二进制启动日志里那句
# version="0.1.0" 就是它，发版与镜像标签也都按它算。改版本号请用
# scripts/bump-version.sh，别手改各处。
#
# 用法：
#   sh scripts/version.sh          # 0.1.0

set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
  | sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' \
  | head -1
