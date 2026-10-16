#!/usr/bin/env bash
# 端到端冒烟：只覆盖这次改动涉及的接口。
# 与 scripts/smoke.sh 的分工：那个跑全量体检，这个跑「这次改了什么就验什么」。
#
# 用法：先起服务，再跑本脚本。
#   PIXORA_DB_DRIVER=sqlite PIXORA_DB_DSN=./target/smoke/pixora.db \
#   PIXORA_DATA_DIR=./target/smoke PIXORA_BIND=127.0.0.1:8099 \
#   PIXORA_BASE_URL=http://127.0.0.1:8099 \
#   PIXORA_WEB_DIR=./web/dist ./target/debug/pixora-server
#
# 一个 Windows 上的坑：**带中文的请求体必须走文件**。
# git-bash 把非 ASCII 参数交给原生 curl.exe 时会被按本地代码页转换，
# 服务端收到的就是坏掉的 UTF-8（报 invalid unicode code point）。
# 所以下面用 `body <名字>` 从 stdin 写文件，再用 `post <名字>` 发出去。
set -u

BASE="http://127.0.0.1:8099/api/v1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/target/smoke"
JAR="$DIR/cookies.txt"
PASS=0
FAIL=0

say() { printf '\n=== %s ===\n' "$1"; }
ok() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad() {
  FAIL=$((FAIL + 1))
  printf '  FAIL %s\n     %s\n' "$1" "$2"
}

# $1 描述 $2 期望子串 $3 实际输出
expect() {
  case "$3" in
    *"$2"*) ok "$1" ;;
    *) bad "$1" "期望包含 [$2]，实际：$(printf '%s' "$3" | head -c 300)" ;;
  esac
}

csrf() {
  # 双提交 Cookie：请求头里要带上与 Cookie 同值的那一份
  grep -o 'pixora_csrf[[:space:]]*[^[:space:]]*' "$JAR" | awk '{print $NF}' | tail -1
}

# 把 stdin 存成请求体文件（中文只能这么传，见文件头说明）
body() { cat > "$DIR/$1"; }

# post METHOD PATH BODY文件
post() {
  curl -s -b "$JAR" -c "$JAR" -X "$1" "$BASE$2" \
    -H 'Content-Type: application/json' -H "X-CSRF-Token: $(csrf)" \
    --data-binary "@$DIR/$3"
}

# req METHOD PATH [ASCII JSON]
req() {
  local method="$1" path="$2" data="${3:-}"
  if [ -n "$data" ]; then
    curl -s -b "$JAR" -c "$JAR" -X "$method" "$BASE$path" \
      -H 'Content-Type: application/json' -H "X-CSRF-Token: $(csrf)" --data "$data"
  else
    curl -s -b "$JAR" -c "$JAR" -X "$method" "$BASE$path" -H "X-CSRF-Token: $(csrf)"
  fi
}

# 取信封里 data 下的字段：jq_get "['user']['used_bytes']"
jq_get() {
  python -c "import json,sys; d=json.load(sys.stdin)['data']; print(eval('d' + sys.argv[1]))" "$1" 2>/dev/null
}

mkdir -p "$DIR"
rm -f "$JAR"

say "安装向导"
curl -s -c "$JAR" "$BASE/site" > /dev/null
body admin.json <<'JSON'
{"email":"admin@pixora.test","password":"Smoke-Test-2026x","site_name":"Smoke 测试站"}
JSON
body site.json <<'JSON'
{"base_url":"http://127.0.0.1:8099","plaza_public":true}
JSON

out=$(req POST /install/database '{"driver":"sqlite"}')
expect "数据库步骤" '"ok":true' "$out"
out=$(post POST /install/admin admin.json)
expect "管理员步骤（带站点名）" 'admin@pixora.test' "$out"
out=$(post POST /install/site site.json)
expect "站点步骤" '"ok":true' "$out"
out=$(req POST /install/finish '{}')
expect "完成安装" '"login_url"' "$out"

say "登录与站点信息"
out=$(post POST /auth/login admin.json)
expect "登录成功" 'admin@pixora.test' "$out"
expect "返回 impersonator 字段" '"impersonator":null' "$out"
out=$(curl -s "$BASE/site")
expect "站点名跟着设置走" 'Smoke 测试站' "$out"
expect "下发长宽上限" '"max_width"' "$out"
expect "下发队列深度" '"queue_depth"' "$out"
expect "下发会话有效期" '"idle_minutes"' "$out"

say "收件域名校验：MX 查到就拒（null MX 的域名收不到信）"
# 这一关默认开着。冒烟里的地址都是假域名，先单独验一次它的判定，
# 后面统一关掉——跑冒烟不该依赖外网 DNS 通不通
say "暂停注册：对外只说暂停，开关与自动关闭状态一致"
# 容量自动关闭会把系统设置里的注册开关一起翻成关闭（管理员看到的必须与真实状态一致）。
# 这条不变式在任何磁盘占用下都成立，所以在这里断言它而不是断言具体占用率
out=$(req GET /admin/settings)
AUTO_CLOSED=$(printf '%s' "$out" | jq_get "['site']['register_auto_closed']")
REG_ENABLED=$(printf '%s' "$out" | jq_get "['site']['register_enabled']")
if [ "$AUTO_CLOSED" = "True" ] && [ "$REG_ENABLED" = "False" ]; then
  ok "自动关闭时注册开关同步为关闭"
elif [ "$AUTO_CLOSED" = "False" ] && [ "$REG_ENABLED" = "True" ]; then
  ok "没有自动关闭时注册开关保持打开"
else
  bad "注册开关与自动关闭状态不一致" "auto_closed=$AUTO_CLOSED enabled=$REG_ENABLED"
fi
out=$(curl -s "$BASE/site")
CLOSED_REASON=$(printf '%s' "$out" | python -c "import json,sys; print(json.load(sys.stdin)['data'].get('registration_closed_reason') or '')")
if [ -z "$CLOSED_REASON" ]; then
  ok "注册开着时不带关闭原因"
else
  case "$CLOSED_REASON" in
    *暂停*时间待定*) ok "对外只说暂停与时间待定" ;;
    *) bad "对外不该带具体口径" "$CLOSED_REASON" ;;
  esac
  case "$CLOSED_REASON" in
    *占用*|*阈值*|*%) bad "对外不该出现占用率或阈值" "$CLOSED_REASON" ;;
    *) ok "对外不带占用率与阈值" ;;
  esac
fi

say "管理员手动打开注册：自动关闭不再抢开关"
out=$(req PUT /admin/settings '{"site":{"register_enabled":true}}')
expect "手动打开注册" '"ok":true' "$out"
out=$(req GET /admin/settings)
expect "注册开关已是打开" '"register_enabled":true' "$out"
expect "覆盖标记立起来了" '"register_auto_close_override":true' "$out"
if printf '%s' "$out" | grep -q '"register_auto_closed":true'; then
  bad "手动打开后不该还被闩锁关着" "$out"
else
  ok "闩锁已解除"
fi
out=$(curl -s "$BASE/site")
expect "站点信息里注册已开放" '"registration_open":true' "$out"

# 这台机器的磁盘常年九成满，注册入口会被容量阈值自动关掉；这一节要验的是
# 收件域名那一关，先把容量自动关闭关掉，免得它先拦下来（顺带也验了这条开关）
out=$(req PUT /admin/quota '{"auto_close_enabled":false}')
expect "关掉容量触顶自动关闭（保存回传整页生效值）" '"capacity"' "$out"
out=$(req POST /auth/register/code '{"email":"someone@example.com"}')
if printf '%s' "$out" | grep -q '不收邮件\|判断不了\|解析'; then
  ok "null MX / 查不出来的域名被拦在发信之前"
else
  bad "收不到信的域名该被拦下" "$(printf '%s' "$out" | head -c 200)"
fi
out=$(req PUT /admin/settings '{"mail":{"mx_check":false}}')
expect "关掉收件域名校验" '"ok":true' "$out"

say "系统设置：新分组"
out=$(req GET /admin/settings)
expect "有会话分组" '"session"' "$out"
expect "有防盗链分组" '"hotlink_enabled"' "$out"
expect "有内容审核分组" '"moderation"' "$out"
expect "格式清单里有 JPG" '"label":"JPG"' "$out"
expect "有 APNG（以前漏的）" '"ext":"apng"' "$out"
body settings.json <<'JSON'
{"upload":{"max_width":4096,"max_height":4096,"queue_depth":3},
 "session":{"idle_minutes":30,"absolute_days":7,"remember_days":14},
 "security":{"hotlink_enabled":false,"hotlink_allow_empty":true,"hotlink_domains":"allowed.example.com"},
 "moderation":{"enabled":true,"provider":"nsfwjs","endpoint":"http://127.0.0.1:9/none","threshold":70,
               "score_field":"nsfw","fail_open":true,"user_enabled":true,"admin_enabled":false,
               "ban_after":2,"ban_minutes":60,"notify_admin":false}}
JSON
out=$(post PUT /admin/settings settings.json)
expect "保存设置" '"ok":true' "$out"
out=$(req GET /admin/settings)
expect "长宽已落库" '"max_width":4096' "$out"
expect "队列深度已落库" '"queue_depth":3' "$out"
expect "闲置超时已落库" '"idle_minutes":30' "$out"
expect "审核开关已落库" '"enabled":true' "$out"

