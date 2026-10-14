# Pixora API 契约

> 前端与后端共同遵守的唯一接口说明。改接口先改这份文件。
> 所有路径以 `/api/v1` 为前缀，直链路由 `/{prefix}/{hash}.{ext}` 不在此列。

## 1. 通用约定

### 1.1 响应信封

成功：

```json
{ "data": { }, "error": null }
```

失败：

```json
{
  "data": null,
  "error": {
    "code": "QUOTA_EXCEEDED",
    "message": "超出账号配额",
    "detail": "还差 2411724 字节"
  }
}
```

`error.code` 是给程序看的，`error.message` 是给用户看的（中文），`error.detail` 可选。

分页响应统一带 `meta`：

```json
{
  "data": [ ],
  "meta": { "page": 1, "per_page": 24, "total": 236, "total_pages": 10 },
  "error": null
}
```

### 1.2 错误码总表

| code | HTTP | 含义 |
|---|---|---|
| `BAD_REQUEST` | 400 | 参数不合法 |
| `UNAUTHORIZED` | 401 | 未登录或凭据无效 |
| `INVALID_CREDENTIALS` | 401 | 邮箱或密码错误 |
| `TOKEN_REVOKED` | 401 | Token 已撤销或过期 |
| `CSRF_FAILED` | 403 | CSRF 校验失败 |
| `ACCOUNT_PENDING` | 403 | 邮箱未验证，账号未激活 |
| `ACCOUNT_SUSPENDED` | 403 | 账号被暂停，可按解封指引申诉 |
| `ACCOUNT_BANNED` | 403 | 账号被封禁，需管理员解封 |
| `ACCOUNT_LOCKED` | 403 | 连续登录失败被临时锁定 |
| `IP_BLOCKED` | 403 | 来源 IP 命中黑名单 |
| `FORBIDDEN` | 403 | 权限不足（非管理员 / 非资源归属者） |
| `NOT_FOUND` | 404 | 资源不存在 |
| `CONFLICT` | 409 | 唯一性冲突（邮箱已注册 / 相册 slug 重复） |
| `QUOTA_EXCEEDED` | 413 | 超出账号配额 |
| `FILE_TOO_LARGE` | 413 | 超出单文件上限 |
| `UNSUPPORTED_FORMAT` | 415 | 格式不在白名单内 |
| `INVALID_IMAGE` | 422 | 魔数校验失败或不是图片 |
| `RATE_LIMITED` | 429 | 触发频率限制，带 `Retry-After` 头 |
| `QUEUE_FULL` | 429 | 队列深度超阈值，带 `Retry-After` |
| `TERMS_REQUIRED` | 451 | 未同意当前生效条款 |
| `INTERNAL` | 500 | 服务端错误 |
| `STORAGE_UNAVAILABLE` | 503 | 存储后端不可用 |

暂停与封禁都是 403，靠 `code` 区分；客户端不要只看状态码。

`ACCOUNT_SUSPENDED` / `ACCOUNT_BANNED` 的响应会附加字段：

```json
{ "code": "ACCOUNT_SUSPENDED", "message": "账号已被暂停使用，无法调用 API",
  "suspended_at": "2026-10-14T15:30:00Z", "reason": "上传违规内容，违反服务条款第 3 条",
  "appeal_email": "admin@example.com", "appeal_url": "https://img.example.com/suspended" }
```

### 1.3 鉴权

浏览器端：

- 会话 Cookie：`pixora_session`（HttpOnly + SameSite=Lax + Secure）
- 每个写请求带 CSRF 头：`X-CSRF-Token: <pixora_csrf Cookie 的值>`
- 登录与 `GET /api/v1/me` 会在响应体里带回 `csrf_token`，前端存内存；同时写一个非 HttpOnly 的 `pixora_csrf` Cookie 供刷新后还原

API 客户端：

- `Authorization: Bearer px_live_xxx` 或 `X-API-Key: px_live_xxx`
- Token 权限范围：`upload` / `read` / `write` / `admin`，路由要求声明见各条目
- 带上 Token 时不校验 CSRF

### 1.4 时间与大小

- 时间一律是 RFC3339 UTC 字符串：`2026-10-14T15:30:00Z`
- 字节数是整数（`size`、`quota_bytes`、`used_bytes`）
- 前端负责把字节数与时间本地化展示

### 1.5 字段命名

