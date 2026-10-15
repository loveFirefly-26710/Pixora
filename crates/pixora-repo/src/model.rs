//! 行模型与解码辅助。
//!
//! 解码只有一条路径：所有时间戳是 RFC3339 文本、所有整数是 64 位、
//! 所有 JSON 载荷是 JSON 文本。这样三种方言共用同一份 `from_row`。

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sqlx::any::AnyRow;
use sqlx::Row;

use crate::error::{RepoError, Result};

// ============================================================
// 时间戳
// ============================================================

/// 绑定用：把时间点转成定长 RFC3339 文本（UTC、微秒）。
pub fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// 当前时间。
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// 当前时间的存储形态。
pub fn now_ts() -> String {
    ts(Utc::now())
}

/// 解析存储的时间戳。空串按错误处理，它是脏数据不是「没有值」。
pub fn parse_ts(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            // 兼容手工插入或早期版本写下的 "YYYY-MM-DD HH:MM:SS" 形式
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f")
                .map(|naive| Utc.from_utc_datetime(&naive))
        })
        .map_err(|e| RepoError::Format(format!("时间戳 {raw:?} 无法解析：{e}")))
}

/// 给 SQL 用的时间表达式：把当前时间点的文本插进查询里做比较。
/// 走绑定参数而不是字符串拼接，避免出现两种写法。
pub fn ts_before(days: i64) -> String {
    ts(Utc::now() - chrono::Duration::days(days))
}

// ============================================================
// 列读取
// ============================================================

pub fn get_i64(row: &AnyRow, column: &str) -> Result<i64> {
    row.try_get::<i64, _>(column)
        .map_err(|e| RepoError::decode(column, e))
}

pub fn get_opt_i64(row: &AnyRow, column: &str) -> Result<Option<i64>> {
    row.try_get::<Option<i64>, _>(column)
        .map_err(|e| RepoError::decode(column, e))
}

pub fn get_i32(row: &AnyRow, column: &str) -> Result<i32> {
    Ok(get_i64(row, column)? as i32)
}

pub fn get_opt_i32(row: &AnyRow, column: &str) -> Result<Option<i32>> {
    Ok(get_opt_i64(row, column)?.map(|v| v as i32))
}

pub fn get_u32(row: &AnyRow, column: &str) -> Result<u32> {
    Ok(get_i64(row, column)?.max(0) as u32)
}

/// 二进制列。目前只有配置类资产（站点背景图）用得到。
pub fn get_bytes(row: &AnyRow, column: &str) -> Result<Vec<u8>> {
    row.try_get::<Vec<u8>, _>(column)
        .map_err(|e| RepoError::decode(column, e))
}

pub fn get_str(row: &AnyRow, column: &str) -> Result<String> {
    row.try_get::<String, _>(column)
        .map_err(|e| RepoError::decode(column, e))
}

pub fn get_opt_str(row: &AnyRow, column: &str) -> Result<Option<String>> {
    row.try_get::<Option<String>, _>(column)
        .map_err(|e| RepoError::decode(column, e))
}

/// 布尔列以 0/1 整数存储，这里统一转成 `bool`。
///
/// 不用驱动的原生布尔类型：`sqlx::Any` 在 SQLite 上会把布尔列报成整数类型，
/// 直接按 `bool` 解码会失败。三种方言统一存整数后读写只有一条路径。
pub fn get_bool(row: &AnyRow, column: &str) -> Result<bool> {
    Ok(get_i64(row, column)? != 0)
}

pub fn get_opt_bool(row: &AnyRow, column: &str) -> Result<Option<bool>> {
    Ok(get_opt_i64(row, column)?.map(|value| value != 0))
}

pub fn get_ts(row: &AnyRow, column: &str) -> Result<DateTime<Utc>> {
    parse_ts(&get_str(row, column)?)
}

pub fn get_opt_ts(row: &AnyRow, column: &str) -> Result<Option<DateTime<Utc>>> {
    match get_opt_str(row, column)? {
        Some(raw) => Ok(Some(parse_ts(&raw)?)),
        None => Ok(None),
    }
}

/// 读一列的原始文本，交给调用方解析 JSON。
pub fn get_json_raw(row: &AnyRow, column: &str) -> Result<String> {
    Ok(get_opt_str(row, column)?.unwrap_or_else(|| "null".into()))
}

pub fn get_json<T: DeserializeOwned>(row: &AnyRow, column: &str) -> Result<T> {
    let raw = get_json_raw(row, column)?;
    serde_json::from_str(&raw).map_err(|e| RepoError::decode(column, e))
}

pub fn get_opt_json<T: DeserializeOwned>(row: &AnyRow, column: &str) -> Result<Option<T>> {
    let raw = get_json_raw(row, column)?;
    if raw.trim().is_empty() || raw.trim() == "null" {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| RepoError::decode(column, e))
}

