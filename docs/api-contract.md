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

- 会话 Cookie：`pixora_session`（HttpOnly + SameSite=Lax；HTTPS 下带 Secure）
- 每个写请求带 CSRF 头：`X-CSRF-Token: <pixora_csrf Cookie 的值>`
- 登录与 `GET /api/v1/me` 会在响应体里带回 `csrf_token`，前端存内存；同时写一个非 HttpOnly 的 `pixora_csrf` Cookie 供刷新后还原

API 客户端：

- `Authorization: Bearer px_live_xxx` 或 `X-API-Key: px_live_xxx`
- Token 权限范围：`upload` / `read` / `write` / `admin`，路由要求声明见各条目
- 创建 Token 时省略 `scopes` 或传空数组，都按最小可用范围 `["upload","read"]` 处理；要改删再加 `write`
- 带上 Token 时不校验 CSRF

### 1.4 限流与响应头

- 鉴权通过后按用户生效策略检查 API 额度：读接口用 `api_read_per_minute`，写接口用 `api_per_minute`；管理员豁免
- 超限返回 429，`code=RATE_LIMITED`，带 `Retry-After` 秒数
- 当前强制的是每分钟额度；`api_per_hour` 与 `api_burst` 还在策略表里，但尚未接入准入逻辑，客户端不要把它们当成已生效的硬限制
- 浏览器会话的读请求只过进程内令牌桶；Token 请求与写操作会落库计数，管理页的“实时用量”据此展示
- 所有 JSON 响应带安全响应头：`X-Content-Type-Options: nosniff`、`X-Frame-Options: SAMEORIGIN`、`Referrer-Policy: strict-origin-when-cross-origin`、`Permissions-Policy`、`Content-Security-Policy: frame-ancestors 'self'`
- 直链与站点资产的响应头见 §10

### 1.5 时间与大小

- 时间一律是 RFC3339 UTC 字符串：`2026-10-14T15:30:00Z`
- 字节数是整数（`size`、`quota_bytes`、`used_bytes`）
- 前端负责把字节数与时间本地化展示

### 1.6 字段命名

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
  "upload_prefix": "images", "login_methods": { "github": false, "google": false, "qq": false, "wechat": false, "phone": false },
  "registration_open": true, "registration_closed_reason": null,
  "register_email_domains": ["example.com"],
  // 站点允许未登录上传时才有这一段：客户端队列按访客账号那套额度限流
  "guest_upload": { "enabled": true, "max_file_bytes": 20971520, "max_batch_size": 1000,
                    "max_concurrent_uploads": 5, "queue_depth": 500, "formats": ["jpg","png"] },
  "plaza_public": true, "plaza_grid_columns": 3, "plaza_auto_refresh_seconds": 5, "recycle_retention_days": 30,
  "formats": ["jpg","png","gif","webp","avif","heic","tiff","bmp","svg"],
  "max_file_bytes": 20971520, "max_batch_size": 1000, "max_pixels": 80000000,
  "thumb_client": true, "thumb_server_fallback": false,
  "appeal_email": "admin@example.com", "background": null,
  // 站点默认头像：账号自己没设头像时用它（新注册的账号也走这条），没配过是 null
  "default_avatar": null,
  "installed": true, "terms": { "required": false, "version": "v2.1", "articles": 6 }
} }
```

`terms.required` 表示「当前登录用户是否必须先同意条款」；未登录时为 `false`。

`plaza_auto_refresh_seconds` 控制广场的自动漫游：`0` 关闭，正数表示每隔多少秒换一批公开图片；服务端把正数钳在 5–3600 秒，默认 5。

### 收件域名校验（反临时邮箱的第三层）

注册、找回密码、换绑邮箱、登录码、关闭二次验证的邮箱码——**所有发自本站的验证邮件**在发信之前
都会查一遍收件域名：先看 MX 记录，没有 MX 才按 RFC 5321 回落到域名自身的 A/AAAA；发布 null MX
（`MX 0 .`）的域名是明确宣告不收信，直接拒。域名收不到信时返回 `FORBIDDEN`（"域名没有邮件服务器
记录"），查询本身失败或超时返回 `INTERNAL`（"判断不了"），两种都**不发信**——退信最后都会堆在配置
的发信邮箱里。后台「邮件与注册验证」里的「收件域名校验」开关能整层关掉，内网自建邮件服务需要它。

注意这一层能判的是**域名**收不收信（不存在、没配 MX、null MX 这些）；同一个域里某个具体信箱存不
存在，发信前没有任何可靠办法知道——只能等对方服务器在 SMTP 阶段回 5xx。那种回复会被当作永久失败
（`收件服务器拒收`）直接落定，不再重试，用户与管理员看到的就是服务器原话。

### `POST /api/v1/auth/register/code` — 公开

请求：`{ "email": "a@b.com" }`
响应：`{ "data": { "sent": true, "expires_in": 600, "resend_after": 60 } }`
错误：`BAD_REQUEST`（邮箱格式非法）、`FORBIDDEN`（一次性邮箱 / MX 校验失败）、`CONFLICT`（邮箱已注册）、`RATE_LIMITED`（同邮箱 5 次/小时或同 IP 20 次/小时）、`ACCOUNT_BANNED`（同邮箱曾被封禁）。

### `POST /api/v1/auth/register` — 公开

请求：`{ "email": "a@b.com", "code": "482913", "password": "Pixora2026Abc" }`
响应：`{ "data": { "uid": "1001", "email": "a@b.com", "quota_bytes": 10737418240, "default_album_id": 12 } }`
错误：`BAD_REQUEST`（验证码错误或过期）、`CONFLICT`。

### `POST /api/v1/auth/login` — 公开

请求：`{ "email": "a@b.com", "password": "…", "totp": "123456", "recovery_code": "1234-5678", "remember": true }`

`totp` 与 `recovery_code` 二选一。开了二次验证的账号，第一次提交（两者都不带）会拿到
`TOTP_REQUIRED`，前端据此翻到第二步；第二步把动态码或一枚恢复码带上来再提交。

响应：`{ "data": { "user": {...见 3.1...}, "csrf_token": "…" } }`，同时 `Set-Cookie: pixora_session=…; HttpOnly; SameSite=Lax`。
错误：`INVALID_CREDENTIALS`、`ACCOUNT_LOCKED`（带 `locked_until`）、`ACCOUNT_PENDING`、`ACCOUNT_SUSPENDED`、`ACCOUNT_BANNED`、`IP_BLOCKED`、`RATE_LIMITED`、`TOTP_REQUIRED`（400，附 `totp_required: true`，表示还差一步动态码）。

### `POST /api/v1/auth/login/email-code` — 公开

登录第二步的备选：验证器不在手边时，用邮箱验证码替代动态码。

请求 `{ "email": "a@b.com" }` → `{ "data": { "ok": true, "message": "验证码已发送（若该邮箱可用）…" } }`。
账号不存在、或没开二次验证时答话一样（不给「这个邮箱注册过没有」的探针）；发信没配好时
`CONFIG` 错误说明缺哪一项。拿到码之后回到 `POST /auth/login`，把 6 位数字放在 `totp` 里、
`code_method: "email"`。

### `POST /api/v1/auth/logout`

清除服务端会话与 Cookie。响应 `{ "data": { "ok": true } }`。

### `POST /api/v1/auth/password/code`

发出的邮件里同时带一条重置链接（`{base_url}/#/reset-password?token=pxr_…`）与那串验证码。
链接里放的是**一次性随机令牌**，不是邮箱与验证码——那两个是明文参数，转发一次链接就等于
把「谁的账号 + 当前有效的验证码」一起递了出去。令牌 1 小时内有效，用一次即废；站点地址
没配时只给验证码，不硬塞一个点不开的链接。