- 图片列表项统一叫 `ImageItem`（见 4.1），页面之间复用同一形状，避免每个页面定义一套
- 可见性取值：`public` / `private`
- 用户状态取值：`pending` / `active` / `suspended` / `banned`
- 角色取值：`admin` / `user`

## 2. 站点与认证

### `GET /api/v1/site` — 公开

未登录也能调，用于渲染导航、登录页、注册页与直链前缀。

```json
{ "data": {
  "name": "Pixora", "base_url": "https://img.example.com",
  "upload_prefix": "images",
  "registration_open": true, "registration_closed_reason": null,
  "plaza_public": true, "plaza_grid_columns": 3, "recycle_retention_days": 30,
  "formats": ["jpg","png","gif","webp","avif","heic","tiff","bmp","svg"],
  "max_file_bytes": 20971520, "max_batch_size": 1000, "max_pixels": 80000000,
  "thumb_client": true, "thumb_server_fallback": false,
  "appeal_email": "admin@example.com", "background": null,
  "installed": true, "terms": { "required": false, "version": "v2.1", "articles": 6 }
} }
```

`terms.required` 表示「当前登录用户是否必须先同意条款」；未登录时为 `false`。

### `POST /api/v1/auth/register/code` — 公开

请求：`{ "email": "a@b.com" }`
响应：`{ "data": { "sent": true, "expires_in": 600, "resend_after": 60 } }`
错误：`BAD_REQUEST`（邮箱格式非法）、`FORBIDDEN`（一次性邮箱 / MX 校验失败）、`CONFLICT`（邮箱已注册）、`RATE_LIMITED`（同邮箱 5 次/小时或同 IP 20 次/小时）、`ACCOUNT_BANNED`（同邮箱曾被封禁）。

### `POST /api/v1/auth/register` — 公开

请求：`{ "email": "a@b.com", "code": "482913", "password": "Pixora2026Abc" }`
响应：`{ "data": { "uid": "1001", "email": "a@b.com", "quota_bytes": 10737418240, "default_album_id": 12 } }`
错误：`BAD_REQUEST`（验证码错误或过期）、`CONFLICT`。

### `POST /api/v1/auth/login` — 公开

请求：`{ "email": "a@b.com", "password": "…", "totp": "123456", "remember": true }`
响应：`{ "data": { "user": {...见 3.1...}, "csrf_token": "…" } }`，同时 `Set-Cookie: pixora_session=…; HttpOnly; SameSite=Lax`。
错误：`INVALID_CREDENTIALS`、`ACCOUNT_LOCKED`（带 `locked_until`）、`ACCOUNT_PENDING`、`ACCOUNT_SUSPENDED`、`ACCOUNT_BANNED`、`IP_BLOCKED`、`RATE_LIMITED`、`TOTP_REQUIRED`（400，附 `totp_required: true` 表示需要补输动态码）。

### `POST /api/v1/auth/logout`

清除服务端会话与 Cookie。响应 `{ "data": { "ok": true } }`。

### `POST /api/v1/auth/password/code` — 公开

找回密码发码：`{ "email": "a@b.com" }`。响应同注册发码。

### `POST /api/v1/auth/password/reset` — 公开

`{ "email": "a@b.com", "code": "482913", "password": "新密码" }`，成功后吊销该用户全部会话。

## 3. 当前用户

### 3.1 `GET /api/v1/me`

```json
{ "data": {
  "user": {
    "id": 42, "uid": "1001", "email": "chenyu@example.com", "role": "user",
    "status": "active", "email_verified": true, "created_at": "2026-08-02T00:00:00Z",
    "last_login_at": "2026-10-14T12:28:00Z", "last_login_ip": "203.0.113.18",
    "totp_enabled": true, "login_alert": true, "new_device_guard": true,
    "quota_bytes": 10737418240, "used_bytes": 2576980377,
    "stats": { "album_count": 3, "image_count": 1284, "public_count": 216, "tag_count": 42 },
    "limits": { "api_per_minute": 120, "upload_per_minute": 30, "upload_per_day": 2000,
                "upload_bytes_per_day": 5368709120, "max_file_size": 20971520,
                "max_batch_size": 1000, "unlimited": false }
  },
  "csrf_token": "…"
} }
```

### `PATCH /api/v1/me`

可改：`login_alert`、`new_device_guard`。响应同 3.1 的 `user`。

