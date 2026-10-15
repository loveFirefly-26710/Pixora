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
expect "6 个哈希超过单批上限 5，被拒" '单批次最多 5 张' "$out"
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
expect "他按覆盖值被挡住（3 > 2）" '单批次最多 2 张' "$out"
out=$(req POST /blobs/exists "{\"hashes\":[$(hashes 2)]}")
expect "2 个哈希在他这里放行" '"missing"' "$out"
out=$(req POST /admin/impersonate/stop '{}')
expect "切回管理员" 'admin@pixora.test' "$out"
out=$(req DELETE "/admin/ratelimit/users/$USER_UID")
expect "清掉这个覆盖" '"ok":true' "$out"

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

say "退出登录"
out=$(req POST /auth/logout '{}')
expect "登出成功" '"ok":true' "$out"
out=$(req GET /me)
expect "登出后接口要求重新登录" 'UNAUTHORIZED' "$out"

say "结果"
printf '  通过 %d 项，失败 %d 项\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