发信能力（SMTP 是否配置）在查账号之前先判，所以「发不出去」这句话对所有人一样，
不构成「这个邮箱注册过没有」的探针。

### `GET /api/v1/auth/password/link?token=pxr_…`

重置页先换一句「给哪个账号设密码」：`{ "data": { "email": "u***@e***.com", "expires_at": "…" } }`。
邮箱是打了码的。链接无效、用过、过期都返回 `BAD_REQUEST` 并附一句能照着做的话。

### `POST /api/v1/auth/password/link/reset` ← `{ "token": "pxr_…", "password": "…" }`

点邮件里的链接设新密码。令牌在验证通过后立即作废；成功会吊销该用户全部会话并清掉
`must_change_password`（密码是用户自己选的）。

 — 公开

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
    "avatar": "/images/9f2a7c41.png",
    // 管理员直接设过密码、用户还没自己改：为 true 时只放行 /me 与改密码
    "must_change_password": false,
    "quota_bytes": 10737418240, "used_bytes": 2576980377,
    "stats": { "album_count": 3, "image_count": 1284, "public_count": 216, "tag_count": 42 },
    "limits": { "api_per_minute": 120, "upload_per_minute": 30, "upload_per_day": 2000,
                "upload_bytes_per_day": 5368709120, "max_file_size": 20971520,
                "max_batch_size": 1000, "max_pixels": 80000000,
                "formats": ["jpg", "png", "webp"], "max_concurrent_uploads": 4,
                "unlimited": false },
    // 仅管理员可见：安装时创建的那个管理员账号的 UID
    "install_admin_uid": "1000"
  },
  "csrf_token": "…"
} }
```

### `PATCH /api/v1/me`

可改：`login_alert`、`new_device_guard`、`avatar`。响应同 3.1 的 `user`。

`avatar` 接受站内直链（`/images/…`，也接受本站完整地址，会折成相对路径）、外链，空串或
`null` 表示清掉。**换掉（或清掉）本地上传的那张头像时，服务端会顺手清理它**：只有自己名下、
带 `avatar` 标签的那张会被删；同一份内容还有别的引用时只删这条引用，物理文件留给另一条
（引用计数归零才回收）。手动把相册里某张照片的直链填成头像的人不受影响——那种图没有标签。

`avatar` 是字符串或 `null`：站内直链会在入库前折成相对路径（`/images/x.png`），
换域名、加反代前缀都不会让满库头像裂掉；`/` 开头的路径原样存，外链（`http(s)://`）
原样存；空串与 `null` 都是「清掉」。上传头像不另开端点：走普通 `POST /upload`
（不指定相册即落到默认相册、带上 `avatar` 标签），把响应里的 `url` 交给这个接口。

### 邮件模板（管理员）

- `GET /api/v1/admin/mail/templates` → 全部模板：`name`、`label`、`when`（什么时候发）、
  `vars`（可用占位符）、当前 `subject`/`body`、`customized`（是否改过）、`builtin_subject`/`builtin_body`。
- `PUT /api/v1/admin/mail/templates/{name}` ← `{ "subject": "…", "body": "…" }`。
  占位符必须在 `vars` 里，写错了返回 `BAD_REQUEST` 并列出可用的那几个——
  拼错的变量渲染时会被静默清掉，只有收信人会发现少一段话。
- `DELETE /api/v1/admin/mail/templates/{name}` → 删掉自定义文案，回到内置。

### `POST /api/v1/me/totp` 的 `send_disable_code`

`{ "action": "send_disable_code" }` → 往账号邮箱发一个 6 位验证码，让「验证器丢了」的人
还能关闭二次验证。响应 `{ "ok": true, "delivered": true, "message": "邮件已投递" }`；
发不出去时 `delivered: false` 且 `message` 说明原因。`disable` 接受动态码或这个邮箱验证码，
任一对上即关闭。

### `POST /api/v1/me/password`

`{ "current": "…", "password": "新密码" }`。成功后会吊销其他设备会话（保留当前会话），
并清掉 `must_change_password`——管理员直接设过密码的账号改完这一下就恢复正常使用。

「必须改密码」状态下这个端点是放行的（另一个是 `GET /me`），其余接口一律 403。

### `POST /api/v1/me/email/code` — write

换绑邮箱的第一步：`{ "email": "新邮箱", "password": "当前密码" }`。

验证码发到**新**地址。要当前密码是因为邮箱是找回账号的凭据，光有会话不该能把它改走；
新地址的域名同样受注册白名单约束，已被别的账号占用时回 409。响应与找回密码发码同一形状：
`{ "data": { "ok": true, "delivered": true, "detail": null, "message": "…" } }`——发不出去时
`delivered` 为 false 且 `message` 说明卡在哪一步，绝不假装已发送。

### `POST /api/v1/me/email` — write

换绑邮箱的第二步：`{ "email": "新邮箱", "code": "482913" }`。