### `POST /api/v1/me/password`

`{ "current": "…", "password": "新密码" }`。成功后会吊销其他设备会话（保留当前会话）。

### `GET /api/v1/me/sessions`

```json
{ "data": [ { "id": 9, "device": "Chrome · Windows", "ip": "203.0.113.18",
              "created_at": "…", "last_seen_at": "…", "current": true } ] }
```

### `DELETE /api/v1/me/sessions/{id}` 与 `DELETE /api/v1/me/sessions?keep_current=true`

后者吊销除当前会话外的全部会话，响应 `{ "data": { "revoked": 3 } }`。

### `GET /api/v1/me/recovery-codes` / `POST /api/v1/me/recovery-codes`

查看剩余数量 / 重新生成。响应：`{ "data": { "codes": ["…"], "remaining": 8 } }`（生成时返回明文一次）。
说明：恢复码属于二次验证的辅助手段，实现上按「一次性登录码」处理，数量上限 8 个。

### `POST /api/v1/me/totp` — 三步走

- `{ "action": "setup" }` → `{ "data": { "secret": "GEZD…", "otpauth_url": "otpauth://totp/…" } }`
- `{ "action": "enable", "code": "123456" }` → `{ "data": { "enabled": true } }`
- `{ "action": "disable", "code": "123456" }` → `{ "data": { "enabled": false } }`

### `GET /api/v1/me/tokens` / `POST /api/v1/me/tokens` / `DELETE /api/v1/me/tokens/{id}`

创建请求：`{ "name": "PicGo 桌面端", "scopes": ["upload","read"], "expires_in_days": 90 }`（`expires_in_days` 省略或 `null` 表示永不过期；填了就必须是 1–3650 的整数，越界返回 `BAD_REQUEST`）
创建响应（明文只此一次）：

```json
{ "data": { "id": 3, "name": "PicGo 桌面端", "token": "px_live_9f2a7c41e8b3d605…",
            "display": "px_live_9f2a7c41", "scopes": ["upload","read"],
            "created_at": "…", "expires_at": null } }
```

列表项不含 `token` 字段，含 `last_used_at`、`revoked` 布尔。

## 4. 相册与图片

### 4.1 `ImageItem` 形状（所有图片列表复用）

```json
{
  "id": 88214, "title": "雪山日出", "original_name": "IMG_20240912_雪山.jpg",
  "sha256": "a3f9c1b8…", "short_hash": "a3f9c1b8", "ext": "webp", "mime": "image/webp",
  "size": 2411724, "width": 4032, "height": 3024,
  "album_id": 7, "album_name": "旅行", "owner_uid": "1001", "owner_id": 42,
  "visibility": "public", "views": 12, "upload_time": "2026-10-14T12:31:00Z",
  "tags": ["风景", "旅行"], "has_thumb": true,
  "url": "https://img.example.com/images/a3f9…f1a2.webp",
  "thumb_url": "https://img.example.com/images/a3f9…f1a2.webp?thumb=1"
}
```

- `title` 为空时前端回落到 `original_name`
- 未登录用户拿不到 `original_name`（服务端置 null）
- `url` 与 `thumb_url` 是拼好的完整地址；`thumb_url` 仅在 `has_thumb` 为真时有值

### `GET /api/v1/albums` — read

查询参数：`uid`（管理员可指定他人；普通用户传他人 UID 返回 403）、`q`、`public_only`。
响应（不带分页，侧边栏要全量）：

```json
{ "data": {
  "all": { "id": null, "name": "全部图片", "image_count": 1284, "bytes": 2576980377 },
  "albums": [ { "id": 7, "name": "旅行", "slug": "旅行", "image_count": 236,
                "bytes": 612430192, "visibility": "private", "cover_url": "…",
                "sort_order": 0, "updated_at": "…" } ],
  "usage": { "used_bytes": 2576980377, "quota_bytes": 10737418240 }
} }
```

### `POST /api/v1/albums` — write

`{ "name": "旅行", "slug": "旅行", "description": null, "visibility": "private" }`
`slug` 省略时按 `name` 生成（净化非字母数字与中日韩字符）。响应为单个相册对象。
错误：`CONFLICT`（slug 重复）。

### `PATCH /api/v1/albums/{id}` / `DELETE /api/v1/albums/{id}` — write