say "背景图：上传进库并按本站地址访问"
python "$ROOT/scripts/make-test-png.py" "$DIR/bg.png" 16
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/admin/settings/background" \
  -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/bg.png")
expect "上传背景图" '"ok":true' "$out"
out=$(curl -s "$BASE/site")
expect "背景图地址是本站的" '/api/v1/site/background?v=' "$out"
code=$(curl -s -o "$DIR/bg-out.png" -w '%{http_code}' "$BASE/site/background")
expect "背景图可访问" '200' "$code"
# 用十六进制比头四个字节：PNG 的魔数第一位是 0x89，直接放进 shell 字符串会被编码问题干扰
expect "返回的是 PNG" '89504e47' "$(xxd -p "$DIR/bg-out.png" | head -c 8)"
code=$(curl -s -o /dev/null -w '%{http_code}' -H 'If-None-Match: "deadbeef"' "$BASE/site/background")
[ "$code" = "200" ] && ok "ETag 不匹配时正常返回（200）" || bad "ETag 不匹配该返回 200" "code=$code"

say "服务条款：新建、重复版本号、生效版本保护、历史版本修订"
body t1.json <<'JSON'
{"version":"v1.0","title":"服务条款","body":"第一条 冒烟测试条款。","status":"active"}
JSON
out=$(post POST /admin/terms t1.json)
expect "新建并发布 v1.0" '"version":"v1.0"' "$out"
expect "列表带回正文" '第一条 冒烟测试条款' "$out"
TERMS_ID=$(printf '%s' "$out" | jq_get "['id']")

body t2.json <<'JSON'
{"version":"v1.0","title":"服务条款","body":"重复版本号","status":"draft"}
JSON
out=$(post POST /admin/terms t2.json)
expect "重复版本号给友好冲突" '已存在' "$out"

body t3.json <<'JSON'
{"version":"v1.0","body":"试图改生效版本"}
JSON
out=$(post PATCH "/admin/terms/$TERMS_ID" t3.json)
expect "生效版本拒绝就地改" '另存为新版本' "$out"

body t4.json <<'JSON'
{"version":"v0.9","title":"服务条款","body":"旧版正文","status":"draft"}
JSON
out=$(post POST /admin/terms t4.json)
expect "新建 v0.9 草稿" '"version":"v0.9"' "$out"
DRAFT_ID=$(printf '%s' "$out" | jq_get "['id']")

body t5.json <<'JSON'
{"version":"v0.9","body":"历史版本也能修订"}
JSON
out=$(post PATCH "/admin/terms/$DRAFT_ID" t5.json)
expect "历史版本可就地改" '历史版本也能修订' "$out"

say "频率限制：两个保存路径都可用"
out=$(req PUT /admin/ratelimit/global '{"api_per_minute":150}')
expect "别名路径 /admin/ratelimit/global" '"ok":true' "$out"
out=$(req PUT /admin/ratelimit '{"api_per_minute":160,"upload_per_minute":40}')
expect "主路径 /admin/ratelimit" '"ok":true' "$out"
out=$(req GET /admin/ratelimit)
expect "全局值已更新" '"api_per_minute":160' "$out"

say "单批次最大张数：批量预检按它卡条数"
# 内容随便，预检只校验形状（64 位十六进制）
hashes() {
  python -c "print(','.join('\"%064x\"' % i for i in range($1)))"
}
# 队列深度在上面被调成了 3，这里同时抬高它，让「单批上限」成为唯一绑定的那条；
# 跑完再放回 3，后面的用例看到的仍是原来那份设置
out=$(req PUT /admin/settings '{"upload":{"max_batch_size":5,"queue_depth":1000}}')
expect "单批上限调到 5" '"ok":true' "$out"
out=$(req POST /blobs/exists "{\"hashes\":[$(hashes 6)]}")
expect "6 个哈希超过单批上限 5，被拒" '单批次上限 5 张' "$out"
out=$(req POST /blobs/exists "{\"hashes\":[$(hashes 4)]}")
expect "4 个哈希在限额内放行" '"missing"' "$out"
out=$(req PUT /admin/settings '{"upload":{"max_batch_size":1000,"queue_depth":3}}')
expect "单批上限改回 1000" '"ok":true' "$out"

say "Token：过期挡位与硬删除"
body tok.json <<'JSON'
{"name":"冒烟令牌","scopes":["upload"],"expires_in_days":90}
JSON
out=$(post POST /me/tokens tok.json)
expect "建令牌" '"token"' "$out"
TOKEN_ID=$(printf '%s' "$out" | jq_get "['id']")
out=$(req DELETE "/me/tokens/$TOKEN_ID?hard=true")
expect "硬删除生效" '"deleted":1' "$out"
out=$(req GET /me/tokens)
case "$out" in
  *冒烟令牌*) bad "删除后列表里不该还有它" "$out" ;;
  *) ok "删除后列表里没有它" ;;
esac

say "Token 改名"
body tok2.json <<'JSON'
{"name":"改名前的名字","scopes":["upload"]}
JSON
out=$(post POST /me/tokens tok2.json)
RENAME_ID=$(printf '%s' "$out" | jq_get "['id']")
body tok3.json <<'JSON'
{"name":"改名后的名字"}
JSON
out=$(post PATCH "/me/tokens/$RENAME_ID" tok3.json)
expect "改名成功" '改名后的名字' "$out"
out=$(req GET /me/tokens)
expect "列表里是新名字" '改名后的名字' "$out"
case "$out" in
  *改名前的名字*) bad "旧名字不该还在" "$out" ;;
  *) ok "旧名字已经不在列表里" ;;
esac
req DELETE "/me/tokens/$RENAME_ID?hard=true" > /dev/null

say "内容审核：服务不可用按 fail_open 放行"
python "$ROOT/scripts/make-test-png.py" "$DIR/up.png" 32
printf '冒烟' > "$DIR/tags.txt"
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/up.png" -F "visibility=public" -F "original_name=smoke.png" -F "tags=<$DIR/tags.txt")
expect "审核不可用仍能上传" '"url"' "$out"
IMAGE_ID=$(printf '%s' "$out" | jq_get "['id']")
expect "标签是中文也没问题" '冒烟' "$out"

say "存储落点：换到数据库内驱动，字节进库、不落磁盘"
before_files=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
body backend.json <<'JSON'
{"driver":"database","label":"库内测试"}
JSON
out=$(post POST /admin/storage/backends backend.json)
BACKEND_ID=$(printf '%s' "$out" | jq_get "['id']")
if [ -z "$BACKEND_ID" ]; then
  bad "建数据库后端" "$out"