验码通过后地址立即生效（新地址标记为已验证），新旧两个地址各收到一封变更通知；
响应 `{ "data": { "email": "新邮箱", "delivered": …, "message": … } }`。这一步会写一条
`account_email_change` 审计（`via: "self"`，见「系统日志」），管理员在后台看得到是谁改的。

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
- `{ "action": "enable", "code": "123456" }` → `{ "data": { "enabled": true, "codes": ["1234-5678", …] } }`。
  开启的同一刻发一组恢复码（8 个）并在响应里给出明文——库里只存哈希，这份明文只出现这一次：
  验证器丢了的时候，它们是唯一的自助进门凭据。`disable` 时 `codes` 为 `null`（关掉二次验证会清空恢复码）
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
  "thumb_url": "https://img.example.com/images/a3f9…f1a2.webp?thumb=2"
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
相册的 `visibility` 只管「这个相册能不能被别人浏览」，**不会**连带改动相册内图片的
`visibility`（那一个是广场与直链的开关，见第 10 节）：前端在把相册改成私有时会问一句要不要
用 `POST /images/batch`（`action: "visibility"`、`filter: { "album_id": … }`）把里面公开的图片
一起改私有。
`DELETE` 把相册与其中的图片一起移进回收站（保留期内可整份恢复）；相册行被彻底删掉时，
里面的图片改挂到主人的默认相册——图片永远有个归属，「未分类」这个状态已经不对外了。

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

`{ "title": "…", "visibility": "public", "album_id": 7, "tags": ["风景"] }`，字段省略即不改。
`album_id: null` 是「不指定」——落点是主人的默认相册，不再有「移出相册」那种没有归属的状态。

### `DELETE /api/v1/images/{id}` — write

软删引用并递减引用计数；物理文件在计数归零后由 GC 回收。

### 未登录上传（游客）

站点把「拒绝游客上传」关掉时，`POST /upload` 与 `POST /blobs/exists` 接受未登录的请求：

- 上传结果落到一个固定的访客账号（`uid` 是 `0000`，邮箱 `visitor@pixora.invalid`，
  密码哈希写成一个永远验不过去的值）名下。配额、回收站、内容审核、按用户的频率限制
  都跟着它走——管理员可以在「用户管理」里给这个账号单独调配额，或者干脆暂停它来
  按住游客上传。
- 这个账号不接受登录：`POST /auth/login` 对它回 403，管理员的改密码、改角色与「切换登录」
  同样被拒（调配额、暂停封禁不受影响）。管理端用户对象带 `"guest": true` 的就是它。
- 它的邮箱是个**记号**（认「这只相册是谁的」用的）：管理员可以改（见 `PATCH /admin/users/{id}`），
  但那不验证、不发变更通知、也不查域名收不收得到信——它既不登录，也收发不了信。
- 它也不会被删：`DELETE /admin/users/{id}` 对它回 403——删掉等于把全站游客的图一次性
  清空，而且下一次游客上传又会把它建回来。要停止游客上传，关掉「允许游客上传」或者
  暂停这个账号；开关来回切不会动它，它名下的相册与图片都保留。
- 客户端在入队口就按 `GET /site` 下发的 `guest_upload` 判许可：站点关着游客上传时，
  未登录的人选中的文件不进队列，本机不再计算 SHA-256，也不会发出预检——省掉两头的
  无用功。这一道只管少做无用的事，许可本身仍在服务端。
- 校验方式不变：访客的 CSRF Cookie 由 `GET /site` 下发，写请求照旧要带
  `X-CSRF-Token` 那双提交头。
- 每次上传都在 `images.upload_ip` / `images.upload_agent` 两列记下来源，审计日志里
  也带 `guest` 与 `agent` 两个字段。
- **访客上传只有公开**：那个账号登不进来，私有图片传完谁也看不见（连它自己都进不去），
  所以服务端按账号把 `visibility` 定成 `public`，客户端传什么都不作数；上传面板上也不摆
  可见性选择。
- 站点开着「拒绝游客上传」时，未登录的上传与预检都返回 401。

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

### 回收站（保留期内可恢复）

图片与相册删除都是先软删；保留天数见 `/site` 的 `recycle_retention_days`（0 = 关闭回收站，
删除即彻底删除）。回收站里的图片**只有本人（与管理员）取得到直链**（见第 10 节）。

- `GET /api/v1/recycle/images` — 与普通图片列表同形状（分页、`ImageItem`），另带
  `meta.retention_days`
- `POST /api/v1/recycle/images/restore` ← `{ "ids": [1,2,3] }` → `{ "data": { "restored": 3 } }`
- `POST /api/v1/recycle/images/purge` ← `{ "ids": [1,2,3] }` → `{ "data": { "purged": 3 } }`（彻底删除，
  引用计数归零的物理文件由 GC 回收）
- `POST /api/v1/recycle/images/empty` → `{ "data": { "purged": 12 } }`（清空回收站）
- `GET /api/v1/recycle/albums` → `{ "data": { "albums": [ …同 albums 形状… ] } }`，
  每项带该相册在回收站里的图片数与占用
- `POST /api/v1/recycle/albums/{id}/restore` → `{ "data": { "restored_images": 12 } }`
  （相册与当初随它一起进回收站的那批图片整份回来）
- `DELETE /api/v1/recycle/albums/{id}` → `{ "data": { "purged_images": 12 } }`（彻底删除相册与其中的图片）

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
| `sha256` | 否 | 客户端算好的哈希，用于和服务端实测值对账；服务端仍会边收边算，不能靠它跳过校验 |
| `thumb` | 否 | 客户端生成的 WebP/PNG/JPEG 缩略图（最长边建议 1024px）；服务端只存不缩放，会校验体积、像素与尺寸 |
| `album_id` | 否 | 目标相册 |
| `tags` | 否 | 逗号分隔，单标签最多 32 字，最多保留 20 个 |
| `visibility` | 否 | 默认 `private`；**访客上传恒为公开**，服务端按账号定，不听这个值 |
| `original_name` | 否 | 原始文件名，仅内部记录；服务端会去掉路径与控制字符，截到 255 字符 |
| `reuse_existing` | 否 | `1` / `true` 表示「这份内容我图库里已有就复用它」，不新增一条引用（头像那套用它） |

SVG 会先净化再入库：`<script>`、`on*` 事件属性、外部 `href/src`、`foreignObject` 与带命名空间前缀的脚本标签都会被去掉，正常图形保留。净化后哈希会重算，因此不要拿上传前的客户端哈希去直链里找这张图。

响应：

```json
{ "data": { "id": 88214, "job_id": 991, "url": "https://…/images/a3f9….webp",
            "thumb_url": "https://…/images/a3f9….webp?thumb=2",
            "sha256": "a3f9…", "size": 2411724, "dedup": false, "reused": false,
            "storage_type": "local", "fallback_message": null, "tags": ["风景"] } }
```