/// 序列化一个值用于入库。失败说明类型本身有问题，属于编码错误。
pub fn json_text<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| RepoError::Format(format!("JSON 序列化失败：{e}")))
}

/// 标签、权限范围这类「字符串数组存一列」的读法。
pub fn get_string_list(row: &AnyRow, column: &str) -> Result<Vec<String>> {
    let raw = get_json_raw(row, column)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).or_else(|_| {
        // 容忍逗号分隔的旧格式
        Ok(raw
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect())
    })
}

// ============================================================
// 角色与状态
// ============================================================

pub mod role {
    pub const ADMIN: &str = "admin";
    pub const USER: &str = "user";
}

pub mod status {
    /// 邮箱未验证
    pub const PENDING: &str = "pending";
    /// 正常
    pub const ACTIVE: &str = "active";
    /// 暂停使用
    pub const SUSPENDED: &str = "suspended";
    /// 封禁
    pub const BANNED: &str = "banned";

    pub fn label(value: &str) -> &'static str {
        match value {
            PENDING => "待验证",
            ACTIVE => "正常",
            SUSPENDED => "已暂停",
            BANNED => "已封禁",
            _ => "未知",
        }
    }
}

pub mod visibility {
    pub const PUBLIC: &str = "public";
    pub const PRIVATE: &str = "private";
}

pub mod job_status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const SKIPPED: &str = "skipped";
    pub const FAILED: &str = "failed";
}

pub mod storage_kind {
    pub const ORIGINAL: &str = "original";
    pub const THUMB: &str = "thumb";
}

pub mod terms_status {
    pub const DRAFT: &str = "draft";
    pub const ACTIVE: &str = "active";
    pub const ARCHIVED: &str = "archived";
}

pub mod ip_scope {
    pub const ALL: &str = "all";
    pub const REGISTER: &str = "register";
    pub const LOGIN: &str = "login";
    pub const UPLOAD: &str = "upload";
}

// ============================================================
// 实体
// ============================================================

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub uid: String,
    pub email: String,
    pub email_verified: bool,
    pub password_hash: String,
    pub role: String,
    pub status: String,
    pub status_reason: Option<String>,
    pub status_changed_at: Option<DateTime<Utc>>,
    pub status_changed_by: Option<i64>,
    pub totp_secret: Option<String>,
    pub totp_enabled: bool,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub login_fail_count: i32,
    pub locked_until: Option<DateTime<Utc>>,
    /// 内容审核自动暂停的到期时间。到点自动恢复；管理员手工暂停的行这里为空。
    pub auto_unsuspend_at: Option<DateTime<Utc>>,
    /// 管理员直接设过密码：下次登录必须自己改一个。没改之前只放行 /me 与改密码本身
    pub must_change_password: bool,
    /// 头像：站内相对路径（`/images/x.png`）或外链。空 = 没设，界面回落到邮箱首字母
    pub avatar: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_login_ip: Option<String>,
}

impl User {
    pub const COLUMNS: &'static str = "id, uid, email, email_verified, password_hash, role, status, \
         status_reason, status_changed_at, status_changed_by, totp_secret, totp_enabled, \
         quota_bytes, used_bytes, login_fail_count, locked_until, auto_unsuspend_at, \
         must_change_password, avatar, created_at, last_login_at, last_login_ip";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            uid: get_str(row, "uid")?,
            email: get_str(row, "email")?,
            email_verified: get_bool(row, "email_verified")?,
            password_hash: get_str(row, "password_hash")?,
            role: get_str(row, "role")?,
            status: get_str(row, "status")?,
            status_reason: get_opt_str(row, "status_reason")?,
            status_changed_at: get_opt_ts(row, "status_changed_at")?,
            status_changed_by: get_opt_i64(row, "status_changed_by")?,
            totp_secret: get_opt_str(row, "totp_secret")?,
            totp_enabled: get_bool(row, "totp_enabled")?,
            quota_bytes: get_i64(row, "quota_bytes")?,
            used_bytes: get_i64(row, "used_bytes")?,
            login_fail_count: get_i32(row, "login_fail_count")?,
            locked_until: get_opt_ts(row, "locked_until")?,
            auto_unsuspend_at: get_opt_ts(row, "auto_unsuspend_at")?,
            must_change_password: get_bool(row, "must_change_password")?,
            avatar: get_opt_str(row, "avatar")?,
            created_at: get_ts(row, "created_at")?,
            last_login_at: get_opt_ts(row, "last_login_at")?,
            last_login_ip: get_opt_str(row, "last_login_ip")?,
        })
    }

    pub fn is_admin(&self) -> bool {
        self.role == role::ADMIN
    }

    /// 只有 active 能登录和调用 API。判断集中在这里，鉴权中间件统一调用。
    pub fn is_active(&self) -> bool {
        self.status == status::ACTIVE
    }

    pub fn is_suspended(&self) -> bool {
        self.status == status::SUSPENDED
    }

    pub fn is_banned(&self) -> bool {
        self.status == status::BANNED
    }

    pub fn is_locked(&self, at: DateTime<Utc>) -> bool {
        self.locked_until.map(|until| until > at).unwrap_or(false)
    }

    pub fn remaining_quota(&self) -> i64 {
        (self.quota_bytes - self.used_bytes).max(0)
    }
}

