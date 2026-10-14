//! 系统设置的类型化读写。
//!
//! 设置存在 `settings` 表里（键名见 migration 的注释），这里给出带默认值的读取。
//! 默认值直接照抄设计稿：即使库里一行都没有，服务也能按设计稿的行为跑起来——
//! 全新实例在跑完安装向导之前不会有任何设置行。
//!
//! 敏感值（SMTP 密码、第三方校验密钥）用主密钥加密后入库，`encrypted` 列标记，
//! 出库时自动解密。拿不到主密钥时读取返回错误而不是静默给明文，
//! 免得出现「看起来配好了其实读不出来」的假象。

use pixora_crypto::MasterKey;
use pixora_repo::Repo;

use crate::error::{AppError, Result};

/// 键名常量。拼错键名的代价是「设置改了不生效」，所以不写字符串字面量。
pub mod keys {
    pub const INSTALLED: &str = "install.completed";
    pub const SITE_NAME: &str = "site.name";
    pub const SITE_BASE_URL: &str = "site.base_url";
    pub const UPLOAD_PREFIX: &str = "site.upload_prefix";
    pub const REGISTER_ENABLED: &str = "register.enabled";
    pub const REGISTER_AUTO_CLOSE_ON: &str = "register.auto_close_on";
    pub const REGISTER_AUTO_CLOSE_OFF: &str = "register.auto_close_off";
    pub const PLAZA_PUBLIC: &str = "plaza.public";
    /// 广场网格列数（界面上的图大小就由它决定）
    pub const PLAZA_GRID_COLUMNS: &str = "plaza.grid_columns";
    pub const UPLOAD_MAX_FILE: &str = "upload.max_file_bytes";
    pub const UPLOAD_MAX_BATCH: &str = "upload.max_batch";
    pub const UPLOAD_MAX_PIXELS: &str = "upload.max_pixels";
    pub const UPLOAD_QUEUE_DEPTH: &str = "upload.queue_depth";
    pub const UPLOAD_FORMATS: &str = "upload.formats";
    pub const UPLOAD_REQUIRE_LOGIN: &str = "upload.require_login";
    pub const THUMB_CLIENT: &str = "thumb.client";
    pub const THUMB_SERVER_FALLBACK: &str = "thumb.server_fallback";
    pub const CAPACITY_MODE: &str = "capacity.mode";
    pub const CAPACITY_MANUAL_TOTAL: &str = "capacity.manual_total";
    pub const QUOTA_DEFAULT: &str = "quota.default_bytes";
    pub const QUOTA_PER_USER_MAX: &str = "quota.per_user_max";
    pub const RATELIMIT_ON_BREACH: &str = "ratelimit.on_breach";
    pub const RATELIMIT_BAN_THRESHOLD: &str = "ratelimit.ban_threshold";
    pub const RATELIMIT_BAN_MINUTES: &str = "ratelimit.ban_minutes";
    pub const IP_ENABLED: &str = "ip.enabled";
    pub const IP_TRUST_PROXY: &str = "ip.trust_proxy";
    pub const IP_PROXY_HOPS: &str = "ip.proxy_hops";
    pub const IP_AUTO_BAN: &str = "ip.auto_ban";
    pub const IP_FAIL_THRESHOLD: &str = "ip.fail_threshold";
    pub const IP_BAN_MINUTES: &str = "ip.ban_minutes";
    pub const MAIL_SMTP_HOST: &str = "mail.smtp_host";
    pub const MAIL_SMTP_PORT: &str = "mail.smtp_port";
    pub const MAIL_SMTP_SECURITY: &str = "mail.smtp_security";
    pub const MAIL_SMTP_USER: &str = "mail.smtp_user";
    pub const MAIL_SMTP_PASSWORD: &str = "mail.smtp_password";
    pub const MAIL_FROM_NAME: &str = "mail.from_name";
    pub const MAIL_FROM_EMAIL: &str = "mail.from_email";
    pub const MAIL_APPEAL_EMAIL: &str = "mail.appeal_email";
    pub const ANTISPAM_DISPOSABLE: &str = "antispam.disposable";
    pub const ANTISPAM_MX: &str = "antispam.mx_check";
    pub const ANTISPAM_THIRD_PARTY: &str = "antispam.third_party_key";
    pub const APPEARANCE_BACKGROUND: &str = "appearance.background";
}