else
  ok "建数据库后端成功（id=$BACKEND_ID）"
  out=$(req PUT /admin/storage/default "{\"driver\":\"database\",\"backend_id\":$BACKEND_ID}")
  expect "切默认后端到数据库内" '"ok":true' "$out"

  # 一张图 + 一个缩略图（缩略图字段名是 thumb）。
  # 内容必须是服务器上还没有的：内容寻址会命中去重，命中之后引用的是原来
  # 那个后端上的实体（这本身是对的），落点就测不出来了
  python "$ROOT/scripts/make-test-png.py" "$DIR/placement.png" 48
  out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)"     -F "file=@$DIR/placement.png" -F "thumb=@$DIR/placement.png" -F "visibility=private"     -F "original_name=placement.png")
  expect "上传成功（带缩略图）" '"url"' "$out"
  PLACED_ID=$(printf '%s' "$out" | jq_get "['id']")

  after_files=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
  if [ "$before_files" = "$after_files" ]; then
    ok "磁盘上没有新增文件（字节进了数据库）"
  else
    bad "数据库内驱动不该往磁盘写字节" "before=$before_files after=$after_files"
  fi
  out=$(req GET /admin/storage)
  expect "分布在数据库内驱动上" '"driver":"database"' "$out"
  expect "库里那份字节的占用不是 0" '"bytes":' "$out"

  say "存储分布：张数按图片算，不按实体条数翻倍"
  DIST_COUNT=$(printf '%s' "$out" | python -c "
import json, sys
rows = json.load(sys.stdin)['data']['distribution']
print(next((r['count'] for r in rows if r['driver'] == 'database'), -1))
" 2>/dev/null)
  [ "$DIST_COUNT" = "1" ] && ok "一张图 + 一个缩略图 = 1 张（不是 2）" || bad "张数该是 1" "count=$DIST_COUNT"
  PENDING=$(printf '%s' "$out" | jq_get "['pending']['count']")
  [ "$PENDING" = "0" ] && ok "没有待回收" || bad "刚传完不该有孤儿" "pending=$PENDING"

  say "还挂着实体时不许删后端"
  out=$(req DELETE "/admin/storage/backends/$BACKEND_ID")
  expect "拒绝删除并说明原因" '还有' "$out"

  say "删图后分布立刻回到 0，回收之后字节消失"
  # 前面的用例还留着一张公开图，所以基准取在这里：删掉这一张之后应当正好少一张
  base_total=$(req GET /admin/storage | jq_get "['totals']['count']")
  req DELETE "/images/$PLACED_ID" > /dev/null
  out=$(req GET /admin/storage)
  BINNED=$(printf '%s' "$out" | jq_get "['binned']['count']")
  [ "$BINNED" = "1" ] && ok "那张图在「回收站」一行里" || bad "回收站张数该是 1" "binned=$BINNED"
  # 驱动行不存在就是 0 张：没有在用内容的后端不会出现在分布里
  DIST_COUNT=$(printf '%s' "$out" | python -c "
import json, sys
rows = json.load(sys.stdin)['data']['distribution']
print(next((r['count'] for r in rows if r['driver'] == 'database'), 0))
" 2>/dev/null)
  [ "$DIST_COUNT" = "0" ] && ok "在用的张数回到 0" || bad "删完不该还算在用" "count=$DIST_COUNT"

  # 彻底删除：引用归零的实体当场回收，不留「待回收」等人点
  req POST /recycle/images/purge "{\"ids\":[$PLACED_ID]}" > /dev/null
  out=$(req GET /admin/storage)
  PENDING=$(printf '%s' "$out" | jq_get "['pending']['count']")
  [ "$PENDING" = "0" ] && ok "彻底删除后没有「待回收」残留" || bad "不该留下待回收" "pending=$PENDING"
  out=$(req POST /admin/storage/gc '{}')
  expect "立即回收入口仍然可用（兜底）" '"removed"' "$out"
  out=$(req GET /admin/storage)
  DB_ROWS=$(printf '%s' "$out" | jq_get "['totals']['count']")
  EXPECTED=$((base_total - 1))
  [ "$DB_ROWS" = "$EXPECTED" ] && ok "合计张数比删之前少了 1（$base_total → $DB_ROWS）" || bad "合计该是 $EXPECTED" "count=$DB_ROWS"

  # 清干净之后才放行删后端，顺手把默认切回本地
  out=$(req PUT /admin/storage/default '{"driver":"local"}')
  expect "默认后端切回本地" '"ok":true' "$out"
  out=$(req DELETE "/admin/storage/backends/$BACKEND_ID")
  expect "没有实体之后可以删掉后端" '"ok":true' "$out"
fi

say "不指定相册时落进默认相册"
DEFAULT_ALBUM=$(curl -s -b "$JAR" "$BASE/albums" | python -c "
import json, sys
albums = json.load(sys.stdin)['data']['albums']
print(next((a['id'] for a in albums if a['slug'] == 'default'), ''))
" 2>/dev/null)
if [ -z "$DEFAULT_ALBUM" ]; then
  bad "找不到默认相册" "albums 里没有 slug=default"
else
  ok "默认相册存在（id=$DEFAULT_ALBUM）"
  out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)"     -F "file=@$DIR/up.png" -F "visibility=private" -F "original_name=no-album.png")
  NO_ALBUM_ID=$(printf '%s' "$out" | jq_get "['id']")
  out=$(curl -s -b "$JAR" "$BASE/images?album_id=$DEFAULT_ALBUM&per_page=50")
  case "$out" in
    *placement.png*|*"no-album"*|*'"id":'*) ok "默认相册里能查到这张图" ;;
    *) bad "不带 album_id 的图该落进默认相册" "$out" ;;
  esac
  req DELETE "/images/$NO_ALBUM_ID" > /dev/null
fi

say "回收站计入额度，彻底删除才释放"
before=$(req GET /me | jq_get "['user']['used_bytes']")
req DELETE "/images/$IMAGE_ID" > /dev/null
after_delete=$(req GET /me | jq_get "['user']['used_bytes']")
if [ "$before" = "$after_delete" ]; then
  ok "进回收站不减少额度（$before）"
else
  bad "进回收站不该减少额度" "before=$before after=$after_delete"
fi
req POST /recycle/images/purge "{\"ids\":[$IMAGE_ID]}" > /dev/null
after_purge=$(req GET /me | jq_get "['user']['used_bytes']")
if [ "$after_purge" -lt "$before" ]; then
  ok "彻底删除后释放额度（$before → $after_purge）"
else
  bad "彻底删除该释放额度" "before=$before after=$after_purge"
fi

say "彻底删除当场回收物理文件，不留「待回收」"
# 切回本地驱动，这样能直接看磁盘上的文件数
req PUT /admin/storage/default '{"driver":"local"}' > /dev/null
before_files=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
python "$ROOT/scripts/make-test-png.py" "$DIR/reclaim.png" 56
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/reclaim.png" -F "visibility=private" -F "original_name=reclaim.png")
RECLAIM_ID=$(printf '%s' "$out" | jq_get "['id']")
after_upload=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$after_upload" -gt "$before_files" ] && ok "本地驱动上传后磁盘上有文件" || bad "本地驱动该往磁盘写" "before=$before_files after=$after_upload"
req DELETE "/images/$RECLAIM_ID" > /dev/null
out=$(req GET /admin/storage)
PENDING=$(printf '%s' "$out" | jq_get "['pending']['count']")
[ "$PENDING" = "0" ] && ok "进回收站不算待回收（文件要留着恢复）" || bad "回收站里的文件不该进待回收" "pending=$PENDING"
req POST /recycle/images/purge "{\"ids\":[$RECLAIM_ID]}" > /dev/null
after_purge_files=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$after_purge_files" = "$before_files" ] && ok "彻底删除当场删掉物理文件（$after_upload → $after_purge_files）" || bad "彻底删除该当场删文件" "before=$before_files after=$after_purge_files"
out=$(req GET /admin/storage)
PENDING=$(printf '%s' "$out" | jq_get "['pending']['count']")
[ "$PENDING" = "0" ] && ok "彻底删除后没有「待回收」残留" || bad "不该留下待回收" "pending=$PENDING"

say "条款：删除是真删，同意记录一起走"
out=$(req POST /terms/consent "{\"version_id\":$TERMS_ID}")
expect "管理员同意当前生效版本" '"agreed_at"' "$out"
body t6.json <<'JSON'
{"version":"v1.1","title":"服务条款","body":"第二版正文：放宽了上传限制的说明。","status":"active"}
JSON
out=$(post POST /admin/terms t6.json)
expect "发布 v1.1（v1.0 归档）" '"version":"v1.1"' "$out"
out=$(req GET /admin/terms)
expect "归档版本带着 1 条同意记录" '"consent_count":1' "$out"
out=$(req DELETE "/admin/terms/$TERMS_ID")
expect "删除归档版本成功" '"ok":true' "$out"
expect "返回连带删掉的同意记录条数" '"consents_removed":1' "$out"
out=$(req GET /admin/terms)
case "$out" in
  *'"version":"v1.0"'*) bad "删掉的版本不该还在列表里" "$out" ;;
  *) ok "删除后列表里没有 v1.0" ;;
esac

