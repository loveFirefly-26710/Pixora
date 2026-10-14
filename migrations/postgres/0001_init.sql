-- Pixora 数据库结构（PostgreSQL）
--
-- 这份 DDL 以 docs/schema.sql 为蓝本。为了让仓储层能用同一份 SQL 同时跑
-- PostgreSQL、MySQL 与 SQLite，有三处类型落地方式做了统一（详见 migrations/README.md）：
--
--   1. 时间列一律 VARCHAR(32)，存 RFC3339 的 UTC 串（2026-10-14T15:30:00.123456Z）。
--      这种格式的字符串排序与时间排序一致，按天/小时分桶用 substr() 即可，
--      不必为三种方言各写一套日期函数。
--   2. JSON 载荷（审计 detail、后端 config、邮件变量）存 TEXT，在仓储层用 serde 解析。
--   3. IP 与 CIDR 存 VARCHAR(64)。网段匹配在进程内用 ipnet 做，规则条数本来就只有几十条，
--      放进内存比让三种数据库各写一条 CIDR 包含逻辑更省事，也更快。
--
-- 其余结构（表、列、约束、索引、引用计数、条款版本化）与设计稿一致。

-- ============================================================
-- 用户
-- ============================================================
CREATE TABLE users (
    id                BIGSERIAL PRIMARY KEY,
    uid               VARCHAR(16)  NOT NULL UNIQUE,
    email             VARCHAR(255) NOT NULL UNIQUE,
    email_verified    BIGINT      NOT NULL DEFAULT 0,
    password_hash     TEXT         NOT NULL,
    role              VARCHAR(16)  NOT NULL DEFAULT 'user',
    status            VARCHAR(16)  NOT NULL DEFAULT 'pending',
    status_reason     TEXT,
    status_changed_at VARCHAR(32),
    status_changed_by BIGINT,
    totp_secret       TEXT,
    totp_enabled      BIGINT      NOT NULL DEFAULT 0,
    quota_bytes       BIGINT       NOT NULL DEFAULT 1073741824,
    used_bytes        BIGINT       NOT NULL DEFAULT 0,
    login_fail_count  BIGINT     NOT NULL DEFAULT 0,
    locked_until      VARCHAR(32),
    created_at        VARCHAR(32)  NOT NULL,
    last_login_at     VARCHAR(32),
    last_login_ip     VARCHAR(64)
);
CREATE INDEX idx_users_role_status ON users(role, status);
CREATE INDEX idx_users_status      ON users(status) WHERE status <> 'active';

-- ============================================================
-- 处置记录（暂停 / 封禁 / 解封 / 改邮箱 / 改配额 …）
-- ============================================================
CREATE TABLE user_actions (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id   BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    action     VARCHAR(32) NOT NULL,
    reason     TEXT,
    detail     TEXT,
    mail_sent  BIGINT     NOT NULL DEFAULT 0,
    created_at VARCHAR(32) NOT NULL
);
CREATE INDEX idx_user_actions_user ON user_actions(user_id, created_at DESC);

-- ============================================================
-- 会话（活跃会话管理、暂停时批量踢下线）
-- ============================================================
CREATE TABLE sessions (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   VARCHAR(64) NOT NULL UNIQUE,
    device       VARCHAR(128),
    ip           VARCHAR(64),
    user_agent   TEXT,
    created_at   VARCHAR(32) NOT NULL,
    last_seen_at VARCHAR(32) NOT NULL,
    revoked_at   VARCHAR(32)
);
CREATE INDEX idx_sessions_user ON sessions(user_id, last_seen_at DESC) WHERE revoked_at IS NULL;

-- ============================================================
-- 邮箱验证（注册 / 找回密码 / 换绑邮箱）
-- ============================================================
CREATE TABLE email_verifications (
    id          BIGSERIAL PRIMARY KEY,
    email       VARCHAR(255) NOT NULL,
    code_hash   TEXT         NOT NULL,
    purpose     VARCHAR(32)  NOT NULL,
    expires_at  VARCHAR(32)  NOT NULL,
    consumed_at VARCHAR(32),
    attempts    BIGINT     NOT NULL DEFAULT 0,
    request_ip  VARCHAR(64),
    created_at  VARCHAR(32)  NOT NULL
);
CREATE INDEX idx_email_verif_lookup ON email_verifications(email, purpose, expires_at);