`DELETE` 是**移进回收站**：相册与它里面的图片一起软删（图片标记为「随相册删除」），
响应 `detached_images` 是带进回收站的图片数。保留期内可以整份恢复（见「回收站」一节）。
**默认相册（slug = `default`）不允许删除**——上传不指定相册时落在它这里，删了没有重建入口，
返回 `BAD_REQUEST`。

`PATCH` 可改 `name`、`description`、`visibility`、`sort_order`、`cover_blob_id`。
`DELETE` 会解除图片与相册的关联（图片本身保留，移入「未分类」）。

### `GET /api/v1/images` — read

查询参数：

| 参数 | 说明 |
|---|---|
| `uid` | 管理员指定用户 |
| `album_id` | 相册筛选；不传即全部 |
| `q` | 关键词，匹配标题与标签 |
| `tag` | 精确标签 |
| `sort` | `new`（默认） / `old` / `size` / `name` / `hot` |
| `page`、`per_page` | 分页，`per_page` 默认 24、上限 100 |
| `ids` | 逗号分隔的一批 id（批量操作前回读用） |

响应：分页信封，`data` 是 `[ImageItem]`。

### `GET /api/v1/images/{id}` — read

单张详情，带额外字段：

```json
{ "data": { "...ImageItem...": "…",
  "storage_driver": "local", "created_at": "…", "original_filename_visible": true,
  "album": { "id": 7, "name": "旅行" }, "siblings": [ { "id": 1, "thumb_url": "…" } ] } }
```

访问非本人的公开图片会累加 `views`（同一会话 24 小时内只算一次）。

### `PATCH /api/v1/images/{id}` — write

`{ "title": "…", "visibility": "public", "album_id": 7, "tags": ["风景"] }`，字段省略即不改，`album_id: null` 表示移出相册。

### `DELETE /api/v1/images/{id}` — write

软删引用并递减引用计数；物理文件在计数归零后由 GC 回收。

### `POST /api/v1/images/batch` — write

批量操作，一个请求做一件事：

```json
{ "action": "delete" | "move" | "tag" | "visibility",
  "ids": [1,2,3], "album_id": 7, "tags": ["风景"], "visibility": "public" }
```

`ids` 最多 1000 个。也可以换成按范围操作（「整相册一键操作」走这条）：

```json
{ "action": "delete", "filter": { "album_id": 7 } }
```

`filter.album_id` 省略或为 `null` 表示不限相册（该账号的全部图片）；范围一次最多 10000 张，
超出只作用于前 10000 张（响应里的 `affected` 是实际处理数）。范围与 `ids` 不能同时给，
同时给时以 `filter` 为准。

响应：`{ "data": { "affected": 3 } }`。

### `GET /api/v1/tags` — read

`?uid=&q=&limit=`，返回 `{ "data": [ { "name": "风景", "usage": 216 } ] }`。
`GET /api/v1/tags?scope=plaza` 返回全站公开标签（广场 chips）。

### `PATCH /api/v1/tags` — write

`{ "action": "rename" | "delete", "from": "风景", "to": "风光" }`。

### `GET /api/v1/fs/tree` — read

同设计稿：`uid`、`album`、`depth`（默认 2，上限 5）。

```json
{ "data": {
  "uid": "1001", "path": "/1001/旅行", "files": 236, "size": 612430192,
  "nodes": [ { "type": "album", "id": 7, "name": "旅行", "slug": "旅行",
               "files": 236, "size": 612430192,
               "images": [ { "id": 1, "name": "a3f9c1b8.webp", "size": 2411724,
                             "upload_time": "…" } ] } ],
  "next_cursor": null
} }
```

## 5. 上传

### `POST /api/v1/blobs/exists` — upload

请求：`{ "hashes": ["a3f9…", "7b21…"] }`（最多 1000 个）
响应：`{ "data": { "exist": ["a3f9…"], "missing": ["7b21…"] } }`

`exist` 的口径是「**调用者自己的图库里已经有一张活着的同内容图片**」，不是「服务器还有这份文件」：
物理文件在最后一张引用被删除后仍会留一小段时间等 GC 回收，若拿它判定，
用户删光图片再传同一批会被整批跳过（图进不来，看起来像删除没删干净）。

### `POST /api/v1/upload` — upload（multipart/form-data）