say "免登录切换用户与切回"
out=$(req POST /admin/users '{"email":"user@pixora.test","password":"Smoke-Test-2026x"}')
USER_ID=$(printf '%s' "$out" | jq_get "['user']['id']")
if [ -z "$USER_ID" ]; then
  out=$(req GET "/admin/users?per_page=50")
  USER_ID=$(printf '%s' "$out" | python -c "
import json,sys
items = json.load(sys.stdin)['data']
print([u['id'] for u in items if u['email'] == 'user@pixora.test'][0])
" 2>/dev/null)
fi
out=$(req POST "/admin/users/$USER_ID/impersonate" '{}')
expect "切换成功" 'user@pixora.test' "$out"
expect "带上 impersonator" '"impersonator"' "$out"
out=$(req GET /me)
expect "当前身份是普通用户" '"role":"user"' "$out"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回管理员" 'admin@pixora.test' "$out"
out=$(req GET /admin/users)
expect "切回后管理接口可用" '"email"' "$out"

say "单批次最大张数：按用户覆盖收紧"
USER_UID=$(req GET "/admin/users?per_page=50" | python -c "
import json,sys
items = json.load(sys.stdin)['data']
print([u['uid'] for u in items if u['email'] == 'user@pixora.test'][0])
" 2>/dev/null)
out=$(req PUT "/admin/ratelimit/users/$USER_UID" '{"max_batch_size":2}')
expect "给这个用户把单批上限压到 2" '"ok":true' "$out"
out=$(req POST "/admin/users/$USER_ID/impersonate" '{}')
expect "再次切换到该用户" 'user@pixora.test' "$out"
out=$(req POST /blobs/exists "{\"hashes\":[$(hashes 3)]}")
expect "他按覆盖值被挡住（3 > 2）" '单批次上限 2 张' "$out"
out=$(req POST /blobs/exists "{\"hashes\":[$(hashes 2)]}")
expect "2 个哈希在他这里放行" '"missing"' "$out"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回管理员" 'admin@pixora.test' "$out"
out=$(req DELETE "/admin/ratelimit/users/$USER_UID")
expect "清掉这个覆盖" '"ok":true' "$out"

say "处置时长：填了给到期时间，不填是永久"
out=$(req POST "/admin/users/$USER_ID/status" '{"action":"suspend","reason":"smoke: manual suspend","notify":false}')
expect "不填时长也能处置" '"status":"suspended"' "$out"
expect "永久处置没有到期时间" '"auto_restore_at":null' "$out"
out=$(req POST "/admin/users/$USER_ID/status" '{"action":"suspend","reason":"smoke: suspend 12h","duration_minutes":720,"notify":false}')
expect "填了时长就有到期时间" '"auto_restore_at":"20' "$out"
out=$(req GET "/admin/users/$USER_ID")
expect "处置记录里存的是分钟" '"duration_minutes":720' "$out"
out=$(req POST "/admin/users/$USER_ID/status" '{"action":"restore","reason":"smoke: restore","notify":false}')
expect "恢复账号" '"status":"active"' "$out"

say "安装账号：其他管理员动不了"
# 冒烟实例里管理员就是安装时创建的那个，另建一个管理员来试
out=$(req POST /admin/users '{"email":"admin2@pixora.test","password":"Smoke-Test-2026x","role":"admin"}')
ADMIN2_ID=$(printf '%s' "$out" | jq_get "['user']['id']")
out=$(req POST "/admin/users/$ADMIN2_ID/impersonate" '{}')
expect "切到第二个管理员" 'admin2@pixora.test' "$out"
out=$(req PATCH /admin/users/1 '{"email":"hijack@example.com"}')
expect "改不了安装账号的邮箱" '安装时创建的管理员' "$out"
out=$(req PATCH /admin/users/1 '{"role":"user"}')
expect "改不了安装账号的角色" '安装时创建的管理员' "$out"
out=$(req DELETE "/admin/users/1?confirm=admin@pixora.test")
expect "删不掉安装账号" '安装时创建的管理员' "$out"
out=$(req POST "/admin/users/1/impersonate" '{}')
expect "切不进安装账号" '安装时创建的管理员' "$out"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回安装账号" 'admin@pixora.test' "$out"
out=$(req GET /me)
expect "安装账号拿到自己的 uid" '"install_admin_uid":"1000"' "$out"
# 安装账号自己改自己不受限（改成原名，只验证这条路是通的）
out=$(req PATCH /admin/users/1 '{"email":"admin@pixora.test"}')
expect "安装账号可以改自己" '"email":"admin@pixora.test"' "$out"

say "删除账号：连着数据和文件一起清"
out=$(req POST /admin/users '{"email":"doomed@pixora.test","password":"Smoke-Test-2026x"}')
DOOMED_ID=$(printf '%s' "$out" | jq_get "['user']['id']")
DOOMED_UID=$(printf '%s' "$out" | jq_get "['user']['uid']")
out=$(req POST "/admin/users/$DOOMED_ID/impersonate" '{}')
expect "切到待删账号" 'doomed@pixora.test' "$out"
python "$ROOT/scripts/make-test-png.py" "$DIR/doomed.png" 72
files_before=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/doomed.png" -F "visibility=private" -F "original_name=doomed.png")
expect "待删账号先传一张" '"url"' "$out"
files_after=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$files_after" -gt "$files_before" ] && ok "文件已落盘（$files_before → $files_after）" \
  || bad "上传该落盘" "before=$files_before after=$files_after"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回安装账号" 'admin@pixora.test' "$out"
out=$(req DELETE "/admin/users/$DOOMED_ID")
expect "没带 confirm 不给删" 'confirm' "$out"
out=$(req DELETE "/admin/users/$DOOMED_ID?confirm=wrong@example.com")
expect "confirm 对不上也不给删" 'confirm' "$out"
out=$(req DELETE "/admin/users/$DOOMED_ID?confirm=doomed@pixora.test")
expect "对上邮箱才删" '"ok":true' "$out"
expect "返回回收掉的实体数" '"blobs":' "$out"
expect "当场回收物理文件" '"reclaimed_files":' "$out"
out=$(req GET "/admin/users?per_page=50")
case "$out" in
  *doomed@pixora.test*) bad "账号该从列表里消失" "$out" ;;
  *) ok "账号已从列表里消失" ;;
esac
files_left=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$files_left" -lt "$files_after" ] && ok "它的文件已经不在盘上（$files_after → $files_left）" \
  || bad "删账号该把文件一起清掉" "after=$files_after left=$files_left"
# 注销之后账号历史还查得到，而且写明是哪个管理员动的
out=$(req GET "/admin/logs?uid=$DOOMED_UID")
expect "注销后账号历史仍在" 'account_delete' "$out"
expect "注销那条写着管理员" '管理员' "$out"
expect "记录里带着销号时的邮箱" 'doomed@pixora.test' "$out"

say "注册邮箱白名单"
# 冒烟机的磁盘占用常年偏高，会把注册入口自动关掉（这是设计行为）。
# 下面几条要验的是注册本身的校验，先把自动关闭停掉，跑完再恢复
req PUT /admin/quota '{"auto_close_enabled":false}' > /dev/null
out=$(req PUT /admin/settings '{"site":{"email_domains":"@Only.Example.com, corp.example.com"}}')
expect "保存白名单" '"ok":true' "$out"
out=$(req GET /admin/settings)
expect "归一化后落库" '"email_domains":"only.example.com,corp.example.com"' "$out"
out=$(req GET /site)
expect "前台下发白名单" '"register_email_domains"' "$out"
expect "白名单内容正确" 'only.example.com' "$out"
out=$(req POST /auth/register/code '{"email":"someone@elsewhere.net"}')
expect "白名单外的邮箱发码被拒" '只接受' "$out"
out=$(req PUT /admin/settings '{"site":{"email_domains":""}}')
expect "清空白名单" '"ok":true' "$out"
out=$(req GET /site)
case "$out" in
  *'"register_email_domains":[]'*) ok "清空后不限域名" ;;
  *) bad "清空后该是空清单" "$out" ;;
esac

say "密码重置：邮件带链接，管理员设密码要强制改"
out=$(req POST "/admin/users/$USER_ID/password-reset-mail")
expect "管理员代发重置邮件：接口可用" '"ok":true' "$out"
# 冒烟实例没配 SMTP：这时必须如实说「留在发件箱」，不能回一句已发送
expect "如实报告没投出去" '"delivered":false' "$out"
expect "说明卡在哪一步" '发件箱' "$out"

MAIL_VARS=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT vars FROM mail_outbox WHERE template_name = 'reset_password' ORDER BY id DESC LIMIT 1"
).fetchone()
print(row[0] if row else "")
PY
)
expect "发件箱里那封带重置链接" 'reset-password?token=' "$MAIL_VARS"
expect "链接用的是随机令牌" 'pxr_' "$MAIL_VARS"
case "$MAIL_VARS" in
  *'reset-password?email='*) bad "链接里不该出现明文邮箱" "$MAIL_VARS" ;;
  *) ok "链接里没有明文邮箱" ;;
esac
RESET_CODE=$(printf '%s' "$MAIL_VARS" | python -c "
import json,sys
data = json.loads(sys.stdin.readline() or '{}')
print(data.get('code',''))
")
RESET_TOKEN=$(printf '%s' "$MAIL_VARS" | python -c "
import json,sys
data = json.loads(sys.stdin.readline() or '{}')
print(data.get('link','').split('token=')[-1].strip())
")
out=$(curl -s "$BASE/auth/password/link?token=$RESET_TOKEN")
expect "令牌换出让谁设密码（打码）" 'u***@p***.test' "$out"
out=$(curl -s -c "$DIR/jar-reset.txt" -X POST "$BASE/auth/password/link/reset" \
  -H 'Content-Type: application/json' \
  -d "{\"token\":\"$RESET_TOKEN\",\"password\":\"LinkReset-2026x\"}")
expect "点链接直接设新密码" '"ok":true' "$out"
out=$(curl -s -X POST "$BASE/auth/password/link/reset" \
  -H 'Content-Type: application/json' \
  -d "{\"token\":\"$RESET_TOKEN\",\"password\":\"LinkReset-Again-2026x\"}")
expect "同一个令牌用第二次被拒" '已经用过' "$out"

out=$(req POST "/admin/users/$USER_ID/password-reset-mail")
expect "再发一封用作验证码那条路" '"ok":true' "$out"
MAIL_VARS=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT vars FROM mail_outbox WHERE template_name = 'reset_password' ORDER BY id DESC LIMIT 1"
).fetchone()
print(row[0] if row else "")
PY
)
RESET_CODE=$(printf '%s' "$MAIL_VARS" | python -c "
import json,sys
print(json.loads(sys.stdin.readline() or '{}').get('code',''))
")
out=$(curl -s -c "$DIR/jar-reset.txt" -X POST "$BASE/auth/password/reset" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"user@pixora.test\",\"code\":\"$RESET_CODE\",\"password\":\"SelfReset-2026x\"}")
expect "用邮件里的验证码也能重置" '"ok":true' "$out"

say "发验证邮件要如实报告，不能永久「已发送」"
out=$(req POST /auth/register/code '{"email":"nobody@example.com"}')
expect "SMTP 没配时注册发码如实报错" '发信服务（SMTP）尚未配置' "$out"

say "关闭二次验证：动态码或邮箱验证码，任一对上"
# 自己算一个 TOTP：服务端用的是标准 HOTP/SHA1，30 秒一步
totp_code() {
  python - "$1" <<'PY'
import base64, hmac, hashlib, struct, sys, time
raw = sys.argv[1].upper()
secret = base64.b32decode(raw + "=" * ((8 - len(raw) % 8) % 8))
digest = hmac.new(secret, struct.pack(">Q", int(time.time()) // 30), hashlib.sha1).digest()
offset = digest[-1] & 0x0F
value = struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF
print(str(value % 1000000).zfill(6))
PY
}
out=$(req POST /me/totp '{"action":"setup"}')
SECRET=$(printf '%s' "$out" | jq_get "['secret']")
[ -n "$SECRET" ] && ok "生成二次验证密钥" || bad "该拿得到密钥" "$out"
out=$(req POST /me/totp "{\"action\":\"enable\",\"code\":\"$(totp_code "$SECRET")\"}")
expect "用动态码开启" '"enabled":true' "$out"
# 开启的同时就把恢复码交出来：验证器丢了的时候那是唯一的进门凭据
expect "开启时发一组恢复码" '"codes"' "$out"
CODES=$(printf '%s' "$out" | jq_get "['codes'][0]")
[ -n "$CODES" ] && ok "恢复码非空" || bad "该有一组恢复码" "$out"
out=$(req GET /me/recovery-codes)
expect "剩余数量与刚发的一组对得上" '"remaining":8' "$out"

out=$(req POST /me/totp '{"action":"send_disable_code"}')
expect "要邮箱验证码时邮件发不出去，如实报错" '发信服务（SMTP）尚未配置' "$out"
# 发不出去就不该留一个用户永远收不到的验证码：库里不该多出待用的那条
PENDING=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT COUNT(*) FROM email_verifications WHERE purpose = 'totp_disable' AND consumed_at IS NULL"
).fetchone()
print(row[0] if row else 0)
PY
)
[ "$PENDING" = "0" ] && ok "邮件没发出去时不留验证码" || bad "发不出去就不该留码" "pending=$PENDING"
out=$(req POST /me/totp '{"action":"disable","code":"000000"}')
expect "错的码带着「验证器丢了」的提示" '验证器不可用' "$out"
out=$(req POST /me/totp "{\"action\":\"disable\",\"code\":\"$(totp_code "$SECRET")\"}")
expect "动态码仍然可以关闭" '"enabled":false' "$out"
# 恢复自动关闭，后面的用例看到的是原来那份设置
req PUT /admin/quota '{"auto_close_enabled":true}' > /dev/null

say "管理员直接设密码：发信告知 + 下次登录强制改"
out=$(req PATCH "/admin/users/$USER_ID" '{"password":"AdminSet-2026x"}')
expect "设密码成功" '"email"' "$out"
expect "响应里带上邮件投递结果" '"mail":' "$out"
MAIL_VARS=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT vars FROM mail_outbox WHERE template_name = 'password_set_by_admin' ORDER BY id DESC LIMIT 1"
).fetchone()
print(row[0] if row else "")
PY
)
expect "邮件里写着新密码" 'AdminSet-2026x' "$MAIL_VARS"

