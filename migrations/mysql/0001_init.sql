-- Pixora 数据库结构（MySQL / MariaDB 10.4+）
--
-- 与 postgres 方言同构，差异：
--   1. 自增主键 BIGINT AUTO_INCREMENT；布尔用 BIGINT。
--   2. MySQL 的 TEXT 列不允许有 DEFAULT，所以长文本列的默认值由应用侧填充。
--   3. MySQL 没有部分索引，三处「全局唯一」改用可空生成列 + 唯一索引实现：
--      生成列在条件不满足时取 NULL，而唯一索引把 NULL 视为互不相同，于是恰好只允许一行。
--   4. 表名与列名避开保留字，`key` 改名 `name`；`rank` 之类的名字不用。
--
-- 表字符集要求 utf8mb4（排序规则用 utf8mb4_0900_ai_ci 或 utf8mb4_general_ci）。

-- ============================================================
-- 用户
-- ============================================================
CREATE TABLE users (
    id                BIGINT AUTO_INCREMENT PRIMARY KEY,
    uid               VARCHAR(16)  NOT NULL UNIQUE,
    email             VARCHAR(255) NOT NULL UNIQUE,
    email_verified    BIGINT   NOT NULL DEFAULT 0,
    password_hash     VARCHAR(255) NOT NULL,
    role              VARCHAR(16)  NOT NULL DEFAULT 'user',
    status            VARCHAR(16)  NOT NULL DEFAULT 'pending',
    status_reason     TEXT,
    status_changed_at VARCHAR(32),
    status_changed_by BIGINT,
    totp_secret       TEXT,
    totp_enabled      BIGINT   NOT NULL DEFAULT 0,
    quota_bytes       BIGINT       NOT NULL DEFAULT 1073741824,
    used_bytes        BIGINT       NOT NULL DEFAULT 0,
    login_fail_count  BIGINT     NOT NULL DEFAULT 0,
    locked_until      VARCHAR(32),
    created_at        VARCHAR(32)  NOT NULL,
    last_login_at     VARCHAR(32),
    last_login_ip     VARCHAR(64)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_users_role_status ON users(role, status);
CREATE INDEX idx_users_status      ON users(status);

-- ============================================================
-- 处置记录
-- ============================================================
CREATE TABLE user_actions (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id    BIGINT      NOT NULL,
    actor_id   BIGINT,
    action     VARCHAR(32) NOT NULL,
    reason     TEXT,
    detail     TEXT,
    mail_sent  BIGINT  NOT NULL DEFAULT 0,
    created_at VARCHAR(32) NOT NULL,
    CONSTRAINT fk_user_actions_user  FOREIGN KEY (user_id)  REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_user_actions_actor FOREIGN KEY (actor_id) REFERENCES users(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_user_actions_user ON user_actions(user_id, created_at);

-- ============================================================
-- 会话
-- ============================================================
CREATE TABLE sessions (
    id           BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id      BIGINT      NOT NULL,
    token_hash   VARCHAR(64) NOT NULL UNIQUE,
    device       VARCHAR(128),
    ip           VARCHAR(64),
    user_agent   TEXT,
    created_at   VARCHAR(32) NOT NULL,
    last_seen_at VARCHAR(32) NOT NULL,
    revoked_at   VARCHAR(32),
    CONSTRAINT fk_sessions_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_sessions_user ON sessions(user_id, last_seen_at);

-- ============================================================
-- 邮箱验证
-- ============================================================
CREATE TABLE email_verifications (
    id          BIGINT AUTO_INCREMENT PRIMARY KEY,
    email       VARCHAR(255) NOT NULL,
    code_hash   VARCHAR(255) NOT NULL,
    purpose     VARCHAR(32)  NOT NULL,
    expires_at  VARCHAR(32)  NOT NULL,
    consumed_at VARCHAR(32),
    attempts    BIGINT     NOT NULL DEFAULT 0,
    request_ip  VARCHAR(64),
    created_at  VARCHAR(32)  NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_email_verif_lookup ON email_verifications(email, purpose, expires_at);

CREATE TABLE disposable_domains (
    domain     VARCHAR(255) PRIMARY KEY,
    source     VARCHAR(64),
    updated_at VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE email_templates (
    name       VARCHAR(64)  PRIMARY KEY,
    subject    VARCHAR(255) NOT NULL,
    body       TEXT         NOT NULL,
    updated_at VARCHAR(32)  NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE mail_outbox (
    id              BIGINT AUTO_INCREMENT PRIMARY KEY,
    to_email        VARCHAR(255) NOT NULL,
    template_name   VARCHAR(64)  NOT NULL,
    vars            TEXT         NOT NULL,
    status          VARCHAR(16)  NOT NULL DEFAULT 'queued',
    attempts        BIGINT     NOT NULL DEFAULT 0,
    error           TEXT,
    next_attempt_at VARCHAR(32),
    created_at      VARCHAR(32)  NOT NULL,
    sent_at         VARCHAR(32)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_mail_pickup ON mail_outbox(status, next_attempt_at);

-- ============================================================
-- 相册
-- ============================================================
CREATE TABLE albums (
    id            BIGINT AUTO_INCREMENT PRIMARY KEY,
    owner_id      BIGINT       NOT NULL,
    name          VARCHAR(128) NOT NULL,
    slug          VARCHAR(128) NOT NULL,
    description   TEXT,
    visibility    VARCHAR(16)  NOT NULL DEFAULT 'private',
    cover_blob_id BIGINT,
    image_count   BIGINT          NOT NULL DEFAULT 0,
    sort_order    BIGINT          NOT NULL DEFAULT 0,
    created_at    VARCHAR(32)  NOT NULL,
    updated_at    VARCHAR(32)  NOT NULL,
    UNIQUE KEY uq_albums_owner_slug (owner_id, slug),
    CONSTRAINT fk_albums_owner FOREIGN KEY (owner_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_albums_owner ON albums(owner_id, sort_order);

-- ============================================================
-- 物理文件实体（内容寻址）
-- ============================================================
CREATE TABLE blobs (
    id             BIGINT AUTO_INCREMENT PRIMARY KEY,
    sha256         VARCHAR(64)  NOT NULL,
    kind           VARCHAR(16)  NOT NULL DEFAULT 'original',
    size           BIGINT       NOT NULL,
    mime_type      VARCHAR(100) NOT NULL,
    ext            VARCHAR(16)  NOT NULL,
    width          BIGINT,
    height         BIGINT,
    storage_driver VARCHAR(32)  NOT NULL,
    storage_key    TEXT         NOT NULL,
    backend_id     BIGINT,
    ref_count      BIGINT          NOT NULL DEFAULT 0,
    created_at     VARCHAR(32)  NOT NULL,
    UNIQUE KEY uq_blobs_sha_kind (sha256, kind)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_blobs_driver ON blobs(storage_driver);
CREATE INDEX idx_blobs_kind   ON blobs(kind);
CREATE INDEX idx_blobs_gc     ON blobs(ref_count);

-- ============================================================
-- 图片与标签
-- ============================================================
CREATE TABLE images (
    id            BIGINT AUTO_INCREMENT PRIMARY KEY,
    owner_id      BIGINT       NOT NULL,
    album_id      BIGINT,
    blob_id       BIGINT       NOT NULL,
    thumb_blob_id BIGINT,
    original_name VARCHAR(255) NOT NULL,
    title         VARCHAR(255),
    visibility    VARCHAR(16)  NOT NULL DEFAULT 'private',
    views         BIGINT       NOT NULL DEFAULT 0,
    upload_time   VARCHAR(32)  NOT NULL,
    deleted_at    VARCHAR(32),
    CONSTRAINT fk_images_owner FOREIGN KEY (owner_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_images_album FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE SET NULL,
    CONSTRAINT fk_images_blob  FOREIGN KEY (blob_id)  REFERENCES blobs(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_images_owner_time ON images(owner_id, upload_time);
CREATE INDEX idx_images_album      ON images(album_id);
CREATE INDEX idx_images_blob       ON images(blob_id);
CREATE INDEX idx_images_thumb_blob ON images(thumb_blob_id);
CREATE INDEX idx_images_public     ON images(visibility, upload_time);

CREATE TABLE tags (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    owner_id   BIGINT      NOT NULL,
    name       VARCHAR(64) NOT NULL,
    created_at VARCHAR(32) NOT NULL,
    UNIQUE KEY uq_tags_owner_name (owner_id, name),
    CONSTRAINT fk_tags_owner FOREIGN KEY (owner_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_tags_owner_name ON tags(owner_id, name);

CREATE TABLE image_tags (
    image_id BIGINT NOT NULL,
    tag_id   BIGINT NOT NULL,
    PRIMARY KEY (image_id, tag_id),
    CONSTRAINT fk_image_tags_image FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE,
    CONSTRAINT fk_image_tags_tag   FOREIGN KEY (tag_id)   REFERENCES tags(id)   ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_image_tags_tag ON image_tags(tag_id);

-- ============================================================
-- 上传任务账本
-- ============================================================
CREATE TABLE upload_jobs (
    id            BIGINT AUTO_INCREMENT PRIMARY KEY,
    owner_id      BIGINT       NOT NULL,
    album_id      BIGINT,
    sha256        VARCHAR(64),
    original_name VARCHAR(255) NOT NULL,
    size          BIGINT       NOT NULL,
    status        VARCHAR(16)  NOT NULL DEFAULT 'queued',
    dedup         BIGINT   NOT NULL DEFAULT 0,
    image_id      BIGINT,
    attempts      BIGINT     NOT NULL DEFAULT 0,
    error         TEXT,
    created_at    VARCHAR(32)  NOT NULL,
    started_at    VARCHAR(32),
    finished_at   VARCHAR(32),
    CONSTRAINT fk_jobs_owner FOREIGN KEY (owner_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_jobs_album FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_jobs_pickup ON upload_jobs(status, created_at);
CREATE INDEX idx_jobs_owner  ON upload_jobs(owner_id, created_at);

-- ============================================================
-- API 令牌
-- ============================================================
CREATE TABLE api_tokens (
    id           BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id      BIGINT      NOT NULL,
    name         VARCHAR(64) NOT NULL,
    token_hash   VARCHAR(64) NOT NULL UNIQUE,
    display      VARCHAR(32) NOT NULL DEFAULT '',
    scopes       TEXT        NOT NULL,
    last_used_at VARCHAR(32),
    expires_at   VARCHAR(32),
    revoked_at   VARCHAR(32),
    created_at   VARCHAR(32) NOT NULL,
    CONSTRAINT fk_api_tokens_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_api_tokens_user ON api_tokens(user_id);

-- ============================================================
-- 频率限制策略
-- 生成列 is_global 在 user_id 非空时为 NULL，唯一索引放过多个 NULL，于是全局默认只允许一行。
-- ============================================================
CREATE TABLE rate_limit_policies (
    id                     BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id                BIGINT,
    is_global              BIGINT GENERATED ALWAYS AS (CASE WHEN user_id IS NULL THEN 1 ELSE NULL END) STORED,
    api_per_minute         BIGINT          NOT NULL DEFAULT 120,
    api_per_hour           BIGINT          NOT NULL DEFAULT 2000,
    api_read_per_minute    BIGINT          NOT NULL DEFAULT 600,
    api_burst              BIGINT          NOT NULL DEFAULT 20,
    upload_per_minute      BIGINT          NOT NULL DEFAULT 30,
    upload_per_hour        BIGINT          NOT NULL DEFAULT 500,
    upload_per_day         BIGINT          NOT NULL DEFAULT 2000,
    upload_bytes_per_day   BIGINT       NOT NULL DEFAULT 5368709120,
    max_concurrent_uploads BIGINT     NOT NULL DEFAULT 4,
    max_batch_size         BIGINT          NOT NULL DEFAULT 1000,
    max_file_size          BIGINT       NOT NULL DEFAULT 20971520,
    unlimited              BIGINT   NOT NULL DEFAULT 0,
    note                   TEXT,
    updated_at             VARCHAR(32)  NOT NULL,
    UNIQUE KEY uq_rate_user (user_id),
    UNIQUE KEY uq_rate_global (is_global),
    -- 生成列的基列上不许挂级联外键（MySQL 8 建表直接报 1215），这张表的
    -- 清理只能由应用来做：删账号前显式删掉它的策略行，见 repo::users::delete_user。
    CONSTRAINT fk_rate_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE rate_counters (
    id           BIGINT AUTO_INCREMENT PRIMARY KEY,
    scope        VARCHAR(16)  NOT NULL,
    subject      VARCHAR(128) NOT NULL,
    kind         VARCHAR(32)  NOT NULL,
    window_start VARCHAR(32)  NOT NULL,
    count        BIGINT       NOT NULL DEFAULT 0,
    bytes        BIGINT       NOT NULL DEFAULT 0,
    updated_at   VARCHAR(32)  NOT NULL,
    UNIQUE KEY uq_rate_counters (scope, subject, kind, window_start)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_rate_counters_window ON rate_counters(window_start);

-- ============================================================
-- IP 规则
-- ============================================================
CREATE TABLE ip_rules (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    cidr       VARCHAR(64) NOT NULL,
    kind       VARCHAR(8)  NOT NULL DEFAULT 'deny',
    scope      VARCHAR(16) NOT NULL DEFAULT 'all',
    note       TEXT,
    hits       BIGINT      NOT NULL DEFAULT 0,
    is_auto    BIGINT  NOT NULL DEFAULT 0,
    expires_at VARCHAR(32),
    created_by BIGINT,
    created_at VARCHAR(32) NOT NULL,
    CONSTRAINT fk_ip_rules_creator FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_ip_rules_lookup ON ip_rules(kind, scope, expires_at);

-- ============================================================
-- 服务条款
-- ============================================================
CREATE TABLE terms_versions (
    id                BIGINT AUTO_INCREMENT PRIMARY KEY,
    version           VARCHAR(32)  NOT NULL UNIQUE,
    title             VARCHAR(128) NOT NULL DEFAULT '服务条款',
    body              TEXT         NOT NULL,
    summary           TEXT,
    status            VARCHAR(16)  NOT NULL DEFAULT 'draft',
    active_flag       VARCHAR(16) GENERATED ALWAYS AS (CASE WHEN status = 'active' THEN 'active' ELSE NULL END) STORED,
    require_reconsent BIGINT   NOT NULL DEFAULT 1,
    effective_at      VARCHAR(32),
    published_at      VARCHAR(32),
    created_by        BIGINT,
    created_at        VARCHAR(32)  NOT NULL,
    UNIQUE KEY uq_terms_active (active_flag),
    CONSTRAINT fk_terms_creator FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE terms_consents (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id    BIGINT      NOT NULL,
    version_id BIGINT      NOT NULL,
    version    VARCHAR(32) NOT NULL,
    agreed_at  VARCHAR(32) NOT NULL,
    ip         VARCHAR(64),
    user_agent TEXT,
    UNIQUE KEY uq_terms_consent (user_id, version_id),
    CONSTRAINT fk_consents_user    FOREIGN KEY (user_id)    REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_consents_version FOREIGN KEY (version_id) REFERENCES terms_versions(id) ON DELETE RESTRICT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_terms_consents_user ON terms_consents(user_id, agreed_at);

-- ============================================================
-- 系统设置
-- ============================================================
CREATE TABLE settings (
    name       VARCHAR(64) PRIMARY KEY,
    value      TEXT,
    encrypted  BIGINT  NOT NULL DEFAULT 0,
    updated_at VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- 存储后端实例
-- 生成列 default_flag 保证默认后端只有一行。
-- ============================================================
CREATE TABLE storage_backends (
    id             BIGINT AUTO_INCREMENT PRIMARY KEY,
    driver         VARCHAR(32) NOT NULL,
    label          VARCHAR(64) NOT NULL,
    config         TEXT        NOT NULL,
    is_default     BIGINT  NOT NULL DEFAULT 0,
    default_flag   BIGINT GENERATED ALWAYS AS (CASE WHEN is_default THEN 1 ELSE NULL END) STORED,
    enabled        BIGINT  NOT NULL DEFAULT 1,
    capacity_mode  VARCHAR(16) NOT NULL DEFAULT 'auto',
    capacity_total BIGINT,
    created_at     VARCHAR(32) NOT NULL,
    UNIQUE KEY uq_backend_default (default_flag)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- 审计日志
-- ============================================================
CREATE TABLE audit_logs (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    actor_id   BIGINT,
    action     VARCHAR(64) NOT NULL,
    target     VARCHAR(128),
    detail     TEXT,
    ip         VARCHAR(64),
    created_at VARCHAR(32) NOT NULL,
    CONSTRAINT fk_audit_actor FOREIGN KEY (actor_id) REFERENCES users(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_audit_actor_time  ON audit_logs(actor_id, created_at);
CREATE INDEX idx_audit_action_time ON audit_logs(action, created_at);

-- ============================================================
-- 数据库内存储驱动（driver = database）
-- ============================================================
CREATE TABLE storage_objects (
    `key`      VARCHAR(255) PRIMARY KEY,
    content    LONGBLOB    NOT NULL,
    size       BIGINT      NOT NULL,
    updated_at VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- ============================================================
-- 二次验证的恢复码（一次性，只存哈希）
-- ============================================================
CREATE TABLE recovery_codes (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id    BIGINT      NOT NULL,
    code_hash  VARCHAR(64) NOT NULL,
    used_at    VARCHAR(32),
    created_at VARCHAR(32) NOT NULL,
    CONSTRAINT fk_recovery_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_recovery_user ON recovery_codes(user_id);
