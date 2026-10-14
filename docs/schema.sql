-- Pixora 数据库结构（PostgreSQL 方言，默认）
-- 其他方言（MySQL / SQLite）由迁移工具从同一份模型生成。
--
-- 三条贯穿全库的设计约定：
--   1. blobs 与 images 分离。物理文件全局唯一，images 只持有引用，删除互不影响。
--   2. 处置与限流都是独立表，不往 users 上堆字段，便于留档和回溯。
--   3. 服务条款版本化，同意记录只增不改，删条款不删记录。

-- ============================================================
-- 用户
-- ============================================================
CREATE TABLE users (
    id                BIGSERIAL PRIMARY KEY,
    uid               VARCHAR(16)  NOT NULL UNIQUE,        -- 对外短标识，作为物理目录一级分类
    email             VARCHAR(255) NOT NULL UNIQUE,
    email_verified    BOOLEAN      NOT NULL DEFAULT FALSE,
    password_hash     TEXT         NOT NULL,               -- Argon2id
    role              VARCHAR(16)  NOT NULL DEFAULT 'user', -- admin | user
    -- pending 邮箱未验证 / active 正常 / suspended 暂停 / banned 封禁
    status            VARCHAR(16)  NOT NULL DEFAULT 'pending',
    status_reason     TEXT,                                -- 展示给用户的原因
    status_changed_at TIMESTAMPTZ,
    status_changed_by BIGINT,                              -- 操作管理员，ON DELETE 留空
    totp_secret       TEXT,                                -- 可选二次验证，加密存储
    quota_bytes       BIGINT       NOT NULL DEFAULT 1073741824, -- 默认 1GiB，注册时套用系统默认值
    used_bytes        BIGINT       NOT NULL DEFAULT 0,
    login_fail_count  SMALLINT     NOT NULL DEFAULT 0,     -- 连续失败计数，用于锁定
    locked_until      TIMESTAMPTZ,                         -- 连续失败后的临时锁定
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_login_at     TIMESTAMPTZ,
    last_login_ip     INET
);
CREATE INDEX idx_users_role_status ON users(role, status);
CREATE INDEX idx_users_status      ON users(status) WHERE status <> 'active';

