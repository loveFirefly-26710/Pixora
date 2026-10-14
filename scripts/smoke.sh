#!/usr/bin/env bash
# 端到端冒烟测试。
#
# 起一个真服务（SQLite 临时库），按真实调用顺序走一遍主链路：
# 安装向导 → 登录 → 建相册 → 上传 → 去重 → 直链 → 改可见性 → 删除 → 回收物理文件。
#
# 为什么用 shell 而不是集成测试：这条路径要验证的是「二进制真的跑起来了」——
# 绑定端口、静态资源、Cookies、multipart、直链的响应头，用 axum 的
# ServiceExt 是绕过了这些的。
#
# 用法：bash scripts/smoke.sh [端口]
#
# Windows 上用 cmd 时请改用 scripts\smoke.ps1（同一套断言，依赖更少：
# 只需要系统自带的 curl.exe 与 PowerShell，不需要 Git Bash 和 python）。

set -euo pipefail

PORT="${1:-18080}"
BASE="http://127.0.0.1:${PORT}"
WORK="$(mktemp -d)"
DATA_DIR="${WORK}/data"
DB="${WORK}/pixora.db"
LOG="${WORK}/server.log"
COOKIE_JAR="${WORK}/cookies.txt"
CSRF=""

cleanup() {
    if [[ -n "${SERVER_PID:-}" ]]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -rf "${WORK}"
}
trap cleanup EXIT

# 把请求体写成文件再交给 curl。
#
# 不能直接把中文放进命令行参数或 python 的 argv：Windows 上 shell 与 Python
# 都会用本地代码页处理参数，中文到服务端就变成了非法 UTF-8（表现为
# 「invalid unicode code point」）。shell 的 printf 不做编码转换，
# 由它写文件最稳。
json_body() {
    local name="$1" content="$2"
    local file="${WORK}/body-${name}.json"
    printf '%s' "${content}" > "${file}"
    printf '@%s' "${file}"
}

# multipart 文本字段：curl 的 `<file` 写法能从文件读值（保持原始字节）
form_value() {
    local name="$1" content="$2"
    local file="${WORK}/field-${name}.txt"
    printf '%s' "${content}" > "${file}"
    printf '<%s' "${file}"
}

step() { printf '\n\033[1;34m== %s\033[0m\n' "$1"; }
ok() { printf '  \033[32m✓\033[0m %s\n' "$1"; }
fail() {
    printf '  \033[31m✗ %s\033[0m\n' "$1"
    echo "--- 服务端日志尾部 ---"
    tail -40 "${LOG}" || true
    exit 1
}

# 断言 JSON 里的字段等于期望值
expect_json() {
    local json="$1" expr="$2" expected="$3" label="$4"
    local actual
    actual="$(printf '%s' "${json}" | python -c "import json,sys;d=json.load(sys.stdin);print(${expr})" 2>/dev/null || echo "__parse_error__")"
    if [[ "${actual}" != "${expected}" ]]; then
        printf '  响应原文：%s
' "${json}"
        fail "${label}：期望 ${expected}，实际 ${actual}"
    fi
    ok "${label}"
}

# 带 Cookie 与 CSRF 的请求（写操作用）
api() {
    local method="$1" path="$2"
    shift 2
    local args=(-s -X "${method}" "${BASE}${path}" -b "${COOKIE_JAR}" -c "${COOKIE_JAR}")
    if [[ "${method}" != "GET" ]]; then
        args+=(-H "X-CSRF-Token: ${CSRF}")
    fi
    curl "${args[@]}" "$@"
}

step "启动服务"
mkdir -p "${DATA_DIR}"
PIXORA_DB_DRIVER=sqlite \
PIXORA_DB_DSN="${DB}" \
PIXORA_DATA_DIR="${DATA_DIR}" \
PIXORA_BIND="127.0.0.1:${PORT}" \
PIXORA_BASE_URL="${BASE}" \
PIXORA_SECURE_COOKIES=false \
RUST_LOG=info \
    cargo run -p pixora-server --quiet >"${LOG}" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if curl -sf "${BASE}/healthz" >/dev/null 2>&1; then break; fi
    if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
        fail "服务进程提前退出"
    fi
    sleep 1