/// 用户管理列表用的精简行，附带图片数量。
#[derive(Debug, Clone)]
pub struct UserSummary {
    pub id: i64,
    pub uid: String,
    pub email: String,
    /// 邮箱验证状态：后台详情页要据此显示徽章（以前没带这个字段，
    /// 界面上一律显示「未验证」，把已激活的账号也说成没验证）
    pub email_verified: bool,
    pub role: String,
    pub status: String,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub image_count: i64,
    /// 是否配置了用户级频率限制覆盖
    pub rate_custom: bool,
}

impl UserSummary {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            uid: get_str(row, "uid")?,
            email: get_str(row, "email")?,
            email_verified: get_bool(row, "email_verified")?,
            role: get_str(row, "role")?,
            status: get_str(row, "status")?,
            quota_bytes: get_i64(row, "quota_bytes")?,
            used_bytes: get_i64(row, "used_bytes")?,
            created_at: get_ts(row, "created_at")?,
            last_login_at: get_opt_ts(row, "last_login_at")?,
            image_count: get_i64(row, "image_count")?,
            rate_custom: get_bool(row, "rate_custom")?,
        })
    }

    pub fn is_admin(&self) -> bool {
        self.role == role::ADMIN
    }

    pub fn used_ratio(&self) -> f64 {
        if self.quota_bytes <= 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.quota_bytes as f64).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub user_id: i64,
    pub token_hash: String,
    pub device: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    /// 「记住我」签发的会话写绝对到期时间；普通会话为 `None`
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// 会话有效期策略。两个口径取自系统设置，管理员可改。
#[derive(Debug, Clone, Copy)]
pub struct SessionPolicy {
    /// 闲置多久算掉线（分钟）。0 = 不限
    pub idle_minutes: i64,
    /// 普通会话的最长寿命（天）。0 = 不限
    pub absolute_days: i64,
}

impl SessionPolicy {
    /// 设置没读出来时的保守默认：闲置两小时掉线，最长 30 天。
    pub fn fallback() -> Self {
        Self {
            idle_minutes: 120,
            absolute_days: 30,
        }
    }
}

impl Session {
    pub const COLUMNS: &'static str = "id, user_id, token_hash, device, ip, user_agent, created_at, \
         last_seen_at, expires_at, revoked_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_i64(row, "user_id")?,
            token_hash: get_str(row, "token_hash")?,
            device: get_opt_str(row, "device")?,
            ip: get_opt_str(row, "ip")?,
            user_agent: get_opt_str(row, "user_agent")?,
            created_at: get_ts(row, "created_at")?,
            last_seen_at: get_ts(row, "last_seen_at")?,
            expires_at: get_opt_ts(row, "expires_at")?,
            revoked_at: get_opt_ts(row, "revoked_at")?,
        })
    }

    /// 只吊销标记。到期判断走 [`Self::is_expired`]，它需要设置里的策略。
    pub fn is_live(&self) -> bool {
        self.revoked_at.is_none()
    }

    /// 是否已经过期。两种口径：
    ///
    ///   - 「记住我」签发的会话由签发时写下的 `expires_at` 决定，不再看闲置时长
    ///     （选了记住我，隔几天打开还应该是登录状态）；
    ///   - 普通会话没有 `expires_at`，按「闲置超过 idle_minutes」与
    ///     「从建立起超过 absolute_days」两条判——前者对应控制台上的超时重登，
    ///     后者给长期挂着不动的会话兜底。
    ///
    /// 两个口径都配 0 就是「永不过期」，这一项留给自建场景。
    pub fn is_expired(&self, policy: SessionPolicy, now: DateTime<Utc>) -> bool {
        if let Some(at) = self.expires_at {
            return at <= now;
        }
        if policy.idle_minutes > 0
            && now - self.last_seen_at > chrono::Duration::minutes(policy.idle_minutes)
        {
            return true;
        }
        if policy.absolute_days > 0 && now - self.created_at > chrono::Duration::days(policy.absolute_days)
        {
            return true;
        }
        false
    }

    /// 可用 = 没被吊销 且 没过期。列表与鉴权共用这一个判断。
    pub fn is_usable(&self, policy: SessionPolicy, now: DateTime<Utc>) -> bool {
        self.is_live() && !self.is_expired(policy, now)
    }
}