out=$(req POST "/admin/users/$USER_ID/impersonate" '{}')
expect "切到该用户" 'user@pixora.test' "$out"
out=$(req GET /me)
expect "标记了必须改密码" '"must_change_password":true' "$out"
out=$(req GET /me/sessions)
expect "没改之前其它接口被拦" '先设置新密码' "$out"
out=$(req POST /me/password '{"current":"AdminSet-2026x","password":"SelfChosen-2026x"}')
expect "改密码本身放行" '"ok":true' "$out"
out=$(req GET /me/sessions)
expect "改完就正常了" '"device"' "$out"
out=$(req GET /me)
expect "标记已清掉" '"must_change_password":false' "$out"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回管理员" 'admin@pixora.test' "$out"

say "邮件模板：能改、能恢复、拼错的变量当场拒"
out=$(req GET /admin/mail/templates)
expect "列出模板" 'reset_password' "$out"
expect "带上内置文案（恢复默认用）" 'builtin_subject' "$out"
# 载荷用 ASCII：内联 JSON 里的中文在 Windows 上会被本地代码页转坏（见文件头）
out=$(req PUT /admin/mail/templates/verify_code '{"subject":"Smoke Verify Code","body":"Your code is {{code}}."}')
expect "改一版" '"customized":true' "$out"
expect "回读到新文案" 'Smoke Verify Code' "$out"
STORED=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT COUNT(*) FROM email_templates WHERE name = 'verify_code' AND subject = 'Smoke Verify Code'"
).fetchone()
print(row[0] if row else 0)
PY
)
[ "$STORED" = "1" ] && ok "改过的模板已经落库" || bad "模板该存进库" "rows=$STORED"
# 发一封走这个模板的信：发件箱里那封的主题应当是刚改的那份
out=$(req POST /me/totp '{"action":"send_disable_code"}')
SENT_SUBJECT=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT subject FROM email_templates WHERE name = 'verify_code'"
).fetchone()
print(row[0] if row else "")
PY
)
[ "$SENT_SUBJECT" = "Smoke Verify Code" ] && ok "发信时用的就是改过的模板"   || bad "该用改过的模板发信" "subject=$SENT_SUBJECT"
out=$(req PUT /admin/mail/templates/verify_code '{"subject":"subject","body":"{{emial}}"}')
expect "拼错的占位符被拒" 'emial' "$out"
out=$(req DELETE /admin/mail/templates/verify_code)
expect "恢复内置" '"customized":false' "$out"

say "头像：地址与上传两条路"
out=$(req PATCH /me '{"avatar":"https://cdn.example.org/me.png"}')
expect "外链头像存得下" '"avatar":"https://cdn.example.org/me.png"' "$out"
out=$(req PATCH /me '{"avatar":"http://127.0.0.1:8099/images/whatever.png"}')
expect "本站直链折成相对路径" '"avatar":"/images/whatever.png"' "$out"
out=$(req PATCH /me '{"avatar":"not-a-url"}')
expect "乱七八糟的地址不收" 'http(s)' "$out"
out=$(req PATCH /me '{"avatar":null}')
expect "清得掉" '"avatar":null' "$out"

# 上传当头像：走普通上传，落进默认相册并打上 avatar 标签
python "$ROOT/scripts/make-test-png.py" "$DIR/face.png" 96
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)"   -F "file=@$DIR/face.png" -F "tags=avatar" -F "visibility=private" -F "original_name=avatar.png")
URL=$(printf '%s' "$out" | jq_get "['url']")
[ -n "$URL" ] && ok "头像图上传成功" || bad "头像该传得上去" "$out"
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)"   -F "file=@$DIR/face.png" -F "tags=avatar" -F "visibility=private" -F "original_name=avatar-again.png")
expect "同一张再传一次命中去重" '"dedup":true' "$out"
out=$(req GET "/images?tag=avatar")
expect "默认相册里能按标签找到两张" '"total":2' "$out"
out=$(req PATCH /me "{\"avatar\":\"$URL\"}")
expect "上传结果直接当头像" '"avatar":"/images/' "$out"

say "登录第二步：两种码，默认动态码"
out=$(req POST /me/totp '{"action":"setup"}')
SECRET=$(printf '%s' "$out" | jq_get "['secret']")
out=$(req POST /me/totp "{\"action\":\"enable\",\"code\":\"$(totp_code "$SECRET")\"}")
expect "开启二次验证" '"enabled":true' "$out"

out=$(curl -s -X POST "$BASE/auth/login" -H 'Content-Type: application/json' -d '{"email":"admin@pixora.test","password":"Smoke-Test-2026x"}')
expect "第一步不带码：回 TOTP_REQUIRED" 'TOTP_REQUIRED' "$out"

# 要邮箱验证码这条：冒烟实例没配 SMTP，如实报错；而且账号存不存在答话一样
out=$(curl -s -X POST "$BASE/auth/login/email-code" -H 'Content-Type: application/json' -d '{"email":"admin@pixora.test"}')
expect "要邮箱验证码时如实说发不出去" '尚未配置' "$out"
out=$(curl -s -X POST "$BASE/auth/login/email-code" -H 'Content-Type: application/json' -d '{"email":"nobody@example.com"}')
expect "不存在的邮箱答话完全相同" '尚未配置' "$out"

# 邮件发不出去，那就直接把一条验证码塞进库：验「邮箱验证码能登录」这条路
python - "$DIR/pixora.db" <<'PY'
import hashlib, sqlite3, sys, datetime
conn = sqlite3.connect(sys.argv[1])
now = datetime.datetime.now(datetime.timezone.utc)
stamp = lambda at: at.strftime("%Y-%m-%dT%H:%M:%S.%f") + "Z"
conn.execute(
    "INSERT INTO email_verifications (email, code_hash, purpose, expires_at, consumed_at, attempts, request_ip, created_at) "
    "VALUES (?, ?, 'login_code', ?, NULL, 0, NULL, ?)",
    ("admin@pixora.test", hashlib.sha256(b"424242").hexdigest(), stamp(now + datetime.timedelta(minutes=10)), stamp(now)),
)
conn.commit()
PY
out=$(curl -s -X POST "$BASE/auth/login" -H 'Content-Type: application/json' -d '{"email":"admin@pixora.test","password":"Smoke-Test-2026x","code_method":"email","totp":"424242"}')
expect "用邮箱验证码登录成功" 'admin@pixora.test' "$out"
out=$(curl -s -X POST "$BASE/auth/login" -H 'Content-Type: application/json' -d '{"email":"admin@pixora.test","password":"Smoke-Test-2026x","code_method":"totp","totp":"424242"}')
expect "同一串码按动态码发就不认" '验证码不匹配' "$out"

out=$(req POST /me/totp "{\"action\":\"disable\",\"code\":\"$(totp_code "$SECRET")\"}")
expect "关闭二次验证，后面的用例接着跑" '"enabled":false' "$out"