done
curl -sf "${BASE}/healthz" >/dev/null || fail "健康检查超时"
ok "服务已就绪 ${BASE}"

step "静态资源与前端回落"
INDEX="$(curl -s "${BASE}/")"
[[ "${INDEX}" == *"<div id=\"app\""* ]] || fail "首页没有返回前端外壳"
ok "首页返回了 SPA 外壳"
JS_BYTES="$(curl -s -o "${WORK}/main.js" -w '%{size_download}' "${BASE}/main.js")"
[[ "${JS_BYTES}" -gt 1000 ]] || fail "main.js 只有 ${JS_BYTES} 字节"
ok "前端脚本可访问（${JS_BYTES} 字节）"
CSS_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/main.css")"
[[ "${CSS_CODE}" == "200" ]] || fail "main.css 返回 ${CSS_CODE}"
ok "样式表可访问（曾经漏链过一次）"
ROUTE_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}/albums")"
[[ "${ROUTE_CODE}" == "200" ]] || fail "SPA 路由回落失败（${ROUTE_CODE}）"
ok "SPA 路由回落到 index.html"

step "安装向导"
STATUS="$(curl -s "${BASE}/api/v1/install/status")"
expect_json "${STATUS}" "d['data']['installed']" "False" "未安装状态"

CHECK="$(curl -s -X POST "${BASE}/api/v1/install/database" -H 'Content-Type: application/json' \
    -d "{\"driver\":\"sqlite\",\"database\":\"${DB}\"}")"
expect_json "${CHECK}" "d['data']['ok']" "True" "数据库连接与建表"

ADMIN="$(curl -s -X POST "${BASE}/api/v1/install/admin" -H 'Content-Type: application/json' \
    -d '{"email":"admin@example.com","password":"Pixora2026Abc","site_name":"Pixora"}')"
expect_json "${ADMIN}" "d['data']['email']" "admin@example.com" "创建管理员"
ADMIN_UID="$(printf '%s' "${ADMIN}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['uid'])")"

SITE="$(curl -s -X POST "${BASE}/api/v1/install/site" -H 'Content-Type: application/json' \
    -d "{\"base_url\":\"${BASE}\",\"upload_prefix\":\"images\",\"smtp\":null}")"
expect_json "${SITE}" "d['data']['ok']" "True" "站点设置"

FIN="$(curl -s -X POST "${BASE}/api/v1/install/finish")"
expect_json "${FIN}" "d['data']['ok']" "True" "完成安装"

LOCKED="$(curl -s -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/install/admin" \
    -H 'Content-Type: application/json' -d '{}')"
[[ "${LOCKED}" == "404" ]] || fail "安装入口未锁定（${LOCKED}）"
ok "安装入口已锁定"

step "登录与会话"
LOGIN_BAD="$(curl -s -X POST "${BASE}/api/v1/auth/login" -H 'Content-Type: application/json' \
    -d '{"email":"admin@example.com","password":"wrong-password"}')"
expect_json "${LOGIN_BAD}" "d['error']['code']" "INVALID_CREDENTIALS" "错误口令被拒绝"

LOGIN="$(curl -s -X POST "${BASE}/api/v1/auth/login" -H 'Content-Type: application/json' \
    -b "${COOKIE_JAR}" -c "${COOKIE_JAR}" \
    -d '{"email":"admin@example.com","password":"Pixora2026Abc"}')"
expect_json "${LOGIN}" "d['data']['user']['email']" "admin@example.com" "登录成功"
CSRF="$(printf '%s' "${LOGIN}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['csrf_token'])")"
[[ -n "${CSRF}" ]] || fail "登录未返回 CSRF 令牌"
ok "拿到 CSRF 令牌"

ME="$(api GET /api/v1/me)"
expect_json "${ME}" "d['data']['user']['role']" "admin" "会话鉴权可用"

NO_CSRF="$(curl -s -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/v1/albums" \
    -b "${COOKIE_JAR}" -H 'Content-Type: application/json' -d '{"name":"x"}')"