-- 一次性邮箱域名清单（随镜像打包，可在线更新）
CREATE TABLE disposable_domains (
    domain     VARCHAR(255) PRIMARY KEY,
    source     VARCHAR(64),
    updated_at VARCHAR(32) NOT NULL
);

-- 邮件模板（暂停通知、封禁通知、验证码、密码重置）
-- 占位符：{{email}} {{uid}} {{reason}} {{appeal_email}} {{appeal_requirements}} {{site}} {{code}}
CREATE TABLE email_templates (
    name       VARCHAR(64)  PRIMARY KEY,
    subject    VARCHAR(255) NOT NULL,
    body       TEXT         NOT NULL,
    updated_at VARCHAR(32)  NOT NULL
);

-- 发件箱：先落库再投递，投递失败进重试队列，进程重启不丢
CREATE TABLE mail_outbox (
    id              BIGSERIAL PRIMARY KEY,
    to_email        VARCHAR(255) NOT NULL,
    template_name   VARCHAR(64)  NOT NULL,
    vars            TEXT         NOT NULL DEFAULT '{}',
    status          VARCHAR(16)  NOT NULL DEFAULT 'queued',  -- queued | sent | failed
    attempts        BIGINT     NOT NULL DEFAULT 0,
    error           TEXT,
    next_attempt_at VARCHAR(32),
    created_at      VARCHAR(32)  NOT NULL,
    sent_at         VARCHAR(32)
);
CREATE INDEX idx_mail_pickup ON mail_outbox(status, next_attempt_at);

-- ============================================================
-- 相册
-- ============================================================
CREATE TABLE albums (
    id            BIGSERIAL PRIMARY KEY,
    owner_id      BIGINT       NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          VARCHAR(128) NOT NULL,
    slug          VARCHAR(128) NOT NULL,
    description   TEXT,
    visibility    VARCHAR(16)  NOT NULL DEFAULT 'private',
    cover_blob_id BIGINT,
    image_count   BIGINT      NOT NULL DEFAULT 0,
    sort_order    BIGINT      NOT NULL DEFAULT 0,
    created_at    VARCHAR(32)  NOT NULL,
    updated_at    VARCHAR(32)  NOT NULL,
    UNIQUE (owner_id, slug)
);
CREATE INDEX idx_albums_owner ON albums(owner_id, sort_order);

-- ============================================================
-- 物理文件实体（内容寻址，全局唯一）
--
-- kind 区分原图与缩略图：缩略图也是内容寻址的一份实体，
-- 两个用户给同一张图生成同一个缩略图时同样只存一份。
-- 唯一键带上 kind，否则「缩略图恰好等于原图」的小图会撞唯一约束。
-- ============================================================
CREATE TABLE blobs (
    id             BIGSERIAL PRIMARY KEY,
    sha256         VARCHAR(64)  NOT NULL,
    kind           VARCHAR(16)  NOT NULL DEFAULT 'original',  -- original | thumb
    size           BIGINT       NOT NULL,
    mime_type      VARCHAR(100) NOT NULL,
    ext            VARCHAR(16)  NOT NULL,
    width          BIGINT,
    height         BIGINT,
    storage_driver VARCHAR(32)  NOT NULL,
    storage_key    TEXT         NOT NULL,
    backend_id     BIGINT,
    ref_count      BIGINT      NOT NULL DEFAULT 0,
    created_at     VARCHAR(32)  NOT NULL,
    UNIQUE (sha256, kind)
);
CREATE INDEX idx_blobs_driver   ON blobs(storage_driver);
CREATE INDEX idx_blobs_kind     ON blobs(kind);
CREATE INDEX idx_blobs_gc       ON blobs(ref_count) WHERE ref_count = 0;