say "图片归属：默认相册兜底，没有「未分类」"
out=$(req POST /albums '{"name":"smoke-target","visibility":"private"}')
# 建相册返回的就是相册本身（不是 {album: …}）
TARGET_ALBUM=$(printf '%s' "$out" | jq_get "['id']")
[ -n "$TARGET_ALBUM" ] && ok "建一个目标相册" || bad "该建得出来" "$out"
python "$ROOT/scripts/make-test-png.py" "$DIR/move.png" 40
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)"   -F "file=@$DIR/move.png" -F "visibility=private" -F "original_name=move.png")
MOVE_ID=$(printf '%s' "$out" | jq_get "['id']")
out=$(req GET "/images?ids=$MOVE_ID")
expect "新图落在默认相册" '默认相册' "$out"
out=$(req POST /images/batch "{\"action\":\"move\",\"ids\":[$MOVE_ID],\"album_id\":$TARGET_ALBUM}")
expect "移到自己建的相册" '"affected":1' "$out"
out=$(req GET "/images?ids=$MOVE_ID")
expect "归属跟着变" 'smoke-target' "$out"
out=$(req POST /images/batch "{\"action\":\"move\",\"ids\":[$MOVE_ID],\"album_id\":null}")
expect "传 null 也移得动" '"affected":1' "$out"
out=$(req GET "/images?ids=$MOVE_ID")
expect "null 的落点是默认相册（不是未分类）" '默认相册' "$out"
out=$(req DELETE "/albums/$TARGET_ALBUM")
expect "删相册仍然可用" '"ok":true' "$out"

say "并发数：可选 1..策略值"
out=$(req PUT /admin/ratelimit '{"max_concurrent_uploads":5}')
expect "把全局并发上限设成 5" '"ok":true' "$out"
out=$(req GET /me)
expect "生效值下发到 5" '"max_concurrent_uploads":5' "$out"
out=$(req PUT /admin/ratelimit '{"max_concurrent_uploads":4}')
expect "改回 4" '"ok":true' "$out"

say "游客上传：开关、访客账号、来源留痕"
out=$(req PUT /admin/settings '{"site":{"require_login_to_upload":true}}')
expect "先关掉游客上传" '"ok":true' "$out"
# 未登录上传：关着时 401
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/upload" -F "file=@$DIR/up.png" -F "original_name=guest.png")
[ "$code" = "401" ] && ok "游客上传关着时未登录上传被拒（401）" || bad "该回 401" "code=$code"
# 预检是唯一会把「文件哈希」送进服务端的入口：关着游客上传时它同样回 401，
# 改了前端也换不来一次比对
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/blobs/exists" \
  -H 'Content-Type: application/json' -d '{"hashes":["0000000000000000000000000000000000000000000000000000000000000000"]}')
[ "$code" = "401" ] && ok "游客上传关着时未登录预检被拒（401）" || bad "该回 401" "code=$code"

out=$(req PUT /admin/settings '{"site":{"require_login_to_upload":false}}')
expect "打开游客上传" '"ok":true' "$out"
# /site 这时要给访客发一个 CSRF Cookie，并下发访客额度
curl -s -D "$DIR/guest-head.txt" -c "$DIR/jar-guest.txt" "$BASE/site" > "$DIR/guest-site.json"
grep -qi "pixora_csrf" "$DIR/guest-head.txt" && ok "访客拿到 CSRF Cookie"   || bad "该给访客发 CSRF Cookie" "$(head -c 120 "$DIR/guest-head.txt")"
expect "下发访客额度" '"guest_upload"' "$(cat "$DIR/guest-site.json")"
GUEST_CSRF=$(grep -o 'pixora_csrf[[:space:]]*[^[:space:]]*' "$DIR/jar-guest.txt" | awk '{print $NF}' | tail -1)
python "$ROOT/scripts/make-test-png.py" "$DIR/guest.png" 64
out=$(curl -s -b "$DIR/jar-guest.txt" -c "$DIR/jar-guest.txt" -X POST "$BASE/upload"   -H "X-CSRF-Token: $GUEST_CSRF" -H "User-Agent: SmokeGuest/1.0"   -F "file=@$DIR/guest.png" -F "visibility=private" -F "original_name=guest.png")
expect "未登录也能传上去" '"url"' "$out"
GUEST_IMAGE=$(printf '%s' "$out" | jq_get "['id']")
expect "去重标记与登录用户一致" '"dedup"' "$out"
# 归属与来源：图片挂在访客账号下，IP 与终端都记下了
GUEST_ROW=$(python - "$DIR/pixora.db" "$GUEST_IMAGE" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
row = conn.execute(
    "SELECT u.uid, u.email, i.upload_ip, i.upload_agent FROM images i        JOIN users u ON u.id = i.owner_id WHERE i.id = ?",
    (int(sys.argv[2]),),
).fetchone()
print("|".join("" if v is None else str(v) for v in row) if row else "")
PY
)
case "$GUEST_ROW" in
  0000*SmokeGuest/1.0) ok "挂在访客账号下，IP 与终端都记下了（$GUEST_ROW）" ;;
  *) bad "该挂到访客账号并记录来源" "$GUEST_ROW" ;;
esac
out=$(req GET "/images?uid=0000")
expect "管理员能翻到访客的照片" '"total":' "$out"
# 上面那次上传客户端填的是 private：访客只有公开一种，服务端按账号定
expect "访客上传被强制公开" '"visibility":"public"' "$out"

# 访客账号不接受登录：它是游客上传的归属地，不是给人用的账号
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/login" \
  -H 'Content-Type: application/json' \
  -d '{"email":"visitor@pixora.invalid","password":"Visitor-Login-2026x"}')
[ "$code" = "403" ] && ok "访客账号登录被拒（403）" || bad "访客账号该登不进" "code=$code"
# 管理端把它标出来，改密码 / 改角色 / 切换登录一概拒绝（配额与处置不受影响）
out=$(req GET "/admin/users?query=0000")
expect "用户列表标出访客账号" '"guest":true' "$out"
GUEST_ID=$(printf '%s' "$out" | jq_get "[0]['id']")
for attempt in '{"password":"Visitor-Login-2026x"}' '{"role":"admin"}'; do
  out=$(req PATCH "/admin/users/$GUEST_ID" "$attempt")
  expect "管理员改访客账号的密码/角色被拒（$attempt）" '访客账号是系统账号' "$out"
done
# 邮箱可以改：它只是个记号（访客登录不进来、也收发不了信），不验证、不发通知
out=$(req PATCH "/admin/users/$GUEST_ID" '{"email":"visitor-bucket@pixora.test"}')
expect "管理员可以改访客账号的邮箱" '"email":"visitor-bucket@pixora.test"' "$out"
expect "改访客邮箱不发变更通知" '"mail":null' "$out"
out=$(req POST "/admin/users/$GUEST_ID/password-reset-mail" '{}')
expect "访客账号仍然收不到重置邮件" '访客账号是系统账号' "$out"
code=$(curl -s -o /dev/null -w '%{http_code}' -b "$JAR" -c "$JAR" -X POST \
  "$BASE/admin/users/$GUEST_ID/impersonate" -H 'Content-Type: application/json' \
  -H "X-CSRF-Token: $(csrf)" -d '{}')
[ "$code" = "403" ] && ok "不能切换登录到访客账号（403）" || bad "该回 403" "code=$code"
# 也不能删：删了等于把全站游客的图一次性清空
code=$(curl -s -o /dev/null -w '%{http_code}' -b "$JAR" -c "$JAR" -X DELETE \
  "$BASE/admin/users/$GUEST_ID?confirm=visitor%40pixora.invalid" -H "X-CSRF-Token: $(csrf)")
[ "$code" = "403" ] && ok "访客账号不能被删除（403）" || bad "该回 403" "code=$code"
out=$(req GET "/admin/users?query=0000")
expect "被拒之后访客账号还在" '"total":1' "$out"

out=$(req PUT /admin/settings '{"site":{"require_login_to_upload":true}}')
expect "关回游客上传" '"ok":true' "$out"
out=$(req GET "/admin/users?query=0000")
expect "关闭游客上传不删访客账号" '"total":1' "$out"

say "缩略图：?thumb=1 给的是缩略图那一份"
# 缩略图用 stdin 递给 curl（`@-`）：git-bash 下同一条命令里放两个「绝对路径的 -F
# 文件」会让 curl 报 rc=26（命令行里那些 /e/... 路径被转换时的坑），@- 绕开它
python - "$DIR/thumb.webp" <<'PY'
import sys
open(sys.argv[1], "wb").write(b"RIFF" + bytes([0x1a, 0, 0, 0]) + b"WEBP" + bytes(14))
PY
out=$(cat "$DIR/thumb.webp" | curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/up.png" -F "thumb=@-;type=image/webp" -F "visibility=public" -F "original_name=thumb-check.png")
expect "带缩略图上传成功" '"thumb_url"' "$out"
THUMB_URL=$(printf '%s' "$out" | jq_get "['thumb_url']")
ORIG_URL=$(printf '%s' "$out" | jq_get "['url']")
THUMB_TYPE=$(curl -s -o /dev/null -w '%{content_type}' "$THUMB_URL")
THUMB_SIZE=$(curl -s -o /dev/null -w '%{size_download}' "$THUMB_URL")
ORIG_SIZE=$(curl -s -o /dev/null -w '%{size_download}' "$ORIG_URL")
case "$THUMB_TYPE" in
  image/webp) ok "带 ?thumb=1 时发的是缩略图（$THUMB_SIZE 字节）" ;;
  *) bad "该发缩略图那一份" "content-type=$THUMB_TYPE" ;;