#[derive(Debug, Clone)]
pub struct EmailVerification {
    pub id: i64,
    pub email: String,
    pub code_hash: String,
    pub purpose: String,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub attempts: i32,
    pub request_ip: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl EmailVerification {
    pub const COLUMNS: &'static str = "id, email, code_hash, purpose, expires_at, consumed_at, \
         attempts, request_ip, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            email: get_str(row, "email")?,
            code_hash: get_str(row, "code_hash")?,
            purpose: get_str(row, "purpose")?,
            expires_at: get_ts(row, "expires_at")?,
            consumed_at: get_opt_ts(row, "consumed_at")?,
            attempts: get_i32(row, "attempts")?,
            request_ip: get_opt_str(row, "request_ip")?,
            created_at: get_ts(row, "created_at")?,
        })
    }

    pub fn is_usable(&self, at: DateTime<Utc>) -> bool {
        self.consumed_at.is_none() && self.expires_at > at
    }
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub visibility: String,
    pub cover_blob_id: Option<i64>,
    pub image_count: i64,
    pub sort_order: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 被移进回收站的时间；`None` 表示正常可见
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Album {
    pub const COLUMNS: &'static str = "id, owner_id, name, slug, description, visibility, \
         cover_blob_id, image_count, sort_order, created_at, updated_at, deleted_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            owner_id: get_i64(row, "owner_id")?,
            name: get_str(row, "name")?,
            slug: get_str(row, "slug")?,
            description: get_opt_str(row, "description")?,
            visibility: get_str(row, "visibility")?,
            cover_blob_id: get_opt_i64(row, "cover_blob_id")?,
            image_count: get_i64(row, "image_count")?,
            sort_order: get_i64(row, "sort_order")?,
            created_at: get_ts(row, "created_at")?,
            updated_at: get_ts(row, "updated_at")?,
            deleted_at: get_opt_ts(row, "deleted_at")?,
        })
    }

    /// 是否在回收站里。
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn is_public(&self) -> bool {
        self.visibility == visibility::PUBLIC
    }
}

#[derive(Debug, Clone)]
pub struct Blob {
    pub id: i64,
    pub sha256: String,
    pub kind: String,
    pub size: i64,
    pub mime_type: String,
    pub ext: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub storage_driver: String,
    pub storage_key: String,
    pub backend_id: Option<i64>,
    pub ref_count: i64,
    pub created_at: DateTime<Utc>,
}

impl Blob {
    pub const COLUMNS: &'static str = "id, sha256, kind, size, mime_type, ext, width, height, \
         storage_driver, storage_key, backend_id, ref_count, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            sha256: get_str(row, "sha256")?,
            kind: get_str(row, "kind")?,
            size: get_i64(row, "size")?,
            mime_type: get_str(row, "mime_type")?,
            ext: get_str(row, "ext")?,
            width: get_opt_i32(row, "width")?,
            height: get_opt_i32(row, "height")?,
            storage_driver: get_str(row, "storage_driver")?,
            storage_key: get_str(row, "storage_key")?,
            backend_id: get_opt_i64(row, "backend_id")?,
            ref_count: get_i64(row, "ref_count")?,
            created_at: get_ts(row, "created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Image {
    pub id: i64,
    pub owner_id: i64,
    pub album_id: Option<i64>,
    pub blob_id: i64,
    pub thumb_blob_id: Option<i64>,
    pub original_name: String,
    pub title: Option<String>,
    pub visibility: String,
    pub views: i64,
    pub upload_time: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// 这张图是「随相册一起」进的回收站：恢复相册时只带回这一批，
    /// 不碰用户早先单独删掉的那些
    pub deleted_by_album: bool,
}

impl Image {
    pub const COLUMNS: &'static str = "id, owner_id, album_id, blob_id, thumb_blob_id, \
         original_name, title, visibility, views, upload_time, deleted_at, deleted_by_album";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            owner_id: get_i64(row, "owner_id")?,
            album_id: get_opt_i64(row, "album_id")?,
            blob_id: get_i64(row, "blob_id")?,
            thumb_blob_id: get_opt_i64(row, "thumb_blob_id")?,
            original_name: get_str(row, "original_name")?,
            title: get_opt_str(row, "title")?,
            visibility: get_str(row, "visibility")?,
            views: get_i64(row, "views")?,
            upload_time: get_ts(row, "upload_time")?,
            deleted_at: get_opt_ts(row, "deleted_at")?,
            deleted_by_album: get_bool(row, "deleted_by_album")?,
        })
    }

    pub fn is_public(&self) -> bool {
        self.visibility == visibility::PUBLIC
    }

    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.original_name)
    }
}

/// 列表与详情用的合成视图：图片 + 物理属性 + 相册 + 标签。
#[derive(Debug, Clone)]
pub struct ImageView {
    pub id: i64,
    pub owner_id: i64,
    pub owner_uid: String,
    pub album_id: Option<i64>,
    pub album_name: Option<String>,
    pub title: Option<String>,
    pub original_name: String,
    pub visibility: String,
    pub views: i64,
    pub upload_time: DateTime<Utc>,
    pub sha256: String,
    pub ext: String,
    pub mime_type: String,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub thumb_blob_id: Option<i64>,
    pub storage_key: String,
    pub storage_driver: String,
    pub tags: Vec<String>,
}