| 字段 | 必填 | 说明 |
|---|---|---|
| `file` | 是 | 图片本体（字段名固定 `file`） |
| `sha256` | 否 | 客户端算好的哈希，带上可省一次服务端计算 |
| `thumb` | 否 | 客户端生成的 WebP 缩略图，服务端只存不缩放 |
| `album_id` | 否 | 目标相册 |
| `tags` | 否 | 逗号分隔 |
| `visibility` | 否 | 默认 `private` |
| `original_name` | 否 | 原始文件名，仅内部记录 |

响应：

```json
{ "data": { "id": 88214, "job_id": 991, "url": "https://…/images/a3f9….webp",
            "thumb_url": "https://…/images/a3f9….webp?thumb=1",
            "sha256": "a3f9…", "size": 2411724, "dedup": false,
            "storage_type": "local", "fallback_message": null, "tags": ["风景"] } }
```

`dedup = true` 表示服务器已有相同文件，本次没有消耗带宽与磁盘。
`fallback_message` 非空表示远端后端失败已回退本地，前端要提示用户。

### `GET /api/v1/queue/{job_id}` — read

`{ "data": { "id": 991, "status": "done", "dedup": false, "image_id": 88214, "error": null } }`

## 6. 广场

### `GET /api/v1/plaza` — 公开

`q`、`tag`、`sort`（`new` / `hot`）、`page`、`per_page`（默认 24，上限 100）。
响应：分页信封，`data` 为 `[ImageItem]`（`original_name` 为 null）。
若站点关闭广场公开浏览（`plaza_public=false`），未登录调用返回 `FORBIDDEN`。

### `GET /api/v1/random` — 公开

随机取一张公开图片，用来做「随机壁纸 / 每日一图」这类接入。

| 参数 | 说明 |
|---|---|
| `tag` | 只在某个标签里挑 |
| `q` | 关键词过滤，规则同广场搜索 |
| `uid` | 只看某个用户（`u_` 开头的公开 UID）的图片 |
| `format` | 填 `json` 返回 JSON，省略则 302 跳转到图片直链 |

默认行为是 **302 跳转**（不是 301：随机结果不该被浏览器永久缓存；响应带 `Cache-Control: no-store`）。
`?format=json` 时返回：

```json
{ "data": { "url": "https://…/images/a3f9….webp", "thumb_url": "https://…/images/a3f9….webp?thumb=1",
            "id": 88214, "width": 1600, "height": 900, "size": 2411724,
            "ext": "webp", "tags": ["风景"], "owner_uid": "u_1042" } }
```

挑不到符合条件的图片时返回 `NOT_FOUND`。`plaza_public=false` 时端点对未登录调用关闭，与广场一致。

## 7. 服务条款

### `GET /api/v1/terms` — read

```json
{ "data": {
  "current": { "id": 3, "version": "v2.1", "title": "服务条款", "body": "一、…",
               "summary": "新增内容合规条款", "articles": 6,
               "effective_at": "2026-10-01T00:00:00Z", "published_at": "…" },
  "consent": { "agreed_at": "2026-10-01T14:22:00Z", "ip": "203.0.113.18", "version": "v2.1" },
  "required": false,
  "history": [ { "version": "v2.0", "effective_at": "…", "summary": "…", "agreed": true } ]
} }
```

### `POST /api/v1/terms/consent` — write

`{ "version_id": 3 }`，记录同意（只增不改，冗余存版本号）。响应 `{ "data": { "agreed_at": "…" } }`。

## 8. 管理后台

以下全部要求 `role = admin`（会话或 `admin` 范围 Token），越权访问记审计日志。

### 8.1 用户

- `GET /api/v1/admin/users?query=&role=&status=&page=&per_page=` → 分页，`data` 为 `UserRow`：

```json
{ "id": 42, "uid": "1001", "email": "chenyu@example.com", "role": "user",
  "status": "active", "status_label": "正常", "quota_bytes": 10737418240,
  "used_bytes": 2576980377, "image_count": 1284, "rate_custom": false,
  "created_at": "…", "last_login_at": "…" }
```

- `GET /api/v1/admin/users/{id}` → `{ "user": UserRow, "actions": [...], "policy": {...} , "limits": {...} }`
- `POST /api/v1/admin/users` → 管理端直接建号：`{ "email": "…", "password": "…", "role": "user" | "admin" }`
  （`role` 省略即普通用户；邮箱直接标记为已验证，不再发验证邮件；配额取当前默认值，建完可在详情页调）
  响应与 `GET /admin/users/{id}` 同一个形状。