esac
[ "$THUMB_SIZE" != "$ORIG_SIZE" ] \
  && ok "缩略图与原图是两份实体（$THUMB_SIZE vs $ORIG_SIZE）" \
  || bad "缩略图不该等于原图" "$THUMB_SIZE"

say "缩略图重算：老图按新尺寸重来"
# 造一张 800px 的图 + 一张假的「老缩略图」（26 字节的 webp），
# 重算之后这张缩略图应当被服务端生成的大图替换掉
python "$ROOT/scripts/make-test-png.py" "$DIR/big.png" 800
out=$(cat "$DIR/thumb.webp" | curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/big.png" -F "thumb=@-;type=image/webp" -F "visibility=public" -F "original_name=big-thumb.png")
expect "带旧缩略图上传" '"thumb_url"' "$out"
BIG_THUMB=$(printf '%s' "$out" | jq_get "['thumb_url']")
BIG_BEFORE_TYPE=$(curl -s -o /dev/null -w '%{content_type}' "$BIG_THUMB")
BIG_BEFORE_SIZE=$(curl -s -o /dev/null -w '%{size_download}' "$BIG_THUMB")
case "$BIG_BEFORE_TYPE" in
  image/webp) ok "重算前用的是客户端那张小图（$BIG_BEFORE_SIZE 字节）" ;;
  *) bad "重算前该是客户端那张" "content-type=$BIG_BEFORE_TYPE" ;;
esac
out=$(req POST /admin/storage/thumbs/rebuild '{}')
expect "开始重算" '"running":true' "$out"
# 后台分批做，轮询进度直到跑完（最多等 40 秒）
for _ in $(seq 1 20); do
  out=$(req GET /admin/storage)
  case "$out" in
    *'"running":true'*) sleep 2 ;;
    *) break ;;
  esac
done
expect "重算跑完" '"running":false' "$out"
BIG_AFTER_TYPE=$(curl -s -o /dev/null -w '%{content_type}' "$BIG_THUMB")
BIG_AFTER_SIZE=$(curl -s -o /dev/null -w '%{size_download}' "$BIG_THUMB")
[ "$BIG_AFTER_TYPE" = "image/jpeg" ]   && ok "重算后换成服务端生成的大缩略图（$BIG_AFTER_SIZE 字节）"   || bad "该换成服务端生成的 jpeg" "content-type=$BIG_AFTER_TYPE"
[ "$BIG_AFTER_SIZE" -gt "$BIG_BEFORE_SIZE" ]   && ok "缩略图确实变大了（$BIG_BEFORE_SIZE → $BIG_AFTER_SIZE）"   || bad "重算后该更大" "before=$BIG_BEFORE_SIZE after=$BIG_AFTER_SIZE"

say "同一份内容复用：不在图库里留第二份（头像那套）"
# 内容要是新的（尺寸别和上面用过的撞：内容一样就会命中去重，复用测不出来）
python "$ROOT/scripts/make-test-png.py" "$DIR/reuse.png" 104
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/reuse.png" -F "visibility=private" -F "original_name=reuse-a.png")
FIRST_ID=$(printf '%s' "$out" | jq_get "['id']")
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/reuse.png" -F "visibility=private" -F "tags=avatar" \
  -F "reuse_existing=1" -F "original_name=reuse-b.png")
expect "带上复用开关" '"reused":true' "$out"
REUSED_ID=$(printf '%s' "$out" | jq_get "['id']")
[ -n "$FIRST_ID" ] && [ "$FIRST_ID" = "$REUSED_ID" ] \
  && ok "复用指回图库里那一张（id=$FIRST_ID）" \
  || bad "该指回已有那张" "first=$FIRST_ID reused=$REUSED_ID"
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/reuse.png" -F "visibility=private" -F "original_name=reuse-c.png")
THIRD_ID=$(printf '%s' "$out" | jq_get "['id']")
[ "$THIRD_ID" != "$FIRST_ID" ] \
  && ok "不带开关时仍是新的一条记录" \
  || bad "普通上传该是新记录" "id=$THIRD_ID"

say "防盗链"
out=$(req PUT /admin/settings '{"security":{"hotlink_enabled":true,"hotlink_allow_empty":true,"hotlink_domains":"allowed.example.com"}}')
expect "开启防盗链" '"ok":true' "$out"
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/up.png" -F "visibility=public" -F "original_name=hotlink.png")
URL=$(printf '%s' "$out" | jq_get "['url']")
if [ -n "$URL" ]; then
  PATH_ONLY=$(printf '%s' "$URL" | sed 's#http://127.0.0.1:8099##')
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Referer: https://evil.example.net/thread" "http://127.0.0.1:8099$PATH_ONLY")
  [ "$code" = "403" ] && ok "陌生来源被拒（403）" || bad "陌生来源该被拒" "code=$code"
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Referer: https://allowed.example.com/post" "http://127.0.0.1:8099$PATH_ONLY")
  [ "$code" = "200" ] && ok "白名单来源放行（200）" || bad "白名单来源该放行" "code=$code"
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Referer: http://127.0.0.1:8099/" "http://127.0.0.1:8099$PATH_ONLY")
  [ "$code" = "200" ] && ok "本站来源放行（200）" || bad "本站来源该放行" "code=$code"
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8099$PATH_ONLY")
  [ "$code" = "200" ] && ok "空来源按配置放行（200）" || bad "空来源该放行" "code=$code"
  # 防盗链只拦直链，不该影响列表接口
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/plaza?per_page=5")
  [ "$code" = "200" ] && ok "广场接口不受防盗链影响" || bad "广场接口该正常" "code=$code"
else
  bad "上传拿直链" "$out"
fi

say "排序：全部选项都能跑通"
for sort in shuffle new old size smallest name type hot; do
  out=$(curl -s "$BASE/plaza?sort=$sort&seed=12345&per_page=5")
  case "$out" in
    *'"error":null'*) ok "广场 sort=$sort" ;;
    *) bad "广场 sort=$sort" "$out" ;;
  esac
done
out=$(curl -s "$BASE/plaza?sort=bogus&per_page=5")
case "$out" in
  *'"error":null'*) ok "未识别的 sort 回落到默认" ;;
  *) bad "未识别的 sort" "$out" ;;
esac
# 随机排序的种子：同一个种子两次取到的顺序必须一致（翻页才不会重复/漏图）
first=$(curl -s "$BASE/plaza?sort=shuffle&seed=777&per_page=3")
second=$(curl -s "$BASE/plaza?sort=shuffle&seed=777&per_page=3")
if [ "$first" = "$second" ]; then
  ok "同一 seed 的随机顺序稳定"
else
  bad "同一 seed 的顺序该稳定" "两次结果不一致"
fi

say "站点默认头像：上传即生效，没设过头像的账号都用它"
# 服务端只嗅探魔数，这张 26 字节的 webp 够用
python - "$DIR/site-avatar.webp" <<'PY'
import sys
open(sys.argv[1], "wb").write(b"RIFF" + bytes([0x1a, 0, 0, 0]) + b"WEBP" + bytes(14))
PY
out=$(cat "$DIR/site-avatar.webp" | curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/admin/settings/avatar" -H "X-CSRF-Token: $(csrf)" -F "file=@-;type=image/webp")
expect "上传默认头像成功" '"url"' "$out"
expect "回报了字节数" '"bytes"' "$out"
out=$(curl -s "$BASE/site")
expect "站点信息里带默认头像" '"default_avatar"' "$out"
DEFAULT_AVATAR=$(printf '%s' "$out" | python -c "import json,sys; print(json.load(sys.stdin)['data'].get('default_avatar') or '')")
[ -n "$DEFAULT_AVATAR" ] && ok "默认头像有公开地址（$DEFAULT_AVATAR）" || bad "该有地址" "$out"
if [ -n "$DEFAULT_AVATAR" ]; then
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8099$DEFAULT_AVATAR")
  [ "$code" = "200" ] && ok "默认头像本体可访问（200）" || bad "该回 200" "code=$code url=$DEFAULT_AVATAR"
fi
# 默认头像是「下发给前端回落」的，不写进账号行：清掉自己的头像后 /me 里就是 null
out=$(req PATCH /me '{"avatar":""}')
expect "默认头像不写进账号行（清掉自己的之后是 null）" '"avatar":null' "$out"
out=$(req PUT /admin/settings '{"appearance":{"default_avatar":null}}')
expect "清掉默认头像" '"ok":true' "$out"
out=$(curl -s "$BASE/site")
expect "清掉之后不再下发地址" '"default_avatar":null' "$out"
if [ -n "$DEFAULT_AVATAR" ]; then
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8099$DEFAULT_AVATAR")
  [ "$code" = "404" ] && ok "资产字节也一起删了（404）" || bad "该回 404" "code=$code"
fi

say "换头像时清理本地那张：没别的引用就连文件一起收"
python "$ROOT/scripts/make-test-png.py" "$DIR/avatar-clean.png" 120
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/avatar-clean.png" -F "tags=avatar" -F "visibility=private" -F "original_name=avatar-clean.png")
expect "传一张当头像的图" '"url"' "$out"
files_after_upload=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
AVATAR_DIRECT=$(printf '%s' "$out" | jq_get "['url']")
out=$(req PATCH /me "{\"avatar\":\"$AVATAR_DIRECT\"}")
expect "设为头像（站内直链折成相对路径）" '"avatar":"/images/' "$out"
out=$(req PATCH /me '{"avatar":"https://example.com/me.png"}')
expect "换成外链头像" 'https://example.com/me.png' "$out"
AVATAR_ROWS=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
print(conn.execute(
    "SELECT COUNT(*) FROM images WHERE original_name = 'avatar-clean.png' AND deleted_at IS NULL"
).fetchone()[0])
PY
)
[ "$AVATAR_ROWS" = "0" ] && ok "本地那张已从图库消失" || bad "该被清掉" "rows=$AVATAR_ROWS"
files_after=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$files_after" -lt "$files_after_upload" ]   && ok "没有别的引用，物理文件也收了（$files_after_upload → $files_after）"   || bad "无引用时该连文件一起删" "upload=$files_after_upload after=$files_after"