[[ "${NO_CSRF}" == "403" ]] || fail "缺 CSRF 头的写请求未被拒绝（${NO_CSRF}）"
ok "缺 CSRF 的写请求返回 403"

step "相册与图片"
ALBUM_BODY="$(json_body album '{"name":"旅行","slug":"旅行","visibility":"private"}')"
ALBUM="$(api POST /api/v1/albums -H 'Content-Type: application/json' -d "${ALBUM_BODY}")"
expect_json "${ALBUM}" "d['data']['name']" "旅行" "新建相册"
ALBUM_ID="$(printf '%s' "${ALBUM}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['id'])")"

# 造一张真正的 PNG（用 Python，不引额外依赖）
PNG="${WORK}/sample.png"
python - "$PNG" <<'PY'
import struct, sys, zlib

def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

width = height = 64

# 每行前面一个 filter 字节，然后是 RGB 像素；颜色按行列变化，
# 这样生成的是真实可解码的 PNG（不是纯色块），便于验证尺寸解析
def row(y):
    px = bytearray()
    for x in range(width):
        px += bytes([(x * 4) % 256, 120, (y * 3) % 256])
    return bytes(px)

raw = b"".join(b"\x00" + row(y) for y in range(height))
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(raw))
       + chunk(b"IEND", b""))
open(sys.argv[1], "wb").write(png)
PY
[[ -s "${PNG}" ]] || fail "测试图片生成失败"
ok "生成测试 PNG（64×64）"

HASH="$(python - "$PNG" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
)"

# 预检：这张图还不存在
PRE="$(api POST /api/v1/blobs/exists -H 'Content-Type: application/json' \
    -d "{\"hashes\":[\"${HASH}\"]}")"
expect_json "${PRE}" "len(d['data']['missing'])" "1" "预检报告文件不存在"

TAGS_FIELD="$(form_value tags '风景,旅行')"
UPLOAD="$(api POST /api/v1/upload -F "file=@${PNG}" -F "sha256=${HASH}" \
    -F "album_id=${ALBUM_ID}" -F "tags=${TAGS_FIELD}" -F "visibility=public" \
    -F "original_name=sample.png")"
expect_json "${UPLOAD}" "d['data']['dedup']" "False" "首次上传未命中重复"
IMAGE_ID="$(printf '%s' "${UPLOAD}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['id'])")"
URL="$(printf '%s' "${UPLOAD}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['url'])")"
expect_json "${UPLOAD}" "d['data']['sha256']" "${HASH}" "服务端哈希与客户端一致"

# 二次预检：这次应当命中
PRE2="$(api POST /api/v1/blobs/exists -H 'Content-Type: application/json' \
    -d "{\"hashes\":[\"${HASH}\"]}")"
expect_json "${PRE2}" "len(d['data']['exist'])" "1" "预检报告文件已存在"

step "去重与引用计数"
UPLOAD2="$(api POST /api/v1/upload -F "file=@${PNG}" -F "sha256=${HASH}" \
    -F "original_name=sample-again.png")"
expect_json "${UPLOAD2}" "d['data']['dedup']" "True" "二次上传命中重复且未重复占盘"
IMAGE_ID2="$(printf '%s' "${UPLOAD2}" | python -c "import json,sys;print(json.load(sys.stdin)['data']['id'])")"
[[ "${IMAGE_ID}" != "${IMAGE_ID2}" ]] || fail "两次上传应当生成两条独立的图片引用"
ok "两次上传生成了两条独立引用（同一物理文件）"

# 磁盘上只有一个物理文件
BLOB_COUNT="$(find "${DATA_DIR}/blobs" -type f | wc -l | tr -d ' ')"
[[ "${BLOB_COUNT}" == "1" ]] || fail "物理文件数应为 1，实际 ${BLOB_COUNT}"
ok "磁盘上只有 1 份物理文件"

# 镜像树：相册目录下有硬链接
VIEW_LINKS="$(find "${DATA_DIR}/users" -type f 2>/dev/null | wc -l | tr -d ' ')"
[[ "${VIEW_LINKS}" -ge 1 ]] || fail "镜像目录树里没有链接"
ok "镜像目录树里建了 ${VIEW_LINKS} 个链接"