-- ============================================================
-- 图片（用户/相册维度的逻辑引用）
--
-- 标签走 tags / image_tags 两张关联表（设计稿里只有 MySQL 用关联表，
-- 这里三种方言统一，好让「按标签筛选」「标签候选」「全局改名」只有一份实现）。
-- ============================================================
CREATE TABLE images (
    id            BIGSERIAL PRIMARY KEY,
    owner_id      BIGINT       NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id      BIGINT       REFERENCES albums(id) ON DELETE SET NULL,
    blob_id       BIGINT       NOT NULL REFERENCES blobs(id),
    thumb_blob_id BIGINT       REFERENCES blobs(id),
    original_name VARCHAR(255) NOT NULL,
    title         VARCHAR(255),
    visibility    VARCHAR(16)  NOT NULL DEFAULT 'private',
    views         BIGINT       NOT NULL DEFAULT 0,
    upload_time   VARCHAR(32)  NOT NULL,
    deleted_at    VARCHAR(32)
);
CREATE INDEX idx_images_owner_time ON images(owner_id, upload_time DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_images_album      ON images(album_id)                   WHERE deleted_at IS NULL;
CREATE INDEX idx_images_blob       ON images(blob_id);
CREATE INDEX idx_images_thumb_blob ON images(thumb_blob_id);
CREATE INDEX idx_images_public     ON images(upload_time DESC)           WHERE visibility = 'public' AND deleted_at IS NULL;

CREATE TABLE tags (
    id         BIGSERIAL PRIMARY KEY,
    owner_id   BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       VARCHAR(64) NOT NULL,
    created_at VARCHAR(32) NOT NULL,
    UNIQUE (owner_id, name)
);
CREATE INDEX idx_tags_owner_name ON tags(owner_id, name);

CREATE TABLE image_tags (
    image_id BIGINT NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id   BIGINT NOT NULL REFERENCES tags(id)   ON DELETE CASCADE,
    PRIMARY KEY (image_id, tag_id)
);
CREATE INDEX idx_image_tags_tag ON image_tags(tag_id);

-- ============================================================
-- 上传任务账本（可恢复、可轮询、兼作背压依据）
-- ============================================================
CREATE TABLE upload_jobs (
    id            BIGSERIAL PRIMARY KEY,
    owner_id      BIGINT       NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id      BIGINT       REFERENCES albums(id) ON DELETE SET NULL,
    sha256        VARCHAR(64),
    original_name VARCHAR(255) NOT NULL,
    size          BIGINT       NOT NULL,
    status        VARCHAR(16)  NOT NULL DEFAULT 'queued',  -- queued|running|done|skipped|failed
    dedup         BIGINT      NOT NULL DEFAULT 0,
    image_id      BIGINT,
    attempts      BIGINT     NOT NULL DEFAULT 0,
    error         TEXT,
    created_at    VARCHAR(32)  NOT NULL,
    started_at    VARCHAR(32),
    finished_at   VARCHAR(32)
);
CREATE INDEX idx_jobs_pickup ON upload_jobs(status, created_at) WHERE status = 'queued';
CREATE INDEX idx_jobs_owner  ON upload_jobs(owner_id, created_at DESC);

-- ============================================================
-- API 令牌（哈希存储）
-- ============================================================
CREATE TABLE api_tokens (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         VARCHAR(64) NOT NULL,
    token_hash   VARCHAR(64) NOT NULL UNIQUE,
    display      VARCHAR(32) NOT NULL DEFAULT '',
    scopes       TEXT        NOT NULL DEFAULT '["upload"]',
    last_used_at VARCHAR(32),
    expires_at   VARCHAR(32),
    revoked_at   VARCHAR(32),
    created_at   VARCHAR(32) NOT NULL
);
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id) WHERE revoked_at IS NULL;