`dedup = true` 表示服务器已有相同文件，本次没有消耗带宽与磁盘。
`reused = true` 表示这次没有新建图片，复用了图库里已有的那张（`id` 就是它；标签只加不减）。
`fallback_message` 非空表示远端后端失败已回退本地，前端要提示用户。

### `GET /api/v1/queue/{job_id}` — read

`{ "data": { "id": 991, "status": "done", "dedup": false, "image_id": 88214, "error": null } }`

## 6. 广场

### `GET /api/v1/plaza` — 公开

`q`、`tag`、`sort`、`seed`、`page`、`per_page`（默认 24，上限 100）。
`sort` 认 `new` / `newest`、`old` / `oldest`、`largest` / `size`、`smallest`、`name_asc`、`name_desc`、`type_asc`、`hot` / `hottest`，以及默认的 `shuffle`；`shuffle` 用 `seed` 固定一次洗牌，翻页时带上同一个 seed 就不会重复或漏图。
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
{ "data": { "url": "https://…/images/a3f9….webp", "thumb_url": "https://…/images/a3f9….webp?thumb=2",
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

`required=true` 表示当前账号还没同意生效版本。前端登录后会弹通告并引导到这里；**通用鉴权目前不会返回 `TERMS_REQUIRED` 强制拦截**，API Token 可以绕过这条提示。若条款需要法律意义上的强制同意，部署侧需要在服务端补拦截，或在注册/建 Token 的流程里绑定同意记录。

## 8. 管理后台

以下全部要求 `role = admin`（会话或 `admin` 范围 Token），越权访问记审计日志。

### 8.1 用户

- `GET /api/v1/admin/users?query=&role=&status=&page=&per_page=` → 分页，`data` 为 `UserRow`：

```json
{ "id": 42, "uid": "1001", "email": "chenyu@example.com", "role": "user",
  "status": "active", "status_label": "正常", "quota_bytes": 10737418240,
  "used_bytes": 2576980377, "image_count": 1284, "rate_custom": false,
  "created_at": "…", "last_login_at": "…",
  // 是不是那个拒不登录的访客账号（见「未登录上传（游客）」）
  "guest": false }
```

- `GET /api/v1/admin/users/{id}` → `{ "user": UserRow, "actions": [...], "policy": {...} , "limits": {...} }`
- `POST /api/v1/admin/users` → 管理端直接建号：`{ "email": "…", "password": "…", "role": "user" | "admin" }`
  （`role` 省略即普通用户；邮箱直接标记为已验证，不再发验证邮件；配额取当前默认值，建完可在详情页调）
  响应与 `GET /admin/users/{id}` 同一个形状。
- `PATCH /api/v1/admin/users/{id}` → `{ "email": "…", "password": "…", "role": "user" }`（改邮箱会重置验证状态并写入处置记录；角色改名同时禁止降级最后一个管理员、禁止改自己；访客账号不接受登录，这三样对它一律 403）

  改邮箱另外写一条 `account_email_change` 审计（`via: "admin"`，含操作人邮箱快照），
  与用户自助换绑同一个动作名——「系统日志」里两条都在，靠 `via` 分得清是谁改的。
  `DELETE /admin/users/{id}` 同样写 `account_delete`，账号删掉之后这条记录还查得到。

  传了 `password` 时：新密码随邮件发给用户，并置上 `must_change_password`——他下次登录
  会被要求自己改一个，改完之前账号只能访问 `/me` 与 `/me/password`。响应里多一个 `mail`
  字段说明邮件投递结果：`{ "delivered": true, "detail": null }`；`delivered` 为 false 时
  `detail` 说明卡在哪一步（例如 SMTP 未配置，邮件留在发件箱等重试）。
- `POST /api/v1/admin/users/{id}/password-reset-mail` → 管理员代发重置邮件

  与「直接设密码」是两条路：这条把带链接的邮件发给用户，由他自己设。响应
  `{ "data": { "ok": true, "delivered": false, "detail": "SMTP 未配置，邮件已留在发件箱…",
  "message": "…" } }`——`message` 是可以直接显示给管理员的一句话。
- `POST /api/v1/admin/users/{id}/impersonate` → 以该账号的身份开一个新会话（响应与登录同形状，
  另带 `impersonator`），同时下发 `pixora_impersonator` Cookie 记住原来的管理员会话。
  安装账号不能被切换，访客账号也不能（见第 4 节的游客一节）。
- `POST /api/v1/admin/impersonate/stop` → 结束切换，恢复成 `pixora_impersonator` 里那位管理员。
- `POST /api/v1/admin/users/{id}/quota` → `{ "quota_bytes": 53687091200 }`（超额拒绝并说明还差多少）
- `POST /api/v1/admin/users/{id}/status` → 处置：

```json
{ "action": "suspend" | "ban" | "restore",
  "reason": "上传违规内容，违反服务条款第 3 条",
  "duration_minutes": 43200, "requirements": "本人手持身份证的清晰照片…",
  "appeal_email": "admin@example.com", "notify": true }
```

`duration_minutes` 为 `null` 或 0 表示永久，只能人工解封；填了由鉴权入口在到期时自动恢复。

响应：`{ "data": { "status": "suspended", "revoked_sessions": 3, "mail_sent": true, "auto_restore_at": "2026-11-15T…" } }`

- `DELETE /api/v1/admin/users/{id}?confirm=<账号邮箱>` → 删除账号

账号名下的图片、相册、令牌、会话随外键级联删除，引用计数一并还回去；不再被别人引用的
物理文件当场回收（共享实体与远端失败留给后台 GC）。这一步不可逆，所以要把账号邮箱原样
填进 `confirm`。安装时创建的管理员账号删不得，其他管理员也不能改它。

响应：`{ "data": { "ok": true, "email": "…", "uid": "…", "blobs": 12, "reclaimed_files": 10 } }`
- `GET /api/v1/admin/stats` →

```json
{ "data": { "user_total": 38, "user_new_this_month": 6, "allocated_quota": 450971566080,
            "used_bytes": 138136862720, "image_total": 1548, "storage_bytes": 140788645888,
            "register_open": true } }
```

### 8.2 存储

- `GET /api/v1/admin/storage` →

  响应里带 `thumb_rebuild`：`{ "running": false, "done": 0, "total": 0 }`——缩略图重算的进度。
- `POST /api/v1/admin/storage/thumbs/rebuild` → 把所有图片的缩略图按当前尺寸（最长边约 1024px）
  重新生成一遍。`{ "data": { "ok": true, "running": true, "done": 0, "total": 159 } }`，
  后台分批做，前端轮询 `GET /admin/storage` 的 `thumb_rebuild` 看进度。
  客户端早期按 400px 生成缩略图，宽屏上放大两三倍就是糊的——老图靠这一步修。

```json
{ "data": {
  "default_driver": "local",
  "backends": [ { "id": 1, "driver": "local", "label": "本地磁盘", "is_default": true,
                  "enabled": true, "capacity_mode": "auto", "capacity_total": null,
                  "config": { } } ],
  "capacity": { "mode": "auto", "total": 536870912000, "used": 140788645888,
                "host_used": 510252462080, "host_available": 26618449920, "source": "auto",
                "note": "ntfs（固定盘） · 总容量 500.0 GB · 已用 475.2 GB · 可用 24.8 GB",
                "probe": { "fs_kind": "local", "fs_name": "ntfs（固定盘）",
                           "fs_type": "0x3", "trustworthy": true,
                           "note": "ntfs（固定盘） · 总容量 500.0 GB · 已用 475.2 GB · 可用 24.8 GB" } },
  "paths": { "data_dir": "/data", "upload_prefix": "images",
             "layout": "/data/{uid}/{相册}/{hash}.{ext}", "source": "PIXORA_DATA_DIR" },
  "distribution": [ { "driver": "local", "label": "本地磁盘", "count": 1284, "bytes": 138136862720 } ],
  "totals": { "count": 1548, "bytes": 140788645888, "dedup_saved_bytes": 45835345920 }
} }
```

- `POST /api/v1/admin/storage/gc` → 立刻扫一遍孤儿文件与过期计数 → `{ "data": { "removed": 3, "pending": 0 } }`
  （`removed` 是这次删掉的物理文件数，`pending` 是仍在宽限期里、下次再收的）
- `PUT /api/v1/admin/storage/default` → `{ "driver": "local", "backend_id": 1 }`
- `PUT /api/v1/admin/storage/prefix` → `{ "upload_prefix": "i" }`（改完旧链依然有效）
- `PUT /api/v1/admin/storage/capacity` → `{ "mode": "manual", "manual_total": 536870912000 }`
- `POST /api/v1/admin/storage/backends` → `{ "driver": "github", "label": "GitHub 仓库", "config": { "token": "…", "owner": "…", "repo": "…", "prefix": "images" } }`
- `PATCH /api/v1/admin/storage/backends/{id}` / `DELETE /api/v1/admin/storage/backends/{id}`
- `POST /api/v1/admin/storage/backends/{id}/test` → `{ "data": { "ok": true, "message": "连通 · owner/repo · 分支 main" } }`
  敏感字段在响应里回显为 `"secret_set": true`，不回明文。

`capacity` 里有三个容量数字，口径不同别混用：`total` 是系统总额（自动模式下即磁盘容量）；
`used` 是本项目占用（库内 blob 汇总，配额那条线用）；`host_used` / `host_available`
是宿主机整盘已用与真实剩余空间，只有 `mode: "auto"` 且探测可信时才有这两个字段，
其余情况为 `null`。磁盘满没满看整盘——`host_used` 里包含宿主机上与本项目无关的占用。

### 8.3 配额与容量

- `GET /api/v1/admin/quota` →

```json
{ "data": {
  "capacity": { "mode": "auto", "total": 536870912000, "used": 140788645888,
                "host_used": 510252462080, "host_available": 26618449920,
                "source": "auto", "note": "ntfs（固定盘） · 总容量 500.0 GB · 已用 475.2 GB · 可用 24.8 GB" },
  "allocatable": 26618449920,
  "allocated": 21474836480, "allocated_percent": 80.7, "remaining": 5143613440,
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
  校验：`所有用户配额合计 ≤ 可分配上限`（`allocatable` = `total` − `host_used`；手填总额或网络挂载
  降级时 `host_used` 为 `null`，上限就是 `total`），超出返回 `BAD_REQUEST` 并在 `detail` 里给出
  还差多少字节。已经越界的存量状态下，单用户调配额只拦「往上加」，调低放行——磁盘占用会把
  上限压低，全拦的话管理员就回不到合规状态了。
- 自动关闭注册的占用率按 `host_used / total` 判定（`host_used` 为 `null` 时退回 `used / total`），
  关闭原因里会写明口径（「宿主机磁盘占用」或「本项目占用」）。这段口径只出现在管理端
  （配额页与系统设置）：**面向访客的 `registration_closed_reason` 只说「注册入口已暂停，
  重新开放时间待定」**，不带占用率、阈值与自动恢复的承诺。
- `auto_close` 里另外两项是给界面用的：`percent`（当前占用率，与判定同口径，算不出时是
  `null`）与 `manual_override`（管理员手动开过注册）。二者一起让「已超阈值但注册是开的」
  这句话能说准——它只有一种成因，就是管理员手动开的。
- 自动关闭与系统设置里的注册开关是一对：
  - 容量触顶自动关闭时，**系统设置里的「允许用户注册」也会被关掉**（管理员看到的必须与
    真实状态一致），并在设置响应里带 `register_auto_closed: true` 与详细口径
    `register_closed_reason`；
  - 占用率跌破恢复阈值时自动重开，并把开关还原成打开；
  - **管理员手动动过开关之后由他接管**：手动打开会立一个覆盖标记
    （`register_auto_close_override`），容量恢复到恢复阈值以下之前这一层不再插手——
    开关刚打开就被按回去会让人以为开关坏了；手动关闭同样接管，容量恢复也不会替他重开。
    设置响应里的三个字段（`register_enabled` / `register_auto_closed` /
    `register_auto_close_override`）就是这套状态机对外的样子。
  - 覆盖期结束的条件是**占用率落到恢复阈值以下**：那时标记清掉，之后占用再涨过触发阈值
    就重新自动关闭。也就是说「手动开启 → 占用恢复 → 再次超阈值」这一圈会自然重来，
    不需要管理员再动一次开关。
  - 重新打开「容量触顶自动关闭」这项策略本身（`auto_close_enabled` 由关变开）会清掉旧闩锁
    与覆盖，让下一次判定从当下的占用率重新算。
- `PUT` 成功时**回传与 `GET` 同一个形状**（保存后的整页生效值）：界面拿它就地重画，
  不必再拉一次，也不会出现「保存成功但页面还是旧数字」。容量与自动关闭那几项要看
  服务端重算后的结果，返回体里就是那份结果。

### 8.4 频率限制

- `GET /api/v1/admin/ratelimit` →

```json
{ "data": {
  "global": { "api_per_minute": 120, "api_per_hour": 2000, "api_read_per_minute": 600,
              "api_burst": 20, "upload_per_minute": 30, "upload_per_hour": 500,
              "upload_per_day": 2000, "upload_bytes_per_day": 5368709120,
              "max_concurrent_uploads": 4, "max_batch_size": 0, "max_file_size": 0,
              "max_pixels": 0, "formats": [] },
  "upload_policy": { "max_file_bytes": 20971520, "max_batch_size": 1000,
                     "max_pixels": 80000000, "queue_depth": 500,
                     "formats": ["jpg", "png", "gif", "webp", "avif", "heic", "tiff", "bmp", "apng", "svg"] },
  "on_breach": { "respond": true, "audit": true, "temp_ban": true,
                 "threshold": 10, "threshold_minutes": 10, "ban_minutes": 30 },
  "queue_depth": { "threshold": 500, "current": 12 },
  "users": [ { "uid": "2044", "email": "design@studio.io", "role": "user", "status": "active",
               "custom": true, "unlimited": false,
               "api_per_minute": 300, "upload_per_minute": 60, "upload_per_day": 5000,
               "upload_bytes_per_day": 10737418240, "hits_24h": 3,
               "upload": { "max_file_size": 52428800, "max_batch_size": 1000,
                           "max_pixels": 40000000, "formats": ["jpg", "webp"],
                           "max_concurrent_uploads": 2 } } ],
  "realtime": [ { "uid": "2044", "email": "design@studio.io", "status": "active",
                  "api_minute": 258, "api_minute_limit": 300,
                  "upload_minute": 12, "upload_minute_limit": 60,
                  "upload_day": 348, "upload_day_limit": 2000, "hits_24h": 3 } ],
  "download_policy": { "anon_per_minute": 60, "anon_per_day": 1000 }
} }
```

- 下载限额分两套：**登录用户**在策略里（`download_per_minute` / `download_per_day`，可按用户覆盖，
  `0` = 不限），**未登录的游客**没有账号可挂，那两格在站点设置里（按来源 IP 计数）。
  `PUT /admin/ratelimit` 一并接受：`{"download_per_minute": 120, "download_per_day": 5000,
  "download_anon_per_minute": 60, "download_anon_per_day": 1000}`。计的是带 `?download=1` 的直链
  请求（页面上显示的图片不计）；管理员不受限。
- 上传策略里的 `max_file_size` 支持**按用户单独设**：`PUT /admin/ratelimit/users/{uid}` 给
  `max_file_size`（字节，`0` = 跟随全局），生效值出现在 `GET /me` 的 `limits.max_file_size`；
  上传时既拦在收流那一步，也通过 `/me` 让客户端队列提前拒绝。

当前版本的实际执行口径：

- 读/写 API 的**每分钟**额度在鉴权后强制，超限 429 + `Retry-After`；管理员豁免。
- `api_per_hour` 与 `api_burst` 仍在策略表与界面里，但没有接入准入逻辑，不能当成已生效的限制。
- 浏览器会话的读请求只过进程内令牌桶，不落库；Token 请求与写操作落库计数，供 `realtime` 展示。
- 上传额度按实际落盘的新文件计数，去重跳过的不算；下载额度只算 `?download=1`。
- 多实例部署时进程内令牌桶与下载分钟桶各自独立，实际额度会按实例数放大。

- `PUT /api/v1/admin/ratelimit/global` ← `global` 对象
- `PUT /api/v1/admin/ratelimit/users/{uid}` ← `{ "unlimited": false, "api_per_minute": 300, "upload_per_minute": 60,
  "upload_per_day": 5000, "upload_bytes_per_day": …, "api_per_hour": …, "api_read_per_minute": …,
  "api_burst": …, "upload_per_hour": …, "max_concurrent_uploads": …, "max_batch_size": …, "max_file_size": …,
  "max_pixels": …, "formats": ["jpg", "webp"], "note": "…" }`
- `DELETE /api/v1/admin/ratelimit/users/{uid}` → 恢复继承默认
- `PUT /api/v1/admin/ratelimit/on_breach` ← `on_breach` 对象
- `GET /api/v1/admin/ratelimit/logs?uid=&page=` → 限流事件分页（来自审计日志）
- `GET /api/v1/admin/logs?action=&uid=&query=&page=&per_page=` → **账号历史**（后台「系统日志」）：

```json
{ "data": [ { "id": 1012, "action": "account_email_change", "action_label": "更换邮箱",
              "target_uid": "1001", "email": "chenyu@example.com",
              "actor_kind": "self", "actor_label": "本人（chenyu@example.com）",
              "actor_id": 42, "ip": "203.0.113.18",
              "detail": { "from": "old@example.com", "to": "chenyu@example.com", "via": "self" },
              "created_at": "…" } ] }
```

  覆盖的动作：`register`、`account_email_change`、`account_delete`、`account_suspend`、
  `account_ban`、`account_restore`、`temp_ban`、`password_reset`、`login_success`、
  `login_fail`、`login_blocked`、`logout`——`action` 留空就是这一组，也可以给逗号分隔的
  一串动作名精确筛选。`uid` 按 UID 精确筛，`query` 在 UID、详情与 IP 里找关键词。

  `actor_kind` 说明这一步是谁做的：`self`（本人）/ `admin`（管理员，`actor_label` 里带邮箱）/
  `system`（系统自己，例如累计违规的自动封禁）/ `unknown`（操作人账号已注销）。
  数据取自审计流水（以 UID 为目标），**账号注销之后记录仍然在**——这是它与用户详情页
  「处置记录」的区别（后者随账号级联删除）。

**上传策略的按用户覆盖**：`max_file_size`、`max_batch_size`、`max_pixels` 与 `formats`
四项走同一张策略表，`0`（数值）与空数组（格式）表示「跟随上一层」，
解析顺序是 **单用户覆盖 → 全局策略行 → `upload_policy`（系统设置里的站点默认）**。
`upload_policy` 就是「系统设置 → 上传策略」那一份；界面上的全局输入直接写它，
所以正常使用下全局策略行的这四列是空的。`users[].upload` 给的是解析后的生效值，
界面与客户端队列都按它展示与限流；`/me` 的 `limits` 同样带这份生效值。
生效范围：单文件上限在收 multipart 流时截断（`FILE_TOO_LARGE`），像素上限
（`INVALID_IMAGE`）与格式白名单（`UNSUPPORTED_FORMAT`）在上传校验里判；
单批张数与并发数是客户端队列的上限，服务端按策略值下发。

**站点资产（背景图、默认头像）**：两处都是 multipart 上传，字段名 `file`：

- `POST /api/v1/admin/settings/background` → `{ "data": { "url": "/api/v1/site/background?v=…", "bytes": 91234 } }`
- `POST /api/v1/admin/settings/avatar` → `{ "data": { "url": "/api/v1/site/avatar?v=…", "bytes": 3210 } }`

上传的字节存进库（`config:background` / `config:default_avatar` 两份资产），`/site` 下发的地址自带
版本号，管理员换一张就换一个 URL。地址也可以在 `PUT /admin/settings` 的 `appearance` 里直接填
外链；填 `null` 表示清掉（资产字节一起删）。

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

`scope=all` 的规则由全站中间件执行：浏览广场、直链、静态资源都会被拦；注册/登录/上传再按场景检查一次。`policy.trust_proxy` / `proxy_hops` 保存后覆盖环境变量 `PIXORA_TRUST_PROXY` 的默认值；直接暴露公网且没有反向代理时两者都应为 `0` 或关闭信任，否则客户端可以伪造 `X-Forwarded-For`。命中次数按规则每分钟最多记一次，拦下的请求不会每来一次就写一次库。

### 系统设置里的注册开关与自动关闭

`GET /api/v1/admin/settings` 的 `site` 里带这几个字段，界面据此把开关的状态说清楚：

```json
"register_enabled": false,
"register_auto_closed": true,
"register_closed_reason": "宿主机磁盘占用已达 89.4%（触发阈值 80%），注册入口已自动关闭；…",
"register_auto_close_override": false,
"register_auto_close": { "enabled": true, "on": 80, "off": 70, "percent": 89.4 }
```

`PUT` 里给了 `site.register_enabled` 就表示「管理员亲手动过开关」：打开时立覆盖标记
（自动关闭让位，容量恢复前不再抢开关），关闭时连闩锁一起清掉（容量恢复也不会替他重开）。

### 8.6 系统设置

- `GET /api/v1/admin/settings` →

```json
{ "data": {
  "site": { "name": "Pixora", "base_url": "https://img.example.com",
            "plaza_public": true, "require_login_to_upload": true, "register_enabled": true,
            "email_domains": "example.com",
            "plaza_grid_columns": 3, "plaza_auto_refresh_seconds": 5, "recycle_retention_days": 30 },
  "upload": { "max_file_bytes": 20971520, "max_batch_size": 1000, "max_pixels": 80000000,
              "queue_depth": 500, "formats": ["jpg","png"] },
  "thumb": { "client": true, "server_fallback": false },
  "mail": { "smtp_host": "smtp.qiye.aliyun.com", "smtp_port": 465,
            "smtp_security": "ssl", "smtp_user": "noreply@example.com",
            "password_set": true, "from_name": "Pixora 管理团队", "from_email": "noreply@example.com",
            "appeal_email": "admin@example.com",
            "disposable_check": true, "mx_check": true, "third_party_check": false,
            "third_party_set": false },
  "appearance": { "background": null, "default_avatar": null }
} }
```

- `PUT /api/v1/admin/settings` ← 上述任意子对象（只写传入的键）

  `appearance.background` 与 `appearance.default_avatar` 都接受：外链地址、本站上传那张的
  公开地址（`/api/v1/site/background`、`/api/v1/site/avatar`）、或 `null`（清掉，同时删掉
  库里那份字节）。
- `POST /api/v1/admin/settings/avatar` → 上传站点默认头像（multipart，字段名 `file`），
  响应 `{ "data": { "ok": true, "url": "…", "bytes": 12345 } }`。字节存进库（不落文件系统），
  上传完立刻生效：没设过头像的账号（含新注册的）都显示它。
- `POST /api/v1/admin/settings/mail/test` → `{ "to": "admin@example.com" }` → `{ "data": { "ok": true, "message": "…" } }`
- `POST /api/v1/admin/settings/formats` → `{ "formats": ["jpg","png","avif"] }`
（「重算缩略图」走 `POST /api/v1/admin/storage/thumbs/rebuild`；缺缩略图的图由后台任务
按轮次补，受 `thumb.server_fallback` 控制。）
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
GET /{upload_prefix}/{sha256}.{ext}?thumb=2    # 这张图的缩略图（没有则回落到原图）
GET /{upload_prefix}/{sha256}.{ext}?download=1 # 强制下载（Content-Disposition: attachment）
```

- 路径里那个 `sha256` 是**原图**的：带 `thumb` 参数时顺着图片行找到它的缩略图实体，
  所以列表页拿一条直链就能同时取到原图与缩略图；URL 里的 `ext` 也始终是原图的格式。
  服务端只认「有没有 `thumb` 这个参数」，值本身是版本号：缩略图的生成方式一变就抬一位
  （`?thumb=1` → `?thumb=2`），让浏览器里那份 `immutable` 缓存自然失效
- **还挂着公开引用**的图片可以匿名取，响应头带 `Cache-Control: public, max-age=31536000, immutable`（内容寻址，永不变化）
- **只被私有内容引用**的图片：直链只对「本人（含管理员）」放行，其余返回 404；拿到链接不等于有权限
  本人取到时带 `Cache-Control: private, max-age=300`，避免私人图片在共享缓存里停留
- 「公开引用」指的是**图片自己那一行**（`images.visibility = 'public'` 且未删除）：相册的可见性只管
  相册能不能被别人浏览，不参与直链的判定，也不影响相册内图片的可见性。两者要一起改，用相册管理页
  的「改为私有」——它会在改完之后问一句要不要把相册里的图片一起改成私有
- 回收站里的图片（软删）对本人与管理员照常放行：回收站页面上的预览用的就是这条直链，否则本人打开
  回收站只能看到一排碎图标。对外仍然是 404
- 响应头带 `ETag: "<sha256>"` 与 `X-Content-Type-Options: nosniff`
- `ext` 与库中记录不一致时返回 404（防止用伪造扩展名探测）
- SVG 额外带 `Content-Security-Policy: sandbox; default-src 'none'; script-src 'none'; object-src 'none'; img-src data:; style-src 'unsafe-inline'; frame-ancestors 'none'`；不可信来源（非本人且非公开）强制 `attachment`
- 应用层统一带 `X-Frame-Options: SAMEORIGIN`、`Referrer-Policy`、`Permissions-Policy` 与 `frame-ancestors 'self'`；直链如果是 SVG，自身的沙箱 CSP 会覆盖这一层
- 一级目录可在后台改（`/api/v1/admin/storage/prefix`），改完旧链依然有效
- 本地驱动可直接由 Caddy 走 `X-Accel-Redirect` 交给内核发送，此时响应头由本站在前一步设置
- **下载（`?download=1`）匿名也能用**：拿到的是一份真正的图片文件，文件名带真实扩展名
  （`545f214a58b00fd8.jpg`）。本人与管理员还能拿到上传时的原始文件名，并同时带上
  `filename*=UTF-8…` 那份编码——中文名不会变成问号。以前这里发的是 `{哈希}.img`，
  文件管理器不认、双击打不开
- 下载按「匿名的额度 / 登录用户的额度」分别限频（见 8.4），超限返回 429 并带 `Retry-After`；
  页面上直接显示的图片（不带 `download=1`）不计数——否则打开一次广场就把额度耗光了

## 10.5 登录方式（第三方与手机号）

邮箱 + 密码始终可用，邮箱是账号本身（必需、不可解绑）。其余方式都是可选的加法：

```
GET  /auth/oauth/{provider}/start?mode=login|bind&next=/…   # 302 去对方站点授权
GET  /auth/oauth/{provider}/callback?code=…&state=…         # 对方回来（302 回前端路由）
POST /auth/oauth/complete        # 对方没给邮箱时：{ ticket, email, code } 补一个再建号
POST /auth/phone/code            # { phone } 发短信验证码
POST /auth/phone/login           # { phone, code, remember? } 手机号登录
GET  /me/identities              # 我挂了哪些方式（含 email 与 password_set）
POST /me/identities              # 绑手机号：{ provider: "phone", phone, code }
POST /me/identities/phone/code   # 绑手机号前要验证码：{ phone }
DELETE /me/identities/{provider} # 解绑（email 拒绝；最后一根稻草也拒绝）
```

- `provider` ∈ `github` / `google` / `qq` / `wechat` / `phone`。支持哪些由管理员在
  「系统设置 → 登录方式」里配（`PUT /admin/login-methods`，密钥加密存储），`GET /site`
  用 `login_methods` 把它们下发给前台：没启用、或凭据没配全的按钮不出现。
- 扫码登录（微信、QQ）：登录页把 `/auth/oauth/{provider}/start?...&embed=1` 嵌进 iframe，
  对方的页面自己画二维码并轮询扫码结果；回调带 `embed` 时返回一小段 HTML，把**顶层窗口**
  带去站内路由（直接 302 只会把站内页面塞进那一小块框里）。对方页面拒绝被嵌时，
  界面上另有「在新窗口打开」的退路。
- `state` 是用主密钥签名的短令牌（15 分钟）：里面带 `mode`、`next`、随机数与发起绑定的人，
  外面验签就能读，不查库。`next` 只接受站内相对路径（`//evil.com`、`/\evil.com` 这类会被改写成 `/`）。
- **`state` 同时绑定发起流程的浏览器**：`/start` 会把 state 的 SHA-256 写进 HttpOnly Cookie
  （`pixora_oauth_state`，Path `/api/v1/auth/oauth`，10 分钟），callback 必须带上同一份。
  只发签名 state 的链接给受害者不再能完成登录，login CSRF 被挡住。
- `redirect_uri` 用站点设置里的基础 URL（后台/安装向导写的那个），不是只看环境变量；换域名后两边要一致。
- 登录：`(provider, subject)` 已绑定就直接发会话；没绑过但对方给了**已验证的邮箱**就建号并绑定；
  没邮箱（QQ、微信从来不给）则回一张签名令牌，前端跳到 `#/complete-signup` 让用户填邮箱、
  走一次注册验证码，再调 `POST /auth/oauth/complete`——邮箱是账号本身，不能凭一句话当成他的。
- 绑定：`mode=bind` 必须在已登录状态发起，回调里再核一次身份；一个第三方身份只能挂一个账号，
  重复挂在别人身上返回 `CONFLICT`。
- 解绑：邮箱不可解绑；解绑任何方式前都会检查「还剩别的方式能进得来」（有可用口令，或还有别的
  身份），否则 `BAD_REQUEST` 并说明原因。第三方建号的账号一开始没有口令，页面会引导先设密码。
- 手机号登录只对**已绑定**的号码有效（没绑过时提示先用邮箱注册再绑定）；`POST /me/identities`
  只处理手机号——第三方方式的绑定必须走授权页，那一步的意义就是「本人到对方站点确认一次」。
- 管理员侧：`GET/PUT /api/v1/admin/login-methods` 读写配置（响应里带回调地址，界面直接给出来
  让人复制）；`DELETE /api/v1/admin/users/{id}/identities/{provider}` 可以替用户摘掉某个方式
  （同样受「别把人锁在门外」的约束）。用户详情返回里带 `identities` 数组。
- 短信走阿里云短信（AccessKey、签名、模板编号，模板变量名要叫 `code`）；没配好时发码接口
  直接返回 `INTERNAL` 并说明缺哪一项，不假装发出去了。发码有 60 秒重发间隔。

## 11. 其它

- `GET /healthz` → `{ "status": "ok", "db": "ok", "uptime_seconds": 1234 }`
- 静态资源：`/`、`/login`、`/albums` 等前端路由全部回落到 `index.html`（SPA 路由）
- 前端的地址栏规则：首页的规范地址是站点根 `/`（不带 `#/`）；认不出来的地址——`/images131231`
  这类陌生路径、打错的 hash 段——一律当首页处理，地址栏收敛成 `/` 并显示广场，不给 404 页
- 登录页把几种方式做成切换标签（账密 / 手机号 / 扫码），不把两套输入框叠在同一屏里；
  扫码那一栏只有微信、QQ 启用时才出现
- 头像：设过的地址加载不出来时（外链失效、路径过期、对方不允许外链）提示一次并**回落到
  默认头像**（站点默认 → 内置那张），账号信息页还会在地址输入框下写清是哪个地址坏了；
  点顶栏头像进「我的相册」（日常用得最多的一页），账号信息等入口在悬停菜单里
- 预览浮层放大后**两个方向都能拖**，边界只保证「还有一块图像留在舞台里」；
  缩放以指针为锚点并显示百分比，滚轮缩到 1× 时偏移归零
- 所有写操作返回的 `data` 里都应带上 `csrf_token`（会话续期时前端就地更新）