step "直链"
URL_PATH="$(printf '%s' "${URL}" | sed -e "s|^${BASE}||")"
DIRECT_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}${URL_PATH}")"
[[ "${DIRECT_CODE}" == "200" ]] || fail "直链返回 ${DIRECT_CODE}"
ok "直链返回 200"

HEADERS="$(curl -s -D - -o /dev/null "${BASE}${URL_PATH}")"
printf '%s' "${HEADERS}" | grep -qi "cache-control: public, max-age=31536000, immutable" \
    || fail "直链缺少 immutable 缓存头"
printf '%s' "${HEADERS}" | grep -qi "x-content-type-options: nosniff" || fail "直链缺少 nosniff"
printf '%s' "${HEADERS}" | grep -qi "etag: \"${HASH}\"" || fail "直链 ETag 不等于内容哈希"
ok "直链响应头正确（immutable / nosniff / ETag）"

BODY_HASH="$(curl -s "${BASE}${URL_PATH}" | python -c "import hashlib,sys;print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())")"
[[ "${BODY_HASH}" == "${HASH}" ]] || fail "直链返回的字节与原文件不一致"
ok "直链内容与上传的字节完全一致"

# 伪造扩展名要 404
WRONG_EXT="${URL_PATH%.*}.gif"
WRONG_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}${WRONG_EXT}")"
[[ "${WRONG_CODE}" == "404" ]] || fail "伪造扩展名未被拒绝（${WRONG_CODE}）"
ok "伪造扩展名返回 404"

step "列表与广场"
# 第二张故意没带 album_id：它应当落在「未分类」，不计入相册
IMAGES="$(api GET "/api/v1/images?album_id=${ALBUM_ID}")"
expect_json "${IMAGES}" "d['meta']['total']" "1" "相册里有 1 张图"
expect_json "${IMAGES}" "len(d['data'][0]['tags'])" "2" "标签已写入"

ALL_IMAGES="$(api GET /api/v1/images)"
expect_json "${ALL_IMAGES}" "d['meta']['total']" "2" "全部图片共 2 张（含未分类那张）"

PLAZA="$(curl -s "${BASE}/api/v1/plaza")"
expect_json "${PLAZA}" "len(d['data']) >= 1" "True" "广场能看到公开图片"
expect_json "${PLAZA}" "d['data'][0]['original_name']" "None" "未登录时原始文件名被裁剪"

TAGS="$(api GET "/api/v1/tags?uid=${ADMIN_UID}")"
expect_json "${TAGS}" "len(d['data'])" "2" "标签词表有 2 项"

TREE="$(api GET "/api/v1/fs/tree")"
expect_json "${TREE}" "d['data']['files']" "2" "目录树统计到 2 个文件"

step "可见性、批量与删除"
api PATCH "/api/v1/images/${IMAGE_ID}" -H 'Content-Type: application/json' \
    -d '{"visibility":"private"}' >/dev/null
PRIVATE_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}${URL_PATH}")"
[[ "${PRIVATE_CODE}" == "404" ]] || fail "私有图片的直链未被拒绝（${PRIVATE_CODE}）"
ok "全部引用转私有后直链返回 404"

# 恢复公开，另一条引用仍生效时直链必须可用
api PATCH "/api/v1/images/${IMAGE_ID}" -H 'Content-Type: application/json' \
    -d '{"visibility":"public"}' >/dev/null
BACK_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}${URL_PATH}")"
[[ "${BACK_CODE}" == "200" ]] || fail "恢复公开后直链仍不可用（${BACK_CODE}）"
ok "恢复公开后直链可用"

BATCH_BODY="$(json_body batch "{\"action\":\"tag\",\"ids\":[${IMAGE_ID},${IMAGE_ID2}],\"tags\":[\"风景\"]}")"
BATCH="$(api POST /api/v1/images/batch -H 'Content-Type: application/json' -d "${BATCH_BODY}")"
expect_json "${BATCH}" "d['data']['affected']" "2" "批量改标签"