-- ============================================================
-- 频率限制策略（user_id 为 NULL 的那一行是全局默认）
-- ============================================================
CREATE TABLE rate_limit_policies (
    id                     BIGSERIAL PRIMARY KEY,
    user_id                BIGINT      REFERENCES users(id) ON DELETE CASCADE,
    api_per_minute         BIGINT     NOT NULL DEFAULT 120,
    api_per_hour           BIGINT     NOT NULL DEFAULT 2000,
    api_read_per_minute    BIGINT     NOT NULL DEFAULT 600,
    api_burst              BIGINT     NOT NULL DEFAULT 20,
    upload_per_minute      BIGINT     NOT NULL DEFAULT 30,
    upload_per_hour        BIGINT     NOT NULL DEFAULT 500,
    upload_per_day         BIGINT     NOT NULL DEFAULT 2000,
    upload_bytes_per_day   BIGINT      NOT NULL DEFAULT 5368709120,
    max_concurrent_uploads BIGINT    NOT NULL DEFAULT 4,
    max_batch_size         BIGINT     NOT NULL DEFAULT 1000,
    max_file_size          BIGINT      NOT NULL DEFAULT 20971520,
    unlimited              BIGINT     NOT NULL DEFAULT 0,
    note                   TEXT,
    updated_at             VARCHAR(32) NOT NULL
);
CREATE UNIQUE INDEX idx_rate_user   ON rate_limit_policies(user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX idx_rate_global ON rate_limit_policies((user_id IS NULL)) WHERE user_id IS NULL;

-- 滑动窗口计数：单实例走进程内令牌桶，跨实例或多窗口口径（天）落这张表
CREATE TABLE rate_counters (
    id           BIGSERIAL PRIMARY KEY,
    scope        VARCHAR(16)  NOT NULL,   -- user | ip | token
    subject      VARCHAR(128) NOT NULL,
    kind         VARCHAR(32)  NOT NULL,   -- api_minute|api_hour|api_read_minute|upload_minute|upload_hour|upload_day|upload_bytes_day
    window_start VARCHAR(32)  NOT NULL,
    count        BIGINT       NOT NULL DEFAULT 0,
    bytes        BIGINT       NOT NULL DEFAULT 0,
    updated_at   VARCHAR(32)  NOT NULL,
    UNIQUE (scope, subject, kind, window_start)
);
CREATE INDEX idx_rate_counters_window ON rate_counters(window_start);

-- ============================================================
-- IP 黑名单 / 白名单
-- ============================================================
CREATE TABLE ip_rules (
    id         BIGSERIAL PRIMARY KEY,
    cidr       VARCHAR(64) NOT NULL,
    kind       VARCHAR(8)  NOT NULL DEFAULT 'deny',    -- deny | allow
    scope      VARCHAR(16) NOT NULL DEFAULT 'all',     -- all | register | login | upload
    note       TEXT,
    hits       BIGINT      NOT NULL DEFAULT 0,
    is_auto    BIGINT     NOT NULL DEFAULT 0,
    expires_at VARCHAR(32),
    created_by BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    created_at VARCHAR(32) NOT NULL
);
CREATE INDEX idx_ip_rules_lookup ON ip_rules(kind, scope, expires_at);

-- ============================================================
-- 服务条款（版本化）
-- ============================================================
CREATE TABLE terms_versions (
    id                BIGSERIAL PRIMARY KEY,
    version           VARCHAR(32)  NOT NULL UNIQUE,
    title             VARCHAR(128) NOT NULL DEFAULT '服务条款',
    body              TEXT         NOT NULL,
    summary           TEXT,
    status            VARCHAR(16)  NOT NULL DEFAULT 'draft',   -- draft | active | archived
    require_reconsent BIGINT      NOT NULL DEFAULT 1,
    effective_at      VARCHAR(32),
    published_at      VARCHAR(32),
    created_by        BIGINT       REFERENCES users(id) ON DELETE SET NULL,
    created_at        VARCHAR(32)  NOT NULL
);
-- 同一时间只能有一个生效版本
CREATE UNIQUE INDEX idx_terms_active ON terms_versions(status) WHERE status = 'active';

CREATE TABLE terms_consents (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    version_id BIGINT      NOT NULL REFERENCES terms_versions(id) ON DELETE RESTRICT,
    version    VARCHAR(32) NOT NULL,
    agreed_at  VARCHAR(32) NOT NULL,
    ip         VARCHAR(64),
    user_agent TEXT,
    UNIQUE (user_id, version_id)
);
CREATE INDEX idx_terms_consents_user ON terms_consents(user_id, agreed_at DESC);

-- ============================================================
-- 系统设置（敏感值加密后存 value）
-- ============================================================
CREATE TABLE settings (
    name       VARCHAR(64) PRIMARY KEY,
    value      TEXT,
    encrypted  BIGINT     NOT NULL DEFAULT 0,
    updated_at VARCHAR(32) NOT NULL
);
-- 约定的键：
--   install.completed        安装向导是否完成（true 后安装入口锁定）
--   site.name / site.base_url / site.upload_prefix
--   register.enabled         是否允许自助注册
--   register.auto_close_on   占用率超过该值（0-100）自动关闭注册
--   register.auto_close_off  降到该值以下自动恢复注册
--   plaza.public             广场是否对未登录开放
--   upload.max_file_bytes / upload.max_batch / upload.max_pixels / upload.queue_depth
--   upload.formats           允许的扩展名清单（逗号分隔）
--   thumb.client / thumb.server_fallback
--   capacity.mode            auto | manual | hidden
--   capacity.manual_total    管理员手填的系统总额（字节）
--   quota.default_bytes / quota.per_user_max
--   ratelimit.on_breach      notice | audit | temp_ban
--   ratelimit.ban_threshold / ratelimit.ban_minutes
--   ip.enabled / ip.trust_proxy / ip.proxy_hops / ip.auto_ban / ip.fail_threshold / ip.ban_minutes
--   mail.smtp_host / mail.smtp_port / mail.smtp_security / mail.smtp_user / mail.smtp_password(加密)
--   mail.from_name / mail.from_email / mail.appeal_email
--   antispam.disposable / antispam.mx_check / antispam.third_party_key(加密)
--   storage.env_path_note    数据目录来源说明，界面只读展示
--   appearance.background    自定义背景图

-- ============================================================
-- 存储后端实例配置
-- ============================================================
CREATE TABLE storage_backends (
    id             BIGSERIAL PRIMARY KEY,
    driver         VARCHAR(32) NOT NULL,
    label          VARCHAR(64) NOT NULL,
    config         TEXT        NOT NULL DEFAULT '{}',
    is_default     BIGINT     NOT NULL DEFAULT 0,
    enabled        BIGINT     NOT NULL DEFAULT 1,
    capacity_mode  VARCHAR(16) NOT NULL DEFAULT 'auto',
    capacity_total BIGINT,
    created_at     VARCHAR(32) NOT NULL
);
CREATE UNIQUE INDEX idx_backend_default ON storage_backends(is_default) WHERE is_default;

-- ============================================================
-- 审计日志
-- ============================================================
CREATE TABLE audit_logs (
    id         BIGSERIAL PRIMARY KEY,
    actor_id   BIGINT       REFERENCES users(id) ON DELETE SET NULL,
    action     VARCHAR(64)  NOT NULL,
    target     VARCHAR(128),
    detail     TEXT,
    ip         VARCHAR(64),
    created_at VARCHAR(32)  NOT NULL
);
CREATE INDEX idx_audit_actor_time  ON audit_logs(actor_id, created_at DESC);
CREATE INDEX idx_audit_action_time ON audit_logs(action, created_at DESC);

-- ============================================================
-- 数据库内存储驱动（driver = database）
-- 字节直接存库，适合小图或不想碰文件系统的场景
-- ============================================================
CREATE TABLE storage_objects (
    key        VARCHAR(255) PRIMARY KEY,
    content    BYTEA        NOT NULL,
    size       BIGINT       NOT NULL,
    updated_at VARCHAR(32)  NOT NULL
);

-- ============================================================
-- 二次验证的恢复码（一次性，只存哈希）
-- ============================================================
CREATE TABLE recovery_codes (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash  VARCHAR(64) NOT NULL,
    used_at    VARCHAR(32),
    created_at VARCHAR(32) NOT NULL
);
CREATE INDEX idx_recovery_user ON recovery_codes(user_id) WHERE used_at IS NULL;