impl ImageView {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            owner_id: get_i64(row, "owner_id")?,
            owner_uid: get_opt_str(row, "owner_uid")?.unwrap_or_default(),
            album_id: get_opt_i64(row, "album_id")?,
            album_name: get_opt_str(row, "album_name")?,
            title: get_opt_str(row, "title")?,
            original_name: get_str(row, "original_name")?,
            visibility: get_str(row, "visibility")?,
            views: get_i64(row, "views")?,
            upload_time: get_ts(row, "upload_time")?,
            sha256: get_str(row, "sha256")?,
            ext: get_str(row, "ext")?,
            mime_type: get_str(row, "mime_type")?,
            size: get_i64(row, "size")?,
            width: get_opt_i32(row, "width")?,
            height: get_opt_i32(row, "height")?,
            thumb_blob_id: get_opt_i64(row, "thumb_blob_id")?,
            storage_key: get_str(row, "storage_key")?,
            storage_driver: get_str(row, "storage_driver")?,
            tags: Vec::new(), // 由仓储层单独填充，避免在列表查询里做聚合
        })
    }

    pub fn is_public(&self) -> bool {
        self.visibility == visibility::PUBLIC
    }

    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.original_name)
    }

    pub fn has_thumb(&self) -> bool {
        self.thumb_blob_id.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Tag {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// 使用该标签的图片数（聚合查询时才填）
    pub usage: i64,
}

impl Tag {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            owner_id: get_i64(row, "owner_id")?,
            name: get_str(row, "name")?,
            created_at: get_ts(row, "created_at")?,
            usage: get_opt_i64(row, "usage")?.unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone)]
pub struct UploadJob {
    pub id: i64,
    pub owner_id: i64,
    pub album_id: Option<i64>,
    pub sha256: Option<String>,
    pub original_name: String,
    pub size: i64,
    pub status: String,
    pub dedup: bool,
    pub image_id: Option<i64>,
    pub attempts: i32,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl UploadJob {
    pub const COLUMNS: &'static str = "id, owner_id, album_id, sha256, original_name, size, status, \
         dedup, image_id, attempts, error, created_at, started_at, finished_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            owner_id: get_i64(row, "owner_id")?,
            album_id: get_opt_i64(row, "album_id")?,
            sha256: get_opt_str(row, "sha256")?,
            original_name: get_str(row, "original_name")?,
            size: get_i64(row, "size")?,
            status: get_str(row, "status")?,
            dedup: get_bool(row, "dedup")?,
            image_id: get_opt_i64(row, "image_id")?,
            attempts: get_i32(row, "attempts")?,
            error: get_opt_str(row, "error")?,
            created_at: get_ts(row, "created_at")?,
            started_at: get_opt_ts(row, "started_at")?,
            finished_at: get_opt_ts(row, "finished_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ApiToken {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub token_hash: String,
    pub display: String,
    pub scopes: Vec<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ApiToken {
    pub const COLUMNS: &'static str = "id, user_id, name, token_hash, display, scopes, \
         last_used_at, expires_at, revoked_at, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_i64(row, "user_id")?,
            name: get_str(row, "name")?,
            token_hash: get_str(row, "token_hash")?,
            display: get_str(row, "display")?,
            scopes: get_string_list(row, "scopes")?,
            last_used_at: get_opt_ts(row, "last_used_at")?,
            expires_at: get_opt_ts(row, "expires_at")?,
            revoked_at: get_opt_ts(row, "revoked_at")?,
            created_at: get_ts(row, "created_at")?,
        })
    }

    pub fn is_usable(&self, at: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at.map(|exp| exp > at).unwrap_or(true)
    }

    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|item| item == scope) || self.scopes.iter().any(|s| s == "admin")
    }
}

/// 命中过限流的用户（管理后台「实时用量」用）。
#[derive(Debug, Clone)]
pub struct RateHit {
    pub uid: String,
    pub email: String,
    pub hits: i64,
}

