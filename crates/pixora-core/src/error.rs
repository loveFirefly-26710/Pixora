//! 领域错误与错误码。
//!
//! 错误码是给客户端看的契约（见 `docs/api-contract.md` 1.2）：暂停和封禁都是 403，
//! 只给状态码的话 PicGo 这类客户端没法告诉用户「可以申诉」还是「已被封禁」，
//! 两种情况的提示文案完全不一样。所以每个错误都必须自带一个稳定的 `code`。

use pixora_crypto::CryptoError;
use pixora_mail::MailError;
use pixora_repo::RepoError;
use pixora_storage::StorageError;

#[derive(Debug)]
pub enum AppError {
    Config(String),
    BadRequest(String),
    Unauthorized,
    InvalidCredentials,
    TokenRevoked,
    Csrf,
    AccountPending,
    AccountSuspended {
        reason: Option<String>,
        suspended_at: Option<String>,
    },
    AccountBanned {
        reason: Option<String>,
    },
    AccountLocked {
        locked_until: Option<String>,
    },
    IpBlocked,
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    QuotaExceeded {
        detail: String,
    },
    FileTooLarge {
        size: u64,
        limit: u64,
    },
    UnsupportedFormat(String),
    InvalidImage(String),
    /// 内容审核判为违规，上传被拦下
    ContentRejected {
        label: String,
        score: f64,
        provider: String,
    },
    RateLimited {
        retry_after: u64,
        detail: String,
    },
    QueueFull {
        retry_after: u64,
    },
    TermsRequired,
    TotpRequired,
    TotpInvalid(String),
    StorageUnavailable(String),
    Internal(String),
    Repo(RepoError),
    Storage(StorageError),
    Mail(MailError),
    Crypto(CryptoError),
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    /// 稳定的错误码。
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Config(_) => "INTERNAL",
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::Unauthorized => "UNAUTHORIZED",
            AppError::InvalidCredentials => "INVALID_CREDENTIALS",
            AppError::TokenRevoked => "TOKEN_REVOKED",
            AppError::Csrf => "CSRF_FAILED",
            AppError::AccountPending => "ACCOUNT_PENDING",
            AppError::AccountSuspended { .. } => "ACCOUNT_SUSPENDED",
            AppError::AccountBanned { .. } => "ACCOUNT_BANNED",
            AppError::AccountLocked { .. } => "ACCOUNT_LOCKED",
            AppError::IpBlocked => "IP_BLOCKED",
            AppError::Forbidden(_) => "FORBIDDEN",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::Conflict(_) => "CONFLICT",
            AppError::QuotaExceeded { .. } => "QUOTA_EXCEEDED",
            AppError::FileTooLarge { .. } => "FILE_TOO_LARGE",
            AppError::UnsupportedFormat(_) => "UNSUPPORTED_FORMAT",
            AppError::InvalidImage(_) => "INVALID_IMAGE",
            AppError::ContentRejected { .. } => "CONTENT_REJECTED",
            AppError::RateLimited { .. } => "RATE_LIMITED",
            AppError::QueueFull { .. } => "QUEUE_FULL",
            AppError::TermsRequired => "TERMS_REQUIRED",
            AppError::TotpRequired => "TOTP_REQUIRED",
            AppError::TotpInvalid(_) => "BAD_REQUEST",
            AppError::StorageUnavailable(_) => "STORAGE_UNAVAILABLE",
            AppError::Internal(_) => "INTERNAL",
            AppError::Repo(_) => "INTERNAL",
            AppError::Storage(err) => {
                if matches!(err, StorageError::NotFound(_)) {
                    "NOT_FOUND"
                } else {
                    "STORAGE_UNAVAILABLE"
                }
            }
            AppError::Mail(_) => "INTERNAL",
            AppError::Crypto(_) => "INTERNAL",
        }
    }

    /// 这次失败还值不值得重试。
    ///
    /// 目前只有邮件一类带这个信息：收件服务器明确拒收（5xx）与认证失败都是
    /// 一次性的结论，发件箱不该再拿它排队重发——重发只是往发信账号里堆退信。
    pub fn is_permanent(&self) -> bool {
        match self {
            AppError::Mail(err) => err.is_permanent(),
            _ => false,
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            AppError::BadRequest(_) | AppError::TotpInvalid(_) | AppError::Config(_) => 400,
            AppError::TotpRequired => 400,
            AppError::Unauthorized | AppError::InvalidCredentials | AppError::TokenRevoked => 401,
            AppError::Csrf
            | AppError::AccountPending
            | AppError::AccountSuspended { .. }
            | AppError::AccountBanned { .. }
            | AppError::AccountLocked { .. }
            | AppError::IpBlocked
            | AppError::Forbidden(_) => 403,
            AppError::NotFound(_) => 404,
            AppError::Conflict(_) => 409,
            AppError::QuotaExceeded { .. } | AppError::FileTooLarge { .. } => 413,
            AppError::UnsupportedFormat(_) => 415,
            AppError::InvalidImage(_) => 422,
            // 违规内容不是「请求格式不对」而是「内容不行」，422 比 400 更贴切，
            // 客户端能靠 code 区分出「被审核拦下」并给出对应文案
            AppError::ContentRejected { .. } => 422,
            AppError::RateLimited { .. } | AppError::QueueFull { .. } => 429,
            AppError::TermsRequired => 451,
            AppError::StorageUnavailable(_) => 503,
            AppError::Internal(_) | AppError::Repo(_) | AppError::Mail(_) | AppError::Crypto(_) => 500,
            AppError::Storage(err) => {
                if matches!(err, StorageError::NotFound(_)) {
                    404
                } else {
                    503
                }
            }
        }
    }

    /// 给用户看的中文说明。
    pub fn message(&self) -> String {
        match self {
            AppError::Config(message) => format!("服务配置有误：{message}"),
            AppError::BadRequest(message) => message.clone(),
            AppError::Unauthorized => "请先登录".into(),
            AppError::InvalidCredentials => "邮箱或密码错误".into(),
            AppError::TokenRevoked => "Token 已撤销或过期".into(),
            AppError::Csrf => "请求校验失败，请刷新页面后重试".into(),
            AppError::AccountPending => "邮箱未验证，账号尚未激活".into(),
            AppError::AccountSuspended { .. } => "账号已被暂停使用，无法调用 API".into(),
            AppError::AccountBanned { .. } => "账号已被封禁，请联系管理员".into(),
            AppError::AccountLocked { .. } => "连续登录失败次数过多，账号已被临时锁定".into(),
            AppError::IpBlocked => "你的网络地址已被拒绝访问".into(),
            AppError::Forbidden(message) => {
                if message.is_empty() {
                    "没有权限执行该操作".into()
                } else {
                    message.clone()
                }
            }
            AppError::NotFound(what) => format!("{what}不存在"),
            AppError::Conflict(message) => message.clone(),
            AppError::QuotaExceeded { detail } => {
                if detail.is_empty() {
                    "超出账号配额".into()
                } else {
                    format!("超出账号配额：{detail}")
                }
            }
            AppError::FileTooLarge { size, limit } => {
                format!("文件过大（{}），单文件上限 {}", human(*size), human(*limit))
            }
            AppError::UnsupportedFormat(ext) => format!("不支持的图片格式：{ext}"),
            AppError::InvalidImage(detail) => format!("文件不是有效的图片：{detail}"),
            AppError::ContentRejected { label, score, .. } => format!(
                "图片未通过内容审核（{label}，{} 分）。请勿上传违规内容，多次违规会被暂停使用。",
                score.round() as i64
            ),
            AppError::RateLimited { detail, .. } => {
                if detail.is_empty() {
                    "请求过于频繁，请稍后重试".into()
                } else {
                    detail.clone()
                }
            }
            AppError::QueueFull { .. } => "服务器上传队列已满，请稍后重试".into(),
            AppError::TermsRequired => "需要先同意服务条款才能继续使用".into(),
            AppError::TotpRequired => "该账号已开启二次验证，请输入动态验证码".into(),
            AppError::TotpInvalid(detail) => format!("动态验证码不正确：{detail}"),
            AppError::StorageUnavailable(detail) => format!("存储后端暂时不可用：{detail}"),
            AppError::Internal(detail) => {
                if detail.is_empty() {
                    "服务端错误".into()
                } else {
                    format!("服务端错误：{detail}")
                }
            }
            AppError::Repo(err) => format!("数据访问失败：{err}"),
            AppError::Storage(err) => err.to_string(),
            AppError::Mail(err) => format!("邮件发送失败：{err}"),
            AppError::Crypto(err) => format!("加密操作失败：{err}"),
        }
    }

    /// 额外字段：暂停/封禁要带上原因、申诉邮箱与申诉地址，客户端据此给不同提示。
    pub fn extra(&self) -> Option<serde_json::Value> {
        match self {
            AppError::AccountSuspended {
                reason,
                suspended_at,
            } => Some(serde_json::json!({
                "suspended_at": suspended_at,
                "reason": reason,
            })),
            AppError::AccountBanned { reason } => Some(serde_json::json!({
                "reason": reason,
            })),
            AppError::AccountLocked { locked_until } => Some(serde_json::json!({
                "locked_until": locked_until,
            })),
            AppError::QuotaExceeded { detail } => Some(serde_json::json!({ "detail": detail })),
            _ => None,
        }
    }

    /// 是否要带 `Retry-After` 头。
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            AppError::RateLimited { retry_after, .. } | AppError::QueueFull { retry_after } => {
                Some(*retry_after)
            }
            _ => None,
        }
    }

    /// 是否是「服务端自己的问题」——这类错误要打日志，客户端只需要知道失败了。
    pub fn is_server_fault(&self) -> bool {
        self.status() >= 500
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for AppError {}

impl From<RepoError> for AppError {
    fn from(value: RepoError) -> Self {
        Self::Repo(value)
    }
}

impl From<StorageError> for AppError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

impl From<MailError> for AppError {
    fn from(value: MailError) -> Self {
        Self::Mail(value)
    }
}

impl From<CryptoError> for AppError {
    fn from(value: CryptoError) -> Self {
        Self::Crypto(value)
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Internal(format!("IO 错误：{value}"))
    }
}

/// 字节数的人话表示，错误信息里用。
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_code_match_contract() {
        // 契约 1.2 的错误码表逐条核对
        assert_eq!(AppError::AccountSuspended { reason: None, suspended_at: None }.status(), 403);
        assert_eq!(AppError::AccountSuspended { reason: None, suspended_at: None }.code(), "ACCOUNT_SUSPENDED");
        assert_eq!(AppError::AccountBanned { reason: None }.code(), "ACCOUNT_BANNED");
        assert_eq!(AppError::AccountPending.code(), "ACCOUNT_PENDING");
        assert_eq!(AppError::IpBlocked.status(), 403);
        assert_eq!(AppError::TokenRevoked.status(), 401);
        assert_eq!(AppError::QuotaExceeded { detail: String::new() }.status(), 413);
        assert_eq!(AppError::RateLimited { retry_after: 30, detail: String::new() }.status(), 429);
        assert_eq!(AppError::RateLimited { retry_after: 30, detail: String::new() }.retry_after(), Some(30));
        assert_eq!(AppError::TermsRequired.status(), 451);
        assert_eq!(AppError::QueueFull { retry_after: 5 }.code(), "QUEUE_FULL");
    }

    #[test]
    fn suspended_error_carries_extra_fields() {
        let error = AppError::AccountSuspended {
            reason: Some("上传违规内容，违反服务条款第 3 条".into()),
            suspended_at: Some("2026-10-14T15:30:00Z".into()),
        };
        let extra = error.extra().unwrap();
        assert_eq!(extra["reason"], "上传违规内容，违反服务条款第 3 条");
        assert_eq!(extra["suspended_at"], "2026-10-14T15:30:00Z");
        // 文案要和界面一致，不能只说「登录失败」
        assert!(error.message().contains("暂停"));
    }

    #[test]
    fn file_too_large_message_is_readable() {
        let error = AppError::FileTooLarge {
            size: 30 * 1024 * 1024,
            limit: 20 * 1024 * 1024,
        };
        let message = error.message();
        assert!(message.contains("30.0 MB"), "{message}");
        assert!(message.contains("20.0 MB"), "{message}");
    }

    #[test]
    fn server_faults_are_marked() {
        assert!(AppError::Internal("x".into()).is_server_fault());
        assert!(!AppError::BadRequest("x".into()).is_server_fault());
    }

    /// 「还值不值得重试」只对邮件类有讲究：收件服务器拒收与认证失败是一次性
    /// 结论，别的（网络超时、TLS、内部错误）都还值得再来一次
    #[test]
    fn only_mail_rejections_are_permanent() {
        assert!(AppError::Mail(MailError::Rejected("550 user unknown".into())).is_permanent());
        assert!(AppError::Mail(MailError::Auth("535 auth failed".into())).is_permanent());
        assert!(!AppError::Mail(MailError::Transport("timed out".into())).is_permanent());
        assert!(!AppError::Mail(MailError::NotConfigured("没配".into())).is_permanent());
        assert!(!AppError::Internal("别的错".into()).is_permanent());
    }
}
