#!/bin/sh
# 改版本号。三处一起改，省得漏：
#
#   1. Cargo.toml 里 [workspace.package] 的 version —— 唯一来源
#   2. Cargo.lock 里七个 pixora-* 包的版本（不改的话下次 cargo build 会顺手改写，
#      提交时容易夹带进别的改动里）
#   3. web/package.json 的 version（前端不发包，纯粹是不想两处对不上）
#
# 用法：
#   sh scripts/bump-version.sh 0.2.0      # 前导 v 会被去掉
#
# 改完推送主干，GitHub 那边的「发行版与镜像」工作流会用这个版本号
# 自动创建标签、发 Release、推镜像。想先本地看一眼用 sh scripts/version.sh。

set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
new=${1:-}

if [ -z "$new" ]; then
  echo "用法：sh scripts/bump-version.sh 0.2.0" >&2
  exit 2
fi
new=${new#v}

old=$(sh "$root/scripts/version.sh")
if [ -z "$old" ]; then
  echo "没读到当前版本号，先看看 Cargo.toml 的 [workspace.package] 段" >&2
  exit 1
fi
if [ "$old" = "$new" ]; then
  echo "版本号已经是 $new，什么都没改"
  exit 0
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

# 1) Cargo.toml：文件里第一处形如 `version = "0.1.0"` 的整行就是 [workspace.package]
#    那一行（依赖清单里的 version 都写在行内的 { } 里，不以 version 开头）
awk -v old="$old" -v new="$new" '
  !done && $0 == "version = \"" old "\"" { print "version = \"" new "\""; done = 1; next }
  { print }
' "$root/Cargo.toml" > "$tmp"
mv "$tmp" "$root/Cargo.toml"

# 2) Cargo.lock：紧跟 name = "pixora-*" 的那行 version
awk -v new="$new" '
  /^name = "pixora-/ { print; local_pkg = 1; next }
  local_pkg && /^version = "/ { print "version = \"" new "\""; local_pkg = 0; next }
  { print }
' "$root/Cargo.lock" > "$tmp"
mv "$tmp" "$root/Cargo.lock"

# 3) web/package.json
sed "s/\"version\": \"$old\"/\"version\": \"$new\"/" "$root/web/package.json" > "$tmp"
mv "$tmp" "$root/web/package.json"

echo "版本号：$old → $new"
echo
echo "接下来："
echo "  git add Cargo.toml Cargo.lock web/package.json"
echo "  git commit -m \"chore: 版本号 $new\""
echo "  git push            # 推上去会自动发版 + 出镜像"