#[derive(Debug, Clone)]
pub struct RateLimitPolicy {
    pub id: i64,
    pub user_id: Option<i64>,
    pub api_per_minute: i64,
    pub api_per_hour: i64,
    pub api_read_per_minute: i64,
    pub api_burst: i64,
    pub upload_per_minute: i64,
    pub upload_per_hour: i64,
    pub upload_per_day: i64,
    pub upload_bytes_per_day: i64,
    pub max_concurrent_uploads: i64,
    /// 上传策略的几项：`0`（数值）与空串（格式）都表示「跟随上一层」。
    /// 解析顺序（见 `pixora_core::uploads::effective_upload_limits`）：
    /// 单用户覆盖 → 全局策略行 → 系统设置里的站点默认
    pub max_batch_size: i64,
    pub max_file_size: i64,
    pub max_pixels: i64,
    /// 长宽上限（像素）。与 `max_pixels` 一起判：三者都过才收
    pub max_width: i64,
    pub max_height: i64,
    /// 逗号分隔的扩展名列表，与 settings 里的 `upload.formats` 同一种写法
    pub formats: Option<String>,
    pub unlimited: bool,
    pub note: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl RateLimitPolicy {
    pub const COLUMNS: &'static str = "id, user_id, api_per_minute, api_per_hour, api_read_per_minute, \
         api_burst, upload_per_minute, upload_per_hour, upload_per_day, upload_bytes_per_day, \
         max_concurrent_uploads, max_batch_size, max_file_size, max_pixels, max_width, max_height, \
         formats, unlimited, note, updated_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_opt_i64(row, "user_id")?,
            api_per_minute: get_i64(row, "api_per_minute")?,
            api_per_hour: get_i64(row, "api_per_hour")?,
            api_read_per_minute: get_i64(row, "api_read_per_minute")?,
            api_burst: get_i64(row, "api_burst")?,
            upload_per_minute: get_i64(row, "upload_per_minute")?,
            upload_per_hour: get_i64(row, "upload_per_hour")?,
            upload_per_day: get_i64(row, "upload_per_day")?,
            upload_bytes_per_day: get_i64(row, "upload_bytes_per_day")?,
            max_concurrent_uploads: get_i64(row, "max_concurrent_uploads")?,
            max_batch_size: get_i64(row, "max_batch_size")?,
            max_file_size: get_i64(row, "max_file_size")?,
            max_pixels: get_i64(row, "max_pixels")?,
            max_width: get_i64(row, "max_width")?,
            max_height: get_i64(row, "max_height")?,
            formats: get_opt_str(row, "formats")?,
            unlimited: get_bool(row, "unlimited")?,
            note: get_opt_str(row, "note")?,
            updated_at: get_ts(row, "updated_at")?,
        })
    }

    pub fn is_global(&self) -> bool {
        self.user_id.is_none()
    }

    /// 全局默认值。管理员和未覆盖用户都从这里起步。
    ///
    /// 上传策略的四项（单批张数、单文件上限、像素上限、格式白名单）默认给 0 / 空：
    /// 它们表示「跟随系统设置里的站点默认」，内置一份数字只会和系统设置打架。
    pub fn defaults() -> Self {
        Self {
            id: 0,
            user_id: None,
            api_per_minute: 120,
            api_per_hour: 2000,
            api_read_per_minute: 600,
            api_burst: 20,
            upload_per_minute: 30,
            upload_per_hour: 500,
            upload_per_day: 2000,
            upload_bytes_per_day: 5 * 1024 * 1024 * 1024,
            max_concurrent_uploads: 4,
            max_batch_size: 0,
            max_file_size: 0,
            max_pixels: 0,
            max_width: 0,
            max_height: 0,
            formats: None,
            unlimited: false,
            note: None,
            updated_at: now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IpRule {
    pub id: i64,
    pub cidr: String,
    pub kind: String,
    pub scope: String,
    pub note: Option<String>,
    pub hits: i64,
    pub is_auto: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_by: Option<i64>,
    pub created_at: DateTime<Utc>,
}

impl IpRule {
    pub const COLUMNS: &'static str = "id, cidr, kind, scope, note, hits, is_auto, expires_at, \
         created_by, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            cidr: get_str(row, "cidr")?,
            kind: get_str(row, "kind")?,
            scope: get_str(row, "scope")?,
            note: get_opt_str(row, "note")?,
            hits: get_i64(row, "hits")?,
            is_auto: get_bool(row, "is_auto")?,
            expires_at: get_opt_ts(row, "expires_at")?,
            created_by: get_opt_i64(row, "created_by")?,
            created_at: get_ts(row, "created_at")?,
        })
    }

    pub fn is_allow(&self) -> bool {
        self.kind == "allow"
    }

    pub fn is_expired(&self, at: DateTime<Utc>) -> bool {
        self.expires_at.map(|exp| exp <= at).unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
pub struct TermsVersion {
    pub id: i64,
    pub version: String,
    pub title: String,
    pub body: String,
    pub summary: Option<String>,
    pub status: String,
    pub require_reconsent: bool,
    pub effective_at: Option<DateTime<Utc>>,
    pub published_at: Option<DateTime<Utc>>,
    pub created_by: Option<i64>,
    pub created_at: DateTime<Utc>,
}

impl TermsVersion {
    pub const COLUMNS: &'static str = "id, version, title, body, summary, status, require_reconsent, \
         effective_at, published_at, created_by, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            version: get_str(row, "version")?,
            title: get_str(row, "title")?,
            body: get_str(row, "body")?,
            summary: get_opt_str(row, "summary")?,
            status: get_str(row, "status")?,
            require_reconsent: get_bool(row, "require_reconsent")?,
            effective_at: get_opt_ts(row, "effective_at")?,
            published_at: get_opt_ts(row, "published_at")?,
            created_by: get_opt_i64(row, "created_by")?,
            created_at: get_ts(row, "created_at")?,
        })
    }

    pub fn is_active(&self) -> bool {
        self.status == terms_status::ACTIVE
    }

    /// 条款条数，界面要显示「共 6 条」。
    pub fn article_count(&self) -> usize {
        self.body
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                line.starts_with('一')
                    || line.starts_with('二')
                    || line.starts_with('三')
                    || line.starts_with('四')
                    || line.starts_with('五')
                    || line.starts_with('六')
                    || line.starts_with('七')
                    || line.starts_with('八')
                    || line.starts_with('九')
                    || line.starts_with('十')
                    || line.starts_with('#')
            })
            .count()
    }
}