- `PATCH /api/v1/admin/users/{id}` → `{ "email": "…", "password": "…", "role": "user" }`（改邮箱会重置验证状态并写入处置记录；角色改名同时禁止降级最后一个管理员、禁止改自己）
- `POST /api/v1/admin/users/{id}/quota` → `{ "quota_bytes": 53687091200 }`（超额拒绝并说明还差多少）
- `POST /api/v1/admin/users/{id}/status` → 处置：

```json
{ "action": "suspend" | "ban" | "restore",
  "reason": "上传违规内容，违反服务条款第 3 条",
  "duration_days": null, "requirements": "本人手持身份证的清晰照片…",
  "appeal_email": "admin@example.com", "notify": true }
```

响应：`{ "data": { "status": "suspended", "revoked_sessions": 3, "mail_sent": true } }`

- `DELETE /api/v1/admin/users/{id}` → 删除账号（内容随外键级联，物理文件由 GC 回收）
- `GET /api/v1/admin/stats` →

```json
{ "data": { "user_total": 38, "user_new_this_month": 6, "allocated_quota": 450971566080,
            "used_bytes": 138136862720, "image_total": 1548, "storage_bytes": 140788645888,
            "register_open": true } }
```

### 8.2 存储

- `GET /api/v1/admin/storage` →

```json
{ "data": {
  "default_driver": "local",
  "backends": [ { "id": 1, "driver": "local", "label": "本地磁盘", "is_default": true,
                  "enabled": true, "capacity_mode": "auto", "capacity_total": null,
                  "config": { } } ],
  "capacity": { "mode": "auto", "total": 536870912000, "used": 140788645888,
                "probe": { "fs_kind": "network", "fs_name": "fuse（ossfs …）",
                           "fs_type": "0x65735546", "trustworthy": false,
                           "note": "statfs 返回伪值，已降级为手动填写" } },
  "paths": { "data_dir": "/data", "upload_prefix": "images",
             "layout": "/data/{uid}/{相册}/{hash}.{ext}", "source": "PIXORA_DATA_DIR" },
  "distribution": [ { "driver": "local", "label": "本地磁盘", "count": 1284, "bytes": 138136862720 } ],
  "totals": { "count": 1548, "bytes": 140788645888, "dedup_saved_bytes": 45835345920 }
} }
```

- `PUT /api/v1/admin/storage/default` → `{ "driver": "local", "backend_id": 1 }`
- `PUT /api/v1/admin/storage/prefix` → `{ "upload_prefix": "i" }`（改完旧链依然有效）
- `PUT /api/v1/admin/storage/capacity` → `{ "mode": "manual", "manual_total": 536870912000 }`
- `POST /api/v1/admin/storage/backends` → `{ "driver": "github", "label": "GitHub 仓库", "config": { "token": "…", "owner": "…", "repo": "…", "prefix": "images" } }`
- `PATCH /api/v1/admin/storage/backends/{id}` / `DELETE /api/v1/admin/storage/backends/{id}`
- `POST /api/v1/admin/storage/backends/{id}/test` → `{ "data": { "ok": true, "message": "连通 · owner/repo · 分支 main" } }`
  敏感字段在响应里回显为 `"secret_set": true`，不回明文。

### 8.3 配额与容量

- `GET /api/v1/admin/quota` →

```json
{ "data": {
  "capacity": { "mode": "manual", "total": 536870912000, "used": 140788645888,
                "source": "manual", "note": "总额为手填" },
  "allocated": 450971566080, "allocated_percent": 84.0, "remaining": 85899345920,
  "default_quota_bytes": 10737418240, "per_user_max": 107374182400,
  "auto_close": { "enabled": true, "on": 90, "off": 80, "register_open": true,
                  "closed_reason": null },
  "top_users": [ { "uid": "2044", "email": "design@studio.io", "role": "user",
                   "status": "active", "quota_bytes": 53687091200,
                   "used_bytes": 36727829504, "image_count": 742 } ]
} }
```

- `PUT /api/v1/admin/quota` → `{ "capacity_mode": "manual", "manual_total": …, "default_quota_bytes": …,
  "per_user_max": …, "auto_close_enabled": true, "auto_close_on": 90, "auto_close_off": 80 }`
  校验：`所有用户配额合计 ≤ 系统总额`，超出返回 `BAD_REQUEST` 并在 `detail` 里给出还差多少字节。