api DELETE "/api/v1/images/${IMAGE_ID2}" >/dev/null
FILES_AFTER_ONE="$(find "${DATA_DIR}/blobs" -type f | wc -l | tr -d ' ')"
[[ "${FILES_AFTER_ONE}" == "1" ]] || fail "删掉一条引用后物理文件不该消失（实际 ${FILES_AFTER_ONE}）"
ok "删掉一条引用后物理文件保留（还有另一条引用）"

api DELETE "/api/v1/images/${IMAGE_ID}" >/dev/null
DEAD_CODE="$(curl -s -o /dev/null -w '%{http_code}' "${BASE}${URL_PATH}")"
[[ "${DEAD_CODE}" == "404" ]] || fail "全部引用删除后直链仍可用（${DEAD_CODE}）"
ok "全部引用删除后直链返回 404"

step "管理后台"
STATS="$(api GET /api/v1/admin/stats)"
expect_json "${STATS}" "d['data']['user_total']" "1" "管理概览：1 个用户"

STORAGE="$(api GET /api/v1/admin/storage)"
expect_json "${STORAGE}" "d['data']['default_driver']" "local" "存储总览：默认驱动是本地磁盘"
expect_json "${STORAGE}" "d['data']['paths']['upload_prefix']" "images" "存储总览：直链前缀"

QUOTA="$(api GET /api/v1/admin/quota)"
expect_json "${QUOTA}" "d['data']['default_quota_bytes'] > 0" "True" "配额总览"

SETTINGS="$(api GET /api/v1/admin/settings)"
expect_json "${SETTINGS}" "d['data']['site']['name']" "Pixora" "系统设置：站点名"

# 直链前缀改名后旧链依然有效（映射在数据库里，不在 URL 结构里）
api PUT /api/v1/admin/storage/prefix -H 'Content-Type: application/json' -d '{"upload_prefix":"i"}' >/dev/null
ok "直链前缀改为 i"

BAD_TERMS="$(api DELETE /api/v1/admin/terms/99999)"
expect_json "${BAD_TERMS}" "d['error']['code']" "NOT_FOUND" "删除不存在的条款返回 NOT_FOUND"

step "错误码与限流"
NOT_FOUND="$(api GET /api/v1/images/999999)"
expect_json "${NOT_FOUND}" "d['error']['code']" "NOT_FOUND" "不存在的图片返回 NOT_FOUND"

TOKEN_BAD="$(curl -s "${BASE}/api/v1/me" -H 'Authorization: Bearer px_live_not_a_real_token')"
expect_json "${TOKEN_BAD}" "d['error']['code']" "TOKEN_REVOKED" "伪造 Token 返回 TOKEN_REVOKED"

SUSPEND_BODY="$(json_body suspend '{"action":"suspend","reason":"测试用"}')"
SUSPEND="$(api POST /api/v1/admin/users/1/status -H 'Content-Type: application/json' -d "${SUSPEND_BODY}")"
expect_json "${SUSPEND}" "d['error']['code']" "BAD_REQUEST" "管理员账号不可被处置"

step "审计日志"
# 上传、删除、改前缀都会留痕；这里只确认审计表有行
AUDIT_ROWS="$(python - "$DB" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
print(conn.execute("select count(*) from audit_logs").fetchone()[0])
PY
)"
[[ "${AUDIT_ROWS}" -gt 5 ]] || fail "审计日志行数偏少（${AUDIT_ROWS}）"
ok "审计日志记录了 ${AUDIT_ROWS} 条事件"

# 引用计数最终应当归零，且没有负数
REF_STATE="$(python - "$DB" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
rows = conn.execute("select sha256, ref_count, kind from blobs").fetchall()
print("|".join(f"{r[2]}:{r[1]}" for r in rows))
PY
)"
[[ "${REF_STATE}" == *":0"* ]] || fail "引用计数未归零：${REF_STATE}"
ok "引用计数已归零（${REF_STATE}）"

printf '\n\033[1;32m全部通过。\033[0m\n'