#[derive(Debug, Clone)]
pub struct TermsConsent {
    pub id: i64,
    pub user_id: i64,
    pub version_id: i64,
    pub version: String,
    pub agreed_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl TermsConsent {
    pub const COLUMNS: &'static str =
        "id, user_id, version_id, version, agreed_at, ip, user_agent";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_i64(row, "user_id")?,
            version_id: get_i64(row, "version_id")?,
            version: get_str(row, "version")?,
            agreed_at: get_ts(row, "agreed_at")?,
            ip: get_opt_str(row, "ip")?,
            user_agent: get_opt_str(row, "user_agent")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Setting {
    pub name: String,
    pub value: Option<String>,
    pub encrypted: bool,
    pub updated_at: DateTime<Utc>,
}

impl Setting {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            name: get_str(row, "name")?,
            value: get_opt_str(row, "value")?,
            encrypted: get_bool(row, "encrypted")?,
            updated_at: get_ts(row, "updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct StorageBackend {
    pub id: i64,
    pub driver: String,
    pub label: String,
    pub config: serde_json::Value,
    pub is_default: bool,
    pub enabled: bool,
    pub capacity_mode: String,
    pub capacity_total: Option<i64>,
    pub created_at: DateTime<Utc>,
}

impl StorageBackend {
    pub const COLUMNS: &'static str = "id, driver, label, config, is_default, enabled, \
         capacity_mode, capacity_total, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            driver: get_str(row, "driver")?,
            label: get_str(row, "label")?,
            config: get_json(row, "config")?,
            is_default: get_bool(row, "is_default")?,
            enabled: get_bool(row, "enabled")?,
            capacity_mode: get_str(row, "capacity_mode")?,
            capacity_total: get_opt_i64(row, "capacity_total")?,
            created_at: get_ts(row, "created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    pub id: i64,
    pub actor_id: Option<i64>,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
    pub ip: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl AuditLog {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            actor_id: get_opt_i64(row, "actor_id")?,
            action: get_str(row, "action")?,
            target: get_opt_str(row, "target")?,
            detail: get_json(row, "detail")?,
            ip: get_opt_str(row, "ip")?,
            created_at: get_ts(row, "created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct UserAction {
    pub id: i64,
    pub user_id: i64,
    pub actor_id: Option<i64>,
    pub action: String,
    pub reason: Option<String>,
    pub detail: serde_json::Value,
    pub mail_sent: bool,
    pub created_at: DateTime<Utc>,
}

impl UserAction {
    pub const COLUMNS: &'static str =
        "id, user_id, actor_id, action, reason, detail, mail_sent, created_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_i64(row, "user_id")?,
            actor_id: get_opt_i64(row, "actor_id")?,
            action: get_str(row, "action")?,
            reason: get_opt_str(row, "reason")?,
            detail: get_json(row, "detail")?,
            mail_sent: get_bool(row, "mail_sent")?,
            created_at: get_ts(row, "created_at")?,
        })
    }

    pub fn action_label(&self) -> String {
        match self.action.as_str() {
            "suspend" => "暂停".into(),
            "unsuspend" => "恢复".into(),
            "ban" => "封禁".into(),
            "unban" => "解封".into(),
            "register" => "注册".into(),
            "reset_password" => "重置密码".into(),
            "change_email" => "改邮箱".into(),
            "quota_change" => "调配额".into(),
            "delete" => "删除账号".into(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MailOutboxItem {
    pub id: i64,
    pub to_email: String,
    pub template_name: String,
    pub vars: serde_json::Value,
    pub status: String,
    pub attempts: i32,
    pub error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
}

impl MailOutboxItem {
    pub const COLUMNS: &'static str = "id, to_email, template_name, vars, status, attempts, error, \
         next_attempt_at, created_at, sent_at";

    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            to_email: get_str(row, "to_email")?,
            template_name: get_str(row, "template_name")?,
            vars: get_json(row, "vars")?,
            status: get_str(row, "status")?,
            attempts: get_i32(row, "attempts")?,
            error: get_opt_str(row, "error")?,
            next_attempt_at: get_opt_ts(row, "next_attempt_at")?,
            created_at: get_ts(row, "created_at")?,
            sent_at: get_opt_ts(row, "sent_at")?,
        })
    }

    pub fn is_sent(&self) -> bool {
        self.status == "sent"
    }
}

#[derive(Debug, Clone)]
pub struct EmailTemplate {
    pub name: String,
    pub subject: String,
    pub body: String,
    pub updated_at: DateTime<Utc>,
}

impl EmailTemplate {
    pub fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            name: get_str(row, "name")?,
            subject: get_str(row, "subject")?,
            body: get_str(row, "body")?,
            updated_at: get_ts(row, "updated_at")?,
        })
    }
}

// ============================================================
// 统计
// ============================================================

#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub image_count: i64,
    /// 还在被引用的实体字节（在用 + 回收站）
    pub total_bytes: i64,
    pub logical_bytes: i64,
    pub dedup_saved_bytes: i64,
    /// 回收站里还没过保留期的张数与占用
    pub binned_count: i64,
    pub binned_bytes: i64,
    /// 已经没人引用、等后台回收的实体（界面上单独一行）
    pub pending_count: i64,
    pub pending_bytes: i64,
    pub average_size: i64,
    pub drivers: Vec<DriverStat>,
    pub mimes: Vec<MimeStat>,
    /// 近 30 天每日图片数
    pub trend: Vec<TrendPoint>,
    /// 今日每小时图片数
    pub hourly: Vec<TrendPoint>,
    pub recent: Vec<ImageView>,
}

