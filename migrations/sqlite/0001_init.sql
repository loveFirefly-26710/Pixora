-- Pixora 数据库结构（SQLite）
--
-- 与 postgres 方言同构，差异只在类型落地：自增主键用 INTEGER PRIMARY KEY AUTOINCREMENT
-- （带 AUTOINCREMENT 才不会被回收 rowid，删掉的图片 id 不会改头换面出现在别处），
-- 布尔用 INTEGER 0/1，长文本一律 TEXT。
--
-- 连接参数（在 DSN 里带上）：foreign_keys=true、journal_mode=WAL、busy_timeout=5000。
-- 外键必须显式打开，否则 ON DELETE CASCADE 全是摆设。

-- ============================================================
-- 用户
-- ============================================================
CREATE TABLE users (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    uid               TEXT    NOT NULL UNIQUE,
    email             TEXT    NOT NULL UNIQUE,
    email_verified    INTEGER NOT NULL DEFAULT 0,
    password_hash     TEXT    NOT NULL,
    role              TEXT    NOT NULL DEFAULT 'user',
    status            TEXT    NOT NULL DEFAULT 'pending',
    status_reason     TEXT,
    status_changed_at TEXT,
    status_changed_by INTEGER,
    totp_secret       TEXT,
    totp_enabled      INTEGER NOT NULL DEFAULT 0,
    quota_bytes       INTEGER NOT NULL DEFAULT 1073741824,
    used_bytes        INTEGER NOT NULL DEFAULT 0,
    login_fail_count  INTEGER NOT NULL DEFAULT 0,
    locked_until      TEXT,
    created_at        TEXT    NOT NULL,
    last_login_at     TEXT,
    last_login_ip     TEXT
);
CREATE INDEX idx_users_role_status ON users(role, status);
CREATE INDEX idx_users_status      ON users(status) WHERE status <> 'active';

-- ============================================================
-- 处置记录
-- ============================================================
CREATE TABLE user_actions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_id   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action     TEXT    NOT NULL,
    reason     TEXT,
    detail     TEXT,
    mail_sent  INTEGER NOT NULL DEFAULT 0,
    created_at TEXT    NOT NULL
);
CREATE INDEX idx_user_actions_user ON user_actions(user_id, created_at DESC);

-- ============================================================
-- 会话
-- ============================================================
CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   TEXT    NOT NULL UNIQUE,
    device       TEXT,
    ip           TEXT,
    user_agent   TEXT,
    created_at   TEXT    NOT NULL,
    last_seen_at TEXT    NOT NULL,
    revoked_at   TEXT
);
CREATE INDEX idx_sessions_user ON sessions(user_id, last_seen_at DESC) WHERE revoked_at IS NULL;

-- ============================================================
-- 邮箱验证
-- ============================================================
CREATE TABLE email_verifications (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    email       TEXT    NOT NULL,
    code_hash   TEXT    NOT NULL,
    purpose     TEXT    NOT NULL,
    expires_at  TEXT    NOT NULL,
    consumed_at TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0,
    request_ip  TEXT,
    created_at  TEXT    NOT NULL
);
CREATE INDEX idx_email_verif_lookup ON email_verifications(email, purpose, expires_at);

CREATE TABLE disposable_domains (
    domain     TEXT PRIMARY KEY,
    source     TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE email_templates (
    name       TEXT PRIMARY KEY,
    subject    TEXT NOT NULL,
    body       TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mail_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    to_email        TEXT    NOT NULL,
    template_name   TEXT    NOT NULL,
    vars            TEXT    NOT NULL DEFAULT '{}',
    status          TEXT    NOT NULL DEFAULT 'queued',
    attempts        INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    next_attempt_at TEXT,
    created_at      TEXT    NOT NULL,
    sent_at         TEXT
);
CREATE INDEX idx_mail_pickup ON mail_outbox(status, next_attempt_at);

-- ============================================================
-- 相册
-- ============================================================
CREATE TABLE albums (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          TEXT    NOT NULL,
    slug          TEXT    NOT NULL,
    description   TEXT,
    visibility    TEXT    NOT NULL DEFAULT 'private',
    cover_blob_id INTEGER,
    image_count   INTEGER NOT NULL DEFAULT 0,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    UNIQUE (owner_id, slug)
);
CREATE INDEX idx_albums_owner ON albums(owner_id, sort_order);

-- ============================================================
-- 物理文件实体（内容寻址）
-- ============================================================
CREATE TABLE blobs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    sha256         TEXT    NOT NULL,
    kind           TEXT    NOT NULL DEFAULT 'original',
    size           INTEGER NOT NULL,
    mime_type      TEXT    NOT NULL,
    ext            TEXT    NOT NULL,
    width          INTEGER,
    height         INTEGER,
    storage_driver TEXT    NOT NULL,
    storage_key    TEXT    NOT NULL,
    backend_id     INTEGER,
    ref_count      INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL,
    UNIQUE (sha256, kind)
);
CREATE INDEX idx_blobs_driver   ON blobs(storage_driver);
CREATE INDEX idx_blobs_kind     ON blobs(kind);
CREATE INDEX idx_blobs_gc       ON blobs(ref_count) WHERE ref_count = 0;