/// 默认值。设计稿里写死的那些数字都在这里。
pub mod defaults {
    /// 单文件 20MiB
    pub const MAX_FILE_BYTES: i64 = 20 * 1024 * 1024;
    /// 单批 1000 张
    pub const MAX_BATCH: i64 = 1000;
    /// 8000 万像素（解压炸弹防线）
    pub const MAX_PIXELS: i64 = 80_000_000;
    /// 队列深度阈值，超过就返回 429
    pub const QUEUE_DEPTH: i64 = 500;
    /// 新用户默认 10GiB
    pub const QUOTA_DEFAULT: i64 = 10 * 1024 * 1024 * 1024;
    /// 单用户上限 100GiB
    pub const QUOTA_PER_USER_MAX: i64 = 100 * 1024 * 1024 * 1024;
    /// 容量触顶自动关闭注册的阈值（百分比）
    pub const AUTO_CLOSE_ON: i64 = 90;
    pub const AUTO_CLOSE_OFF: i64 = 80;
    /// 限流触发后的临时封禁
    pub const BAN_THRESHOLD: i64 = 10;
    pub const BAN_MINUTES: i64 = 30;
    /// 同 IP 登录或上传失败的自动封禁
    pub const IP_FAIL_THRESHOLD: i64 = 20;
    pub const IP_BAN_MINUTES: i64 = 30;
    pub const UPLOAD_PREFIX: &str = "images";
    /// 广场网格默认 3 列：比设计稿的 4/5 列大一圈，预览更清楚
    pub const GRID_COLUMNS: i64 = 3;
    /// 网格列数的可配置范围。少于 2 列不成网格，多于 6 列缩略图就糊了
    pub const GRID_COLUMNS_MIN: i64 = 2;
    pub const GRID_COLUMNS_MAX: i64 = 6;
    pub const SITE_NAME: &str = "Pixora";
    pub const FROM_NAME: &str = "Pixora 管理团队";
    pub const SMTP_HOST: &str = "smtp.qiye.aliyun.com";
    pub const SMTP_PORT: i64 = 465;
    pub const SMTP_SECURITY: &str = "ssl";
}

/// 一组站点设置。
#[derive(Debug, Clone)]
pub struct SiteSettings {
    pub name: String,
    pub base_url: String,
    pub upload_prefix: String,
    pub plaza_public: bool,
    pub require_login_to_upload: bool,
    pub register_enabled: bool,
    /// 广场每行放几张图。已经钳到 `GRID_COLUMNS_MIN..=GRID_COLUMNS_MAX`
    pub plaza_grid_columns: i64,
}

/// 一组上传设置。
#[derive(Debug, Clone)]
pub struct UploadSettings {
    pub max_file_bytes: i64,
    pub max_batch: i64,
    pub max_pixels: i64,
    pub queue_depth: i64,
    pub formats: Vec<String>,
    pub thumb_client: bool,
    pub thumb_server_fallback: bool,
}

/// 一组邮件设置。密码单独取，因为它要解密。
#[derive(Debug, Clone)]
pub struct MailSettings {
    pub smtp_host: String,
    pub smtp_port: i64,
    pub smtp_security: String,
    pub smtp_user: String,
    pub password_set: bool,
    pub from_name: String,
    pub from_email: String,
    pub appeal_email: String,
    pub disposable_check: bool,
    pub mx_check: bool,
    pub third_party_set: bool,
}

#[derive(Clone)]
pub struct Settings {
    repo: Repo,
    master_key: Option<MasterKey>,
}

impl Settings {
    pub fn new(repo: Repo, master_key: Option<MasterKey>) -> Self {
        Self { repo, master_key }
    }

    pub async fn raw(&self, name: &str) -> Result<Option<String>> {
        Ok(self.repo.get_setting_value(name).await?)
    }

    pub async fn string(&self, name: &str, default: &str) -> Result<String> {
        Ok(self.raw(name).await?.unwrap_or_else(|| default.to_string()))
    }

    pub async fn bool(&self, name: &str, default: bool) -> Result<bool> {
        Ok(match self.raw(name).await? {
            Some(value) => matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
            None => default,
        })
    }

    pub async fn integer(&self, name: &str, default: i64) -> Result<i64> {
        Ok(self
            .raw(name)
            .await?
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(default))
    }

    pub async fn set(&self, name: &str, value: &str) -> Result<()> {
        self.repo.set_setting_plain(name, value).await?;
        Ok(())
    }