### 8.4 频率限制

- `GET /api/v1/admin/ratelimit` →

```json
{ "data": {
  "global": { "api_per_minute": 120, "api_per_hour": 2000, "api_read_per_minute": 600,
              "api_burst": 20, "upload_per_minute": 30, "upload_per_hour": 500,
              "upload_per_day": 2000, "upload_bytes_per_day": 5368709120,
              "max_concurrent_uploads": 4, "max_batch_size": 1000, "max_file_size": 20971520 },
  "on_breach": { "respond": true, "audit": true, "temp_ban": true,
                 "threshold": 10, "threshold_minutes": 10, "ban_minutes": 30 },
  "queue_depth": { "threshold": 500, "current": 12 },
  "users": [ { "uid": "2044", "email": "design@studio.io", "role": "user", "status": "active",
               "custom": true, "unlimited": false,
               "api_per_minute": 300, "upload_per_minute": 60, "upload_per_day": 5000,
               "upload_bytes_per_day": 10737418240, "hits_24h": 3 } ],
  "realtime": [ { "uid": "2044", "email": "design@studio.io", "status": "active",
                  "api_minute": 258, "api_minute_limit": 300,
                  "upload_minute": 12, "upload_minute_limit": 60,
                  "upload_day": 348, "upload_day_limit": 2000, "hits_24h": 3 } ]
} }
```

- `PUT /api/v1/admin/ratelimit/global` ← `global` 对象
- `PUT /api/v1/admin/ratelimit/users/{id}` ← `{ "unlimited": false, "api_per_minute": 300, "upload_per_minute": 60,
  "upload_per_day": 5000, "upload_bytes_per_day": …, "api_per_hour": …, "api_read_per_minute": …,
  "api_burst": …, "upload_per_hour": …, "max_concurrent_uploads": …, "max_batch_size": …, "max_file_size": …, "note": "…" }`
- `DELETE /api/v1/admin/ratelimit/users/{id}` → 恢复继承默认
- `PUT /api/v1/admin/ratelimit/on_breach` ← `on_breach` 对象
- `GET /api/v1/admin/ratelimit/logs?uid=&page=` → 限流事件分页（来自审计日志）

### 8.5 IP 规则

- `GET /api/v1/admin/ip` →

```json
{ "data": {
  "policy": { "enabled": true, "trust_proxy": true, "proxy_hops": 1,
              "auto_ban": true, "fail_threshold": 20, "fail_window_minutes": 10, "ban_minutes": 30 },
  "rules": [ { "id": 5, "cidr": "203.0.113.0/24", "kind": "deny", "scope": "all",
               "scope_label": "全站", "note": "批量爬取图片", "hits": 1842,
               "is_auto": false, "expires_at": null, "created_at": "…" } ],
  "count": 7
} }
```

- `POST /api/v1/admin/ip/rules` → `{ "cidr": "…", "kind": "deny", "scope": "all", "note": "…", "expires_at": null }`
- `DELETE /api/v1/admin/ip/rules/{id}` → 亦用于「提前解除」自动规则
- `PUT /api/v1/admin/ip/policy` ← `policy` 对象

### 8.6 系统设置

- `GET /api/v1/admin/settings` →

```json
{ "data": {
  "site": { "name": "Pixora", "base_url": "https://img.example.com",
            "plaza_public": true, "require_login_to_upload": true, "register_enabled": true,
            "plaza_grid_columns": 3, "recycle_retention_days": 30 },
  "upload": { "max_file_bytes": 20971520, "max_batch_size": 1000, "max_pixels": 80000000,
              "queue_depth": 500, "formats": ["jpg","png"] },
  "thumb": { "client": true, "server_fallback": false },
  "mail": { "smtp_host": "smtp.qiye.aliyun.com", "smtp_port": 465,
            "smtp_security": "ssl", "smtp_user": "noreply@example.com",
            "password_set": true, "from_name": "Pixora 管理团队", "from_email": "noreply@example.com",
            "appeal_email": "admin@example.com",
            "disposable_check": true, "mx_check": true, "third_party_check": false,
            "third_party_set": false },
  "appearance": { "background": null }
} }
```