# 反过来：同一份内容在图库里还留着另一条引用时，只删头像这条，文件留着
python "$ROOT/scripts/make-test-png.py" "$DIR/avatar-shared.png" 128
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/avatar-shared.png" -F "visibility=private" -F "original_name=avatar-shared.png")
expect "先存一份在相册里" '"url"' "$out"
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" -F "file=@$DIR/avatar-shared.png" -F "tags=avatar" -F "visibility=private" -F "original_name=avatar-shared-tag.png")
expect "同一份内容再传一次当头像" '"url"' "$out"
SHARED_DIRECT=$(printf '%s' "$out" | jq_get "['url']")
out=$(req PATCH /me "{\"avatar\":\"$SHARED_DIRECT\"}")
expect "设为头像" '"avatar":"/images/' "$out"
files_before_shared=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
out=$(req PATCH /me '{"avatar":"https://example.com/me2.png"}')
expect "再换成外链头像" 'https://example.com/me2.png' "$out"
SHARED_ROWS=$(python - "$DIR/pixora.db" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
rows = dict(conn.execute(
    "SELECT original_name, COUNT(*) FROM images WHERE original_name LIKE 'avatar-shared%' AND deleted_at IS NULL GROUP BY original_name"
))
print(f"{rows.get('avatar-shared.png', 0)}|{rows.get('avatar-shared-tag.png', 0)}")
PY
)
case "$SHARED_ROWS" in
  "1|0") ok "只删了头像那条引用，相册里那份还在（$SHARED_ROWS）" ;;
  *) bad "该只删头像那条" "相册那份|头像那份 = $SHARED_ROWS" ;;
esac
files_after_shared=$(find "$DIR/blobs" -type f 2>/dev/null | wc -l)
[ "$files_after_shared" = "$files_before_shared" ]   && ok "还有引用，物理文件留着（$files_after_shared）"   || bad "有引用时别删文件" "before=$files_before_shared after=$files_after_shared"

say "换绑邮箱与账号历史"
# 没有 SMTP 时这一步要如实报错，而不是假装把码发出去了
out=$(req POST /me/email/code '{"email":"admin-new@pixora.test","password":"Smoke-Test-2026x"}')
expect "SMTP 没配时换绑发码如实报错" '发信服务（SMTP）尚未配置' "$out"
# 码在真实流程里由邮件送达；这里直接落一条，走完第二步
python - "$DIR/pixora.db" <<'PY'
import datetime, hashlib, sqlite3, sys
conn = sqlite3.connect(sys.argv[1])
stamp = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=10)).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"
conn.execute(
    "INSERT INTO email_verifications (email, code_hash, purpose, expires_at, consumed_at, attempts, request_ip, created_at) "
    "VALUES (?, ?, 'change_email', ?, NULL, 0, '127.0.0.1', ?)",
    ("admin-new@pixora.test", hashlib.sha256(b"135790").hexdigest(), stamp, stamp),
)
conn.commit()
PY
out=$(req POST /me/email '{"email":"admin-new@pixora.test","code":"135790"}')
expect "带上验证码换绑成功" '"email":"admin-new@pixora.test"' "$out"
expect "通知没发出去时如实说" '"delivered":false' "$out"
out=$(req GET /me)
expect "当前邮箱已经是新的" 'admin-new@pixora.test' "$out"
out=$(req GET "/admin/logs?query=admin-new@pixora.test")
expect "账号历史记下这次换绑" 'account_email_change' "$out"
expect "换绑那条标明是本人操作" '本人' "$out"
out=$(req GET "/admin/logs?action=login_success")
expect "登录记录也在账号历史里" 'login_success' "$out"
out=$(req GET "/admin/logs?action=account_email_change")
expect "按动作筛得动" '更换邮箱' "$out"

say "下载：给的是真图片文件，匿名也能下"
python "$ROOT/scripts/make-test-png.py" "$DIR/dl.png" 96
out=$(req POST /albums '{"name":"dl-album","visibility":"public"}')
DL_ALBUM=$(printf '%s' "$out" | jq_get "['id']")
out=$(curl -s -b "$JAR" -c "$JAR" -X POST "$BASE/upload" -H "X-CSRF-Token: $(csrf)" \
  -F "file=@$DIR/dl.png" -F "album_id=$DL_ALBUM" -F "visibility=public" -F "original_name=下载测试.png")
DL_URL=$(printf '%s' "$out" | jq_get "['url']")
[ -n "$DL_URL" ] && ok "下载测试图已上传" || bad "图该传得上去" "$out"

# 响应头：附件名要是 .png 这种真扩展名，不再是 .img
HEADERS=$(curl -s -D - -o /dev/null "$DL_URL?download=1")
expect "下载带附件头" 'attachment;' "$HEADERS"
expect "附件名是真图片扩展名" '.png' "$HEADERS"
if printf '%s' "$HEADERS" | grep -qi 'filename=.*\.img'; then
  bad "不该再出现 .img" "$(printf '%s' "$HEADERS" | grep -i content-disposition)"
else
  ok "附件名不是 .img"
fi
if printf '%s' "$HEADERS" | grep -qi 'cache-control'; then
  ok "下载响应带缓存策略"
else
  bad "下载响应该带缓存策略" "$HEADERS"
fi

say "下载限频：匿名的额度单独设，超了给 429"
out=$(req PUT /admin/ratelimit '{"download_anon_per_minute":2,"download_anon_per_day":100}')
expect "把匿名下载限到 2 次/分钟" '"ok":true' "$out"
code1=$(curl -s -o /dev/null -w '%{http_code}' "$DL_URL?download=1&retry=1")
code2=$(curl -s -o /dev/null -w '%{http_code}' "$DL_URL?download=1&retry=2")
code3=$(curl -s -o /dev/null -w '%{http_code}' "$DL_URL?download=1&retry=3")
[ "$code1" = "200" ] && [ "$code2" = "200" ] && ok "前两次放行" || bad "前两次该放行" "$code1/$code2"
[ "$code3" = "429" ] && ok "第三次被限流（429）" || bad "第三次该 429" "$code3"
out=$(curl -s -b "$JAR" -o /dev/null -w '%{http_code}' "$DL_URL?download=1&asuser=1")
[ "$out" = "200" ] && ok "登录用户的额度分开算（管理员不受限）" || bad "登录用户该能下" "$out"

say "单张图片最大大小：可以按用户单独设"
# 用户详情按自增 id 取，限流覆盖按 uid 写：两个接口的路径口径本来就不一样
ID_1000=$(req GET "/admin/users?query=1000" | jq_get "[0]['id']")
UID_1000=$(req GET "/admin/users?query=1000" | jq_get "[0]['uid']")
POLICY=$(req GET "/admin/users/$ID_1000")
expect "详情里带登录方式字段" '"identities"' "$POLICY"
out=$(req PUT "/admin/ratelimit/users/$UID_1000" '{"max_file_size":1048576}')
expect "写入单用户单张上限" '"ok":true' "$out"
out=$(req GET /me)
expect "生效值跟着变（1 MiB）" '"max_file_size":1048576' "$out"

say "登录方式：默认只有邮箱；没配好的第三方不摆按钮"
out=$(curl -s "$BASE/site")
expect "下发 login_methods" '"login_methods"' "$out"
expect "默认不开 GitHub" '"github":false' "$out"
out=$(req GET /me/identities)
expect "邮箱必需" '"email"' "$out"
expect "没有绑定第三方" '"identities":[]' "$out"
out=$(req DELETE /me/identities/email)
expect "解绑邮箱被拒" '邮箱是账号本身' "$out"
out=$(req POST /me/identities '{"provider":"phone","phone":"13800138000","code":"123456"}')
expect "短信没配时绑手机号给出原因" '短信' "$out"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/oauth/github/start?next=/")
[ "$code" != "200" ] && ok "没配凭据的第三方入口不可用（$code）" || bad "该拒绝未配置的入口" "$code"

say "配额保存：回传保存后的整页生效值"
out=$(req PUT /admin/quota '{"auto_close_enabled":true}')
expect "保存返回生效值（与 GET 同形状）" '"per_user_max"' "$out"
expect "带上容量口径" '"capacity"' "$out"

say "退出登录"
out=$(req POST /auth/logout '{}')
expect "登出成功" '"ok":true' "$out"
out=$(req GET /me)
expect "登出后接口要求重新登录" 'UNAUTHORIZED' "$out"

say "结果"
printf '  通过 %d 项，失败 %d 项\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