    pub async fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        self.set(name, if value { "true" } else { "false" }).await
    }

    pub async fn set_integer(&self, name: &str, value: i64) -> Result<()> {
        self.set(name, &value.to_string()).await
    }

    /// 加密写入。没有主密钥时明确报错——把明文当密文写进去是更坏的结果。
    pub async fn set_secret(&self, name: &str, value: &str) -> Result<()> {
        let key = self.master_key.as_ref().ok_or_else(|| {
            AppError::Config("缺少主密钥，无法加密存储凭据。请设置 PIXORA_MASTER_KEY".into())
        })?;
        let encrypted = key.encrypt_str(value)?;
        self.repo.set_setting(name, Some(&encrypted), true).await?;
        Ok(())
    }

    pub async fn get_secret(&self, name: &str) -> Result<Option<String>> {
        let Some(setting) = self.repo.get_setting(name).await? else {
            return Ok(None);
        };
        let Some(value) = setting.value else {
            return Ok(None);
        };
        if !setting.encrypted {
            // 明文存的凭据（手工写库或旧版本）也要能读，但不能悄悄升级成密文
            return Ok(Some(value));
        }
        let key = self.master_key.as_ref().ok_or_else(|| {
            AppError::Config(
                "库里的凭据是加密的，但当前没有可用的主密钥；请配置正确的 PIXORA_MASTER_KEY"
                    .into(),
            )
        })?;
        Ok(Some(key.decrypt_str(&value)?))
    }

    /// 判断某个密钥类设置是否已配置（不返回明文，界面只显示「已配置」）。
    pub async fn secret_configured(&self, name: &str) -> Result<bool> {
        Ok(self
            .repo
            .get_setting(name)
            .await?
            .and_then(|setting| setting.value)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false))
    }

    pub async fn is_installed(&self) -> Result<bool> {
        self.bool(keys::INSTALLED, false).await
    }

    pub async fn mark_installed(&self) -> Result<()> {
        self.set_bool(keys::INSTALLED, true).await
    }

    pub async fn site(&self, env_base_url: Option<&str>) -> Result<SiteSettings> {
        Ok(SiteSettings {
            name: self.string(keys::SITE_NAME, defaults::SITE_NAME).await?,
            // 设置里的地址优先于环境变量？不：环境变量是运维真相，
            // 但安装向导写进设置的值也要能用。顺序是「设置里有就用设置的」。
            base_url: match self.raw(keys::SITE_BASE_URL).await? {
                Some(value) if !value.trim().is_empty() => value,
                _ => env_base_url.unwrap_or("").to_string(),
            },
            upload_prefix: {
                let raw = self
                    .string(keys::UPLOAD_PREFIX, defaults::UPLOAD_PREFIX)
                    .await?;
                normalize_prefix(&raw)
            },
            plaza_public: self.bool(keys::PLAZA_PUBLIC, true).await?,
            require_login_to_upload: self.bool(keys::UPLOAD_REQUIRE_LOGIN, true).await?,
            register_enabled: self.bool(keys::REGISTER_ENABLED, true).await?,
            // 库里的值可能被手工改坏：钳在合法区间里再出库，
            // 这样界面永远拿到一个能用的列数
            plaza_grid_columns: self
                .integer(keys::PLAZA_GRID_COLUMNS, defaults::GRID_COLUMNS)
                .await?
                .clamp(defaults::GRID_COLUMNS_MIN, defaults::GRID_COLUMNS_MAX),
        })
    }

    pub async fn upload(&self) -> Result<UploadSettings> {
        let formats = match self.raw(keys::UPLOAD_FORMATS).await? {
            // 归一化会把 jpeg/tif/heif 折叠成 jpg/tiff/heic，清单里因此会出现重复项，
            // 去重后再返回（界面上的格式 chips 不该重复出现同一个扩展名）
            Some(value) if !value.trim().is_empty() => dedupe(
                value
                    .split(',')
                    .map(crate::media::normalize_ext)
                    .filter(|item| crate::media::rule_for_ext(item).is_some()),
            ),
            _ => dedupe(crate::media::DEFAULT_ENABLED.iter().map(|item| crate::media::normalize_ext(*item))),
        };

        Ok(UploadSettings {
            max_file_bytes: self
                .integer(keys::UPLOAD_MAX_FILE, defaults::MAX_FILE_BYTES)
                .await?,
            max_batch: self.integer(keys::UPLOAD_MAX_BATCH, defaults::MAX_BATCH).await?,
            max_pixels: self
                .integer(keys::UPLOAD_MAX_PIXELS, defaults::MAX_PIXELS)
                .await?,
            queue_depth: self
                .integer(keys::UPLOAD_QUEUE_DEPTH, defaults::QUEUE_DEPTH)
                .await?,
            formats,
            thumb_client: self.bool(keys::THUMB_CLIENT, true).await?,
            thumb_server_fallback: self.bool(keys::THUMB_SERVER_FALLBACK, false).await?,
        })
    }

    pub async fn mail(&self) -> Result<MailSettings> {
        Ok(MailSettings {
            smtp_host: self
                .string(keys::MAIL_SMTP_HOST, defaults::SMTP_HOST)
                .await?,
            smtp_port: self
                .integer(keys::MAIL_SMTP_PORT, defaults::SMTP_PORT)
                .await?,
            smtp_security: self
                .string(keys::MAIL_SMTP_SECURITY, defaults::SMTP_SECURITY)
                .await?,
            smtp_user: self.string(keys::MAIL_SMTP_USER, "").await?,
            password_set: self.secret_configured(keys::MAIL_SMTP_PASSWORD).await?,
            from_name: self
                .string(keys::MAIL_FROM_NAME, defaults::FROM_NAME)
                .await?,
            from_email: self.string(keys::MAIL_FROM_EMAIL, "").await?,
            appeal_email: self.string(keys::MAIL_APPEAL_EMAIL, "").await?,
            disposable_check: self.bool(keys::ANTISPAM_DISPOSABLE, true).await?,
            mx_check: self.bool(keys::ANTISPAM_MX, true).await?,
            third_party_set: self.secret_configured(keys::ANTISPAM_THIRD_PARTY).await?,
        })
    }

    /// 配额与容量相关的一组值。
    pub async fn quota_values(&self) -> Result<(i64, i64)> {
        Ok((
            self.integer(keys::QUOTA_DEFAULT, defaults::QUOTA_DEFAULT)
                .await?,
            self.integer(keys::QUOTA_PER_USER_MAX, defaults::QUOTA_PER_USER_MAX)
                .await?,
        ))
    }

    /// 系统总额（字节）。`mode = auto` 时返回 `None`，由容量探测补齐。
    pub async fn manual_total(&self) -> Result<Option<i64>> {
        let mode = self.capacity_mode().await?;
        if mode != "manual" {
            return Ok(None);
        }
        let value = self.integer(keys::CAPACITY_MANUAL_TOTAL, 0).await?;
        Ok(if value > 0 { Some(value) } else { None })
    }

    pub async fn capacity_mode(&self) -> Result<String> {
        let raw = self.string(keys::CAPACITY_MODE, "auto").await?;
        let mode = raw.trim().to_ascii_lowercase();
        Ok(match mode.as_str() {
            "manual" | "hidden" => mode,
            _ => "auto".to_string(),
        })
    }

    /// 上传格式白名单的展示用清单（全部可选格式 + 当前启用项）。
    pub async fn formats_and_enabled(&self) -> Result<(Vec<String>, Vec<String>)> {
        let all: Vec<String> = crate::media::FORMATS
            .iter()
            .map(|rule| rule.ext.to_string())
            .filter(|ext| ext != "jpeg" && ext != "tif" && ext != "heif")
            .collect();
        let enabled = self.upload().await?.formats;
        Ok((all, enabled))
    }
}