-- ============================================================
-- 图片与标签
-- ============================================================
CREATE TABLE images (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id      INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    blob_id       INTEGER NOT NULL REFERENCES blobs(id),
    thumb_blob_id INTEGER REFERENCES blobs(id),
    original_name TEXT    NOT NULL,
    title         TEXT,
    visibility    TEXT    NOT NULL DEFAULT 'private',
    views         INTEGER NOT NULL DEFAULT 0,
    upload_time   TEXT    NOT NULL,
    deleted_at    TEXT
);
CREATE INDEX idx_images_owner_time ON images(owner_id, upload_time DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_images_album      ON images(album_id)                   WHERE deleted_at IS NULL;
CREATE INDEX idx_images_blob       ON images(blob_id);
CREATE INDEX idx_images_thumb_blob ON images(thumb_blob_id);
CREATE INDEX idx_images_public     ON images(upload_time DESC)           WHERE visibility = 'public' AND deleted_at IS NULL;

CREATE TABLE tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id   INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    created_at TEXT    NOT NULL,
    UNIQUE (owner_id, name)
);
CREATE INDEX idx_tags_owner_name ON tags(owner_id, name);

CREATE TABLE image_tags (
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id)   ON DELETE CASCADE,
    PRIMARY KEY (image_id, tag_id)
);
CREATE INDEX idx_image_tags_tag ON image_tags(tag_id);

-- ============================================================
-- 上传任务账本
-- ============================================================
CREATE TABLE upload_jobs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    album_id      INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    sha256        TEXT,
    original_name TEXT    NOT NULL,
    size          INTEGER NOT NULL,
    status        TEXT    NOT NULL DEFAULT 'queued',
    dedup         INTEGER NOT NULL DEFAULT 0,
    image_id      INTEGER,
    attempts      INTEGER NOT NULL DEFAULT 0,
    error         TEXT,
    created_at    TEXT    NOT NULL,
    started_at    TEXT,
    finished_at   TEXT
);
CREATE INDEX idx_jobs_pickup ON upload_jobs(status, created_at) WHERE status = 'queued';
CREATE INDEX idx_jobs_owner  ON upload_jobs(owner_id, created_at DESC);

-- ============================================================
-- API 令牌
-- ============================================================
CREATE TABLE api_tokens (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL UNIQUE,
    display      TEXT    NOT NULL DEFAULT '',
    scopes       TEXT    NOT NULL DEFAULT '["upload"]',
    last_used_at TEXT,
    expires_at   TEXT,
    revoked_at   TEXT,
    created_at   TEXT    NOT NULL
);
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id) WHERE revoked_at IS NULL;

-- ============================================================
-- 频率限制策略
-- SQLite 的唯一索引把 NULL 视为互不相同，所以「至多一行全局默认」要靠表达式索引来锁。
-- ============================================================
CREATE TABLE rate_limit_policies (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id                INTEGER REFERENCES users(id) ON DELETE CASCADE,
    api_per_minute         INTEGER NOT NULL DEFAULT 120,
    api_per_hour           INTEGER NOT NULL DEFAULT 2000,
    api_read_per_minute    INTEGER NOT NULL DEFAULT 600,
    api_burst              INTEGER NOT NULL DEFAULT 20,
    upload_per_minute      INTEGER NOT NULL DEFAULT 30,
    upload_per_hour        INTEGER NOT NULL DEFAULT 500,
    upload_per_day         INTEGER NOT NULL DEFAULT 2000,
    upload_bytes_per_day   INTEGER NOT NULL DEFAULT 5368709120,
    max_concurrent_uploads INTEGER NOT NULL DEFAULT 4,
    max_batch_size         INTEGER NOT NULL DEFAULT 1000,
    max_file_size          INTEGER NOT NULL DEFAULT 20971520,
    unlimited              INTEGER NOT NULL DEFAULT 0,
    note                   TEXT,
    updated_at             TEXT    NOT NULL
);
CREATE UNIQUE INDEX idx_rate_user   ON rate_limit_policies(user_id) WHERE user_id IS NOT NULL;
CREATE UNIQUE INDEX idx_rate_global ON rate_limit_policies((user_id IS NULL)) WHERE user_id IS NULL;