- `PUT /api/v1/admin/settings` ← 上述任意子对象（只写传入的键）
- `POST /api/v1/admin/settings/mail/test` → `{ "to": "admin@example.com" }` → `{ "data": { "ok": true, "message": "…" } }`
- `POST /api/v1/admin/settings/formats` → `{ "formats": ["jpg","png","avif"] }`
- `POST /api/v1/admin/thumbnails/rebuild` → `{ "data": { "queued": 128, "enabled": false } }`
  （服务端兜底关闭时 `enabled = false`、`queued = 0`，并说明原因）

### 8.7 服务条款管理

- `GET /api/v1/admin/terms` → `{ "data": [ { "id": 4, "version": "v2.1", "summary": "…",
  "effective_at": "…", "status": "active", "status_label": "生效中",
  "consent_count": 38, "user_total": 38, "updated_at": "…" } ] }`
- `POST /api/v1/admin/terms` → `{ "version": "v2.2", "title": "服务条款", "body": "…", "summary": "…",
  "status": "draft" | "active", "require_reconsent": true, "effective_at": null }`
- `PATCH /api/v1/admin/terms/{id}`（仅草稿可改）
- `POST /api/v1/admin/terms/{id}/publish` → `{ "data": { "status": "active", "archived": "v2.1" } }`
- `DELETE /api/v1/admin/terms/{id}`（生效中的版本拒绝删除，返回 `BAD_REQUEST` 并说明）

## 9. 安装向导

未安装时（`settings.install.completed` 非真）这组接口可用，安装完成后一律返回 404。

- `GET /api/v1/install/status` → `{ "data": { "installed": false, "step": 2,
  "env": { "data_dir": "/data", "env_source": "PIXORA_DATA_DIR",
           "db_driver": "postgres", "db_dsn_set": true, "master_key_set": true },
  "tables": 0 } }`
- `POST /api/v1/install/database` → `{ "driver": "postgres"|"mysql"|"sqlite", "host": "127.0.0.1",
  "port": 5432, "database": "pixora", "username": "pixora", "password": "…" }`
  → `{ "data": { "ok": true, "message": "PostgreSQL 16.4 · 字符集 UTF8 · 已检测到 0 张表，将执行建表" } }`
- `POST /api/v1/install/admin` → `{ "email": "admin@example.com", "password": "…", "site_name": "Pixora" }`
- `POST /api/v1/install/site` → `{ "base_url": "https://img.example.com", "upload_prefix": "images",
  "smtp": { "host": "…", "port": 465, "security": "ssl", "user": "…", "password": "…", "from_name": "…" } }`
- `POST /api/v1/install/finish` → `{ "data": { "ok": true, "admin_uid": "1000", "login_url": "/login" } }`

## 10. 直链

```
GET /{upload_prefix}/{sha256}.{ext}            # 原图
GET /{upload_prefix}/{sha256}.{ext}?thumb=1    # 缩略图（没有则回落到原图）
GET /{upload_prefix}/{sha256}.{ext}?download=1 # 强制下载（Content-Disposition: attachment）
```

- **还挂着公开引用**的图片可以匿名取，响应头带 `Cache-Control: public, max-age=31536000, immutable`（内容寻址，永不变化）
- **只被私有内容引用**的图片：直链只对「本人（含管理员）」放行，其余返回 404；拿到链接不等于有权限
  本人取到时带 `Cache-Control: private, max-age=300`，避免私人图片在共享缓存里停留
- 响应头带 `ETag: "<sha256>"` 与 `X-Content-Type-Options: nosniff`
- `ext` 与库中记录不一致时返回 404（防止用伪造扩展名探测）
- SVG 额外带 `Content-Security-Policy: sandbox`；不可信来源（非本人且非公开）强制 `attachment`
- 一级目录可在后台改（`/api/v1/admin/storage/prefix`），改完旧链依然有效
- 本地驱动可直接由 Caddy 走 `X-Accel-Redirect` 交给内核发送，此时响应头由本站在前一步设置

## 11. 其它

- `GET /healthz` → `{ "status": "ok", "db": "ok", "uptime_seconds": 1234 }`
- 静态资源：`/`、`/login`、`/albums` 等前端路由全部回落到 `index.html`（SPA 路由）
- 所有写操作返回的 `data` 里都应带上 `csrf_token`（会话续期时前端就地更新）