#[derive(Debug, Clone)]
pub struct DriverStat {
    pub driver: String,
    /// 图片张数（不是实体条数：一张图带一个缩略图不该显示成两张）
    pub count: i64,
    /// 这个后端上仍被引用的实体字节（原图 + 缩略图，每个实体只算一次）
    pub bytes: i64,
}

#[derive(Debug, Clone)]
pub struct MimeStat {
    pub mime: String,
    pub count: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone)]
pub struct TrendPoint {
    pub bucket: String,
    pub count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct UserStats {
    pub album_count: i64,
    pub image_count: i64,
    pub public_count: i64,
    pub tag_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AdminStats {
    pub user_total: i64,
    pub user_new_this_month: i64,
    pub allocated_quota: i64,
    pub used_bytes: i64,
    pub image_total: i64,
    pub storage_bytes: i64,
    pub register_open: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_roundtrip_is_lexicographically_ordered() {
        let early = ts(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap());
        let late = ts(Utc.with_ymd_and_hms(2026, 10, 14, 15, 30, 0).unwrap());
        assert_eq!(early, "2026-01-02T03:04:05.000000Z");
        assert!(early < late, "定长 RFC3339 的字典序必须与时间序一致");
        assert_eq!(parse_ts(&late).unwrap(), Utc.with_ymd_and_hms(2026, 10, 14, 15, 30, 0).unwrap());
    }

    #[test]
    fn parse_tolerates_manual_edits() {
        let parsed = parse_ts("2026-10-14 15:30:00").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-10-14T15:30:00+00:00");
        assert!(parse_ts("").is_err());
    }

    #[test]
    fn token_scopes_fall_back_to_comma_list() {
        let scopes = serde_json::from_str::<Vec<String>>("[\"upload\",\"read\"]").unwrap();
        assert_eq!(scopes.len(), 2);
    }

    #[test]
    fn user_helpers() {
        let mut user = User {
            id: 1,
            uid: "1001".into(),
            email: "a@b.c".into(),
            email_verified: true,
            password_hash: String::new(),
            role: role::ADMIN.into(),
            status: status::ACTIVE.into(),
            status_reason: None,
            status_changed_at: None,
            status_changed_by: None,
            totp_secret: None,
            totp_enabled: false,
            quota_bytes: 100,
            used_bytes: 40,
            login_fail_count: 0,
            locked_until: None,
            auto_unsuspend_at: None,
            must_change_password: false,
            avatar: None,
            created_at: now(),
            last_login_at: None,
            last_login_ip: None,
        };
        assert!(user.is_admin());
        assert!(user.is_active());
        assert_eq!(user.remaining_quota(), 60);
        user.status = status::SUSPENDED.into();
        assert!(user.is_suspended());
        assert!(!user.is_banned());
    }

    #[test]
    fn terms_article_counting() {
        let terms = TermsVersion {
            id: 1,
            version: "v2.1".into(),
            title: "服务条款".into(),
            body: "一、服务说明\n正文\n二、账号与验证\n正文\n三、上传限制\n正文".into(),
            summary: None,
            status: terms_status::ACTIVE.into(),
            require_reconsent: true,
            effective_at: None,
            published_at: None,
            created_by: None,
            created_at: now(),
        };
        assert_eq!(terms.article_count(), 3);
    }
}