CREATE TABLE rate_counters (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    scope        TEXT    NOT NULL,
    subject      TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    window_start TEXT    NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    bytes        INTEGER NOT NULL DEFAULT 0,
    updated_at   TEXT    NOT NULL,
    UNIQUE (scope, subject, kind, window_start)
);
CREATE INDEX idx_rate_counters_window ON rate_counters(window_start);

-- ============================================================
-- IP 规则
-- ============================================================
CREATE TABLE ip_rules (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    cidr       TEXT    NOT NULL,
    kind       TEXT    NOT NULL DEFAULT 'deny',
    scope      TEXT    NOT NULL DEFAULT 'all',
    note       TEXT,
    hits       INTEGER NOT NULL DEFAULT 0,
    is_auto    INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT,
    created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT    NOT NULL
);
CREATE INDEX idx_ip_rules_lookup ON ip_rules(kind, scope, expires_at);

-- ============================================================
-- 服务条款
-- ============================================================
CREATE TABLE terms_versions (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    version           TEXT    NOT NULL UNIQUE,
    title             TEXT    NOT NULL DEFAULT '服务条款',
    body              TEXT    NOT NULL,
    summary           TEXT,
    status            TEXT    NOT NULL DEFAULT 'draft',
    require_reconsent INTEGER NOT NULL DEFAULT 1,
    effective_at      TEXT,
    published_at      TEXT,
    created_by        INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at        TEXT    NOT NULL
);
CREATE UNIQUE INDEX idx_terms_active ON terms_versions(status) WHERE status = 'active';

CREATE TABLE terms_consents (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    version_id INTEGER NOT NULL REFERENCES terms_versions(id) ON DELETE RESTRICT,
    version    TEXT    NOT NULL,
    agreed_at  TEXT    NOT NULL,
    ip         TEXT,
    user_agent TEXT,
    UNIQUE (user_id, version_id)
);
CREATE INDEX idx_terms_consents_user ON terms_consents(user_id, agreed_at DESC);

-- ============================================================
-- 系统设置
-- ============================================================
CREATE TABLE settings (
    name       TEXT PRIMARY KEY,
    value      TEXT,
    encrypted  INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

-- ============================================================
-- 存储后端实例
-- ============================================================
CREATE TABLE storage_backends (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    driver         TEXT    NOT NULL,
    label          TEXT    NOT NULL,
    config         TEXT    NOT NULL DEFAULT '{}',
    is_default     INTEGER NOT NULL DEFAULT 0,
    enabled        INTEGER NOT NULL DEFAULT 1,
    capacity_mode  TEXT    NOT NULL DEFAULT 'auto',
    capacity_total INTEGER,
    created_at     TEXT    NOT NULL
);
CREATE UNIQUE INDEX idx_backend_default ON storage_backends(is_default) WHERE is_default;

-- ============================================================
-- 审计日志
-- ============================================================
CREATE TABLE audit_logs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id   INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action     TEXT    NOT NULL,
    target     TEXT,
    detail     TEXT,
    ip         TEXT,
    created_at TEXT    NOT NULL
);
CREATE INDEX idx_audit_actor_time  ON audit_logs(actor_id, created_at DESC);
CREATE INDEX idx_audit_action_time ON audit_logs(action, created_at DESC);

-- ============================================================
-- 数据库内存储驱动（driver = database）
-- ============================================================
CREATE TABLE storage_objects (
    key        TEXT PRIMARY KEY,
    content    BLOB NOT NULL,
    size       INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

-- ============================================================
-- 二次验证的恢复码（一次性，只存哈希）
-- ============================================================
CREATE TABLE recovery_codes (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash  TEXT    NOT NULL,
    used_at    TEXT,
    created_at TEXT    NOT NULL
);
CREATE INDEX idx_recovery_user ON recovery_codes(user_id) WHERE used_at IS NULL;