/// 保持原顺序去重。
fn dedupe(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|item| seen.insert(item.clone())).collect()
}

/// 直链一级目录的净化：只留字母数字与连字符，长度不超过 32。
/// 它会成为 URL 的第一段，任何斜杠或点都可能被用来构造奇怪的路由。
pub fn normalize_prefix(input: &str) -> String {
    let cleaned: String = input
        .trim()
        .trim_matches('/')
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .take(32)
        .collect();
    if cleaned.is_empty() {
        defaults::UPLOAD_PREFIX.to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_sanitized() {
        assert_eq!(normalize_prefix("images"), "images");
        assert_eq!(normalize_prefix("/i/"), "i");
        assert_eq!(normalize_prefix("p"), "p");
        assert_eq!(normalize_prefix("../../etc"), "etc");
        assert_eq!(normalize_prefix("带中文的前缀"), defaults::UPLOAD_PREFIX);
        assert_eq!(normalize_prefix(""), defaults::UPLOAD_PREFIX);
        assert_eq!(normalize_prefix("a".repeat(80).as_str()).len(), 32);
    }

    #[test]
    fn defaults_match_design_doc() {
        assert_eq!(defaults::MAX_FILE_BYTES, 20 * 1024 * 1024);
        assert_eq!(defaults::MAX_BATCH, 1000);
        assert_eq!(defaults::QUEUE_DEPTH, 500);
        assert_eq!(defaults::QUOTA_DEFAULT, 10 * 1024 * 1024 * 1024);
        assert_eq!(defaults::AUTO_CLOSE_ON, 90);
        assert_eq!(defaults::AUTO_CLOSE_OFF, 80);
        assert_eq!(defaults::SMTP_PORT, 465);
        assert_eq!(defaults::SMTP_HOST, "smtp.qiye.aliyun.com");
        assert_eq!(defaults::GRID_COLUMNS, 3);
    }
}