-- ============================================================
-- 处置记录（暂停 / 封禁 / 解封 / 改邮箱 / 改配额 …）
-- ============================================================
CREATE TABLE user_actions (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id    BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    action      VARCHAR(32) NOT NULL,   -- suspend|unsuspend|ban|unban|reset_password|change_email|quota_change|delete
    reason      TEXT,
    detail      JSONB,                  -- 变更前后的值，便于追责
    mail_sent   BOOLEAN     NOT NULL DEFAULT FALSE,  -- 是否已发出通知邮件
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_user_actions_user ON user_actions(user_id, created_at DESC);

-- ============================================================
-- 会话（活跃会话管理、暂停时批量踢下线）
-- ============================================================
CREATE TABLE sessions (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   CHAR(64)    NOT NULL UNIQUE,
    device       VARCHAR(128),
    ip           INET,
    user_agent   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX idx_sessions_user ON sessions(user_id, last_seen_at DESC) WHERE revoked_at IS NULL;

-- ============================================================
-- 邮箱验证（注册 / 找回密码 / 换绑邮箱）
-- ============================================================
CREATE TABLE email_verifications (
    id            BIGSERIAL PRIMARY KEY,
    email         VARCHAR(255) NOT NULL,
    code_hash     TEXT         NOT NULL,      -- 只存哈希，不存明文验证码
    purpose       VARCHAR(32)  NOT NULL,      -- register | reset | change_email
    expires_at    TIMESTAMPTZ  NOT NULL,
    consumed_at   TIMESTAMPTZ,
    attempts      SMALLINT     NOT NULL DEFAULT 0,
    request_ip    INET,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);
CREATE INDEX idx_email_verif_lookup ON email_verifications(email, purpose, expires_at);

-- 一次性邮箱域名清单（随镜像打包，可在线更新）
CREATE TABLE disposable_domains (
    domain     VARCHAR(255) PRIMARY KEY,
    source     VARCHAR(64),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 邮件模板（暂停通知、封禁通知、验证码、密码重置）
CREATE TABLE email_templates (
    key        VARCHAR(64) PRIMARY KEY,   -- suspend_notice | ban_notice | verify_code | reset_password
    subject    VARCHAR(255) NOT NULL,
    body       TEXT         NOT NULL,     -- 支持 {{email}} {{uid}} {{reason}} {{appeal_email}} 占位符
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- ============================================================
-- 相册
-- ============================================================
CREATE TABLE albums (
    id            BIGSERIAL PRIMARY KEY,
    owner_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          VARCHAR(128) NOT NULL,
    slug          VARCHAR(128) NOT NULL,        -- 物理目录名，同一用户内唯一
    description   TEXT,
    visibility    VARCHAR(16) NOT NULL DEFAULT 'private', -- public | private
    cover_blob_id BIGINT,
    image_count   INTEGER     NOT NULL DEFAULT 0,
    sort_order    INTEGER     NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, slug)
);
CREATE INDEX idx_albums_owner ON albums(owner_id, sort_order);

-- ============================================================
-- 物理文件实体（内容寻址，全局唯一）
-- ============================================================
CREATE TABLE blobs (
    id             BIGSERIAL PRIMARY KEY,
    sha256         CHAR(64)    NOT NULL UNIQUE,  -- 内容哈希，去重依据
    size           BIGINT      NOT NULL,
    mime_type      VARCHAR(100) NOT NULL,
    ext            VARCHAR(16)  NOT NULL,
    width          INTEGER,
    height         INTEGER,
    storage_driver VARCHAR(32) NOT NULL,         -- local | database | github | webdav | telegram | s3
    storage_key    TEXT        NOT NULL,         -- CAS 路径：blobs/{xx}/{sha256}.{ext}
    ref_count      INTEGER     NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_blobs_driver   ON blobs(storage_driver);
CREATE INDEX idx_blobs_refcount ON blobs(ref_count) WHERE ref_count = 0; -- GC 扫描

-- ============================================================
-- 图片（用户/相册维度的逻辑引用）
-- ============================================================
CREATE TABLE images (
    id             BIGSERIAL PRIMARY KEY,
    owner_id       BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id       BIGINT      REFERENCES albums(id) ON DELETE SET NULL,
    blob_id        BIGINT      NOT NULL REFERENCES blobs(id),
    original_name  VARCHAR(255) NOT NULL,        -- 仅内部保留，绝不对外暴露
    title          VARCHAR(255),
    tags           TEXT[]      NOT NULL DEFAULT '{}',
    visibility     VARCHAR(16) NOT NULL DEFAULT 'private', -- public | private
    upload_time    TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at     TIMESTAMPTZ                   -- 软删除
);
CREATE INDEX idx_images_owner_time ON images(owner_id, upload_time DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_images_album      ON images(album_id)                   WHERE deleted_at IS NULL;
CREATE INDEX idx_images_blob       ON images(blob_id);
CREATE INDEX idx_images_tags       ON images USING GIN(tags);
CREATE INDEX idx_images_public     ON images(upload_time DESC)           WHERE visibility = 'public' AND deleted_at IS NULL;

-- 广场全文检索（title + tags 合并成 tsvector）
ALTER TABLE images ADD COLUMN search_vec tsvector
    GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(title, '') || ' ' || array_to_string(tags, ' '))
    ) STORED;
CREATE INDEX idx_images_search ON images USING GIN(search_vec);

-- ============================================================
-- 上传队列（持久化，进程重启可恢复）
-- ============================================================
CREATE TABLE upload_jobs (
    id            BIGSERIAL PRIMARY KEY,
    owner_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id      BIGINT      REFERENCES albums(id) ON DELETE SET NULL,
    sha256        CHAR(64),                      -- 客户端预检提供，API 上传由服务端回填
    original_name VARCHAR(255) NOT NULL,
    size          BIGINT      NOT NULL,
    status        VARCHAR(16) NOT NULL DEFAULT 'queued', -- queued|running|done|skipped|failed
    attempts      SMALLINT    NOT NULL DEFAULT 0,
    error         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at    TIMESTAMPTZ,
    finished_at   TIMESTAMPTZ
);
CREATE INDEX idx_jobs_pickup ON upload_jobs(status, created_at) WHERE status = 'queued';

-- ============================================================
-- API 令牌（哈希存储）
-- ============================================================
CREATE TABLE api_tokens (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         VARCHAR(64) NOT NULL,
    token_hash   CHAR(64)    NOT NULL UNIQUE,
    scopes       TEXT[]      NOT NULL DEFAULT '{upload}',
    last_used_at TIMESTAMPTZ,
    expires_at   TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id) WHERE revoked_at IS NULL;

-- ============================================================
-- 频率限制（user_id 为 NULL 的那一行是全局默认）
-- ============================================================
CREATE TABLE rate_limit_policies (
    id                     BIGSERIAL PRIMARY KEY,
    user_id                BIGINT      REFERENCES users(id) ON DELETE CASCADE,
    -- API 调用
    api_per_minute         INTEGER     NOT NULL DEFAULT 120,
    api_per_hour           INTEGER     NOT NULL DEFAULT 2000,
    api_read_per_minute    INTEGER     NOT NULL DEFAULT 600,   -- 读接口单独放宽
    api_burst              INTEGER     NOT NULL DEFAULT 20,    -- 瞬时并发上限
    -- 图片上传
    upload_per_minute      INTEGER     NOT NULL DEFAULT 30,
    upload_per_hour        INTEGER     NOT NULL DEFAULT 500,
    upload_per_day         INTEGER     NOT NULL DEFAULT 2000,
    upload_bytes_per_day   BIGINT      NOT NULL DEFAULT 5368709120, -- 5GiB
    -- 并发与批次
    max_concurrent_uploads SMALLINT    NOT NULL DEFAULT 4,
    max_batch_size         INTEGER     NOT NULL DEFAULT 1000,
    max_file_size          BIGINT      NOT NULL DEFAULT 20971520,   -- 20MiB
    -- -1 表示不限，管理员默认豁免
    unlimited              BOOLEAN     NOT NULL DEFAULT FALSE,
    note                   TEXT,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- 每个用户至多一条覆盖，user_id 为 NULL 至多一条全局默认
CREATE UNIQUE INDEX idx_rate_user   ON rate_limit_policies(user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX idx_rate_global ON rate_limit_policies((user_id IS NULL)) WHERE user_id IS NULL;

-- ============================================================
-- IP 黑名单 / 白名单
-- ============================================================
CREATE TABLE ip_rules (
    id          BIGSERIAL PRIMARY KEY,
    cidr        CIDR        NOT NULL,          -- 单个 IP 写 /32，IPv6 写 /128
    kind        VARCHAR(8)  NOT NULL DEFAULT 'deny', -- deny | allow
    scope       VARCHAR(16) NOT NULL DEFAULT 'all',  -- all | register | login | upload
    note        TEXT,
    hits        BIGINT      NOT NULL DEFAULT 0,      -- 命中次数，便于判断要不要续期
    is_auto     BOOLEAN     NOT NULL DEFAULT FALSE,  -- 风控自动加的临时规则
    expires_at  TIMESTAMPTZ,                          -- 空表示永久
    created_by  BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_ip_rules_lookup ON ip_rules(kind, scope, expires_at);

-- ============================================================
-- 服务条款（版本化）
-- ============================================================
CREATE TABLE terms_versions (
    id                 BIGSERIAL PRIMARY KEY,
    version            VARCHAR(32)  NOT NULL UNIQUE,   -- v2.1
    title              VARCHAR(128) NOT NULL DEFAULT '服务条款',
    body               TEXT         NOT NULL,          -- 正文存库，改完即时生效
    summary            TEXT,                           -- 更新摘要，展示给用户
    status             VARCHAR(16)  NOT NULL DEFAULT 'draft', -- draft | active | archived
    require_reconsent  BOOLEAN      NOT NULL DEFAULT TRUE,    -- 发布后是否要求用户重新同意
    effective_at       TIMESTAMPTZ,
    published_at       TIMESTAMPTZ,
    created_by         BIGINT       REFERENCES users(id) ON DELETE SET NULL,
    created_at         TIMESTAMPTZ  NOT NULL DEFAULT now()
);
-- 同一时间只能有一个生效版本
CREATE UNIQUE INDEX idx_terms_active ON terms_versions(status) WHERE status = 'active';

CREATE TABLE terms_consents (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    version_id BIGINT      NOT NULL REFERENCES terms_versions(id) ON DELETE RESTRICT,
    version    VARCHAR(32) NOT NULL,          -- 冗余一份，条款被删也查得到当初同意的是哪版
    agreed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    ip         INET,
    user_agent TEXT,
    UNIQUE (user_id, version_id)
);
CREATE INDEX idx_terms_consents_user ON terms_consents(user_id, agreed_at DESC);

-- ============================================================
-- 系统设置（敏感值加密）
-- ============================================================
CREATE TABLE settings (
    key        VARCHAR(64) PRIMARY KEY,
    value      TEXT,
    encrypted  BOOLEAN     NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- 容量与配额相关的键：
--   capacity.mode            auto | manual | hidden
--   capacity.manual_total    管理员手填的系统总额（字节），OSS 挂载场景必填
--   quota.default_bytes      新用户默认配额
--   quota.per_user_max       单用户配额上限
--   register.auto_close_on   占用率超过该值时自动关闭注册
--   register.auto_close_off  降到该值以下自动恢复
--   ratelimit.on_breach      429 | log | temp_ban 的组合
--   ratelimit.ban_threshold  触发次数阈值（如 10 次 / 10 分钟）
--   ratelimit.ban_minutes    临时封禁时长
--   storage.env_path_note    数据目录来自环境变量，界面只读展示

-- ============================================================
-- 存储后端实例配置（可挂多个同类型后端）
-- ============================================================
CREATE TABLE storage_backends (
    id              BIGSERIAL PRIMARY KEY,
    driver          VARCHAR(32)  NOT NULL,       -- local | database | github | webdav | telegram | s3
    label           VARCHAR(64)  NOT NULL,
    config          JSONB        NOT NULL DEFAULT '{}', -- 敏感字段加密后存放
    is_default      BOOLEAN      NOT NULL DEFAULT FALSE,
    enabled         BOOLEAN      NOT NULL DEFAULT TRUE,
    -- 容量展示：auto 探测 / manual 手填 / hidden 不显示
    capacity_mode   VARCHAR(16)  NOT NULL DEFAULT 'auto',
    capacity_total  BIGINT,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX idx_backend_default ON storage_backends(is_default) WHERE is_default;

-- ============================================================
-- 审计日志
-- ============================================================
CREATE TABLE audit_logs (
    id         BIGSERIAL PRIMARY KEY,
    actor_id   BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    action     VARCHAR(64) NOT NULL,   -- login_fail|login_blocked|rate_limited|admin_view_user|delete_image ...
    target     VARCHAR(128),
    detail     JSONB,
    ip         INET,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_audit_actor_time ON audit_logs(actor_id, created_at DESC);
CREATE INDEX idx_audit_action_time ON audit_logs(action, created_at DESC);

-- ============================================================
-- 队列取任务示例（PostgreSQL：无锁并发）
-- ============================================================
-- BEGIN;
--   SELECT id FROM upload_jobs
--    WHERE status = 'queued'
--    ORDER BY created_at
--    FOR UPDATE SKIP LOCKED
--    LIMIT 1;
--   UPDATE upload_jobs SET status='running', started_at=now(), attempts=attempts+1
--    WHERE id = <picked>;
-- COMMIT;

-- ============================================================
-- 去重引用与删除（跨用户隔离的关键路径）
-- ============================================================
-- 建立引用：INSERT INTO images(...) 后
--   UPDATE blobs SET ref_count = ref_count + 1 WHERE id = <blob>;
--   UPDATE users SET used_bytes = used_bytes + <size> WHERE id = <owner>;
--
-- 删除引用（软删）：
--   UPDATE images SET deleted_at = now() WHERE id = <image>;
--   UPDATE blobs  SET ref_count = ref_count - 1 WHERE id = <blob>;
--   UPDATE users  SET used_bytes = used_bytes - <size> WHERE id = <owner>;
--   仅当 blobs.ref_count 归零时，后台 GC 才删除物理文件。

-- ============================================================
-- 配额总额校验（保存前在事务里跑一次）
-- ============================================================
-- SELECT
--   (SELECT COALESCE(SUM(quota_bytes), 0) FROM users)                       AS allocated,
--   (SELECT (value)::BIGINT FROM settings WHERE key = 'capacity.manual_total') AS total;
-- allocated > total 时拒绝保存，并回报还差多少字节。

-- ============================================================
-- 暂停 / 封禁的生效路径
-- ============================================================
-- 1) UPDATE users SET status='suspended', status_reason=?, status_changed_at=now(),
--                     status_changed_by=? WHERE id=?;
-- 2) UPDATE sessions SET revoked_at=now() WHERE user_id=? AND revoked_at IS NULL;   -- 踢下线
--    （API Token 不删，鉴权时按 users.status 拦截，解封即恢复）
-- 3) INSERT INTO user_actions(...) 记录处置，并回填 mail_sent
-- 4) 由管理员邮箱发出通知邮件，正文取 email_templates.suspend_notice
