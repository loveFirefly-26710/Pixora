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
    /// 背景图上传后存进 `storage_objects` 的键名
    pub const BACKGROUND_ASSET_KEY: &str = "config:background";
    /// `appearance.background` 取这个值表示「用上传的那张图」
    pub const BACKGROUND_ASSET_MARKER: &str = "asset";
    /// 背景图的公开地址（服务器自己提供，与站点同源）
    pub const BACKGROUND_PUBLIC_PATH: &str = "/api/v1/site/background";
    /// 站点默认头像上传后存进 `storage_objects` 的键名
    pub const AVATAR_ASSET_KEY: &str = "config:default_avatar";
    /// `site.default_avatar` 取这个值表示「用上传的那张图」
    pub const AVATAR_ASSET_MARKER: &str = "asset";
    /// 默认头像的公开地址（服务器自己提供，与站点同源）
    pub const AVATAR_PUBLIC_PATH: &str = "/api/v1/site/avatar";
    pub const INSTALLED: &str = "install.completed";
    /// 安装时创建的那个管理员账号的 UID。它是安装账号，别的管理员动不了它
    pub const INSTALL_ADMIN_UID: &str = "install.admin_uid";
    pub const SITE_NAME: &str = "site.name";
    pub const SITE_BASE_URL: &str = "site.base_url";
    pub const UPLOAD_PREFIX: &str = "site.upload_prefix";
    pub const REGISTER_ENABLED: &str = "register.enabled";
    /// 允许注册的邮箱域名白名单，逗号分隔；空 = 不限域名。子域自动算命中
    pub const REGISTER_EMAIL_DOMAINS: &str = "register.email_domains";
    pub const REGISTER_AUTO_CLOSE_ON: &str = "register.auto_close_on";
    pub const REGISTER_AUTO_CLOSE_OFF: &str = "register.auto_close_off";
    pub const PLAZA_PUBLIC: &str = "plaza.public";
    /// 广场网格列数（界面上的图大小就由它决定）
    pub const PLAZA_GRID_COLUMNS: &str = "plaza.grid_columns";
    /// 广场自动漫游间隔（秒）。0 = 关闭
    pub const PLAZA_AUTO_REFRESH_SECONDS: &str = "plaza.auto_refresh_seconds";
    pub const UPLOAD_MAX_FILE: &str = "upload.max_file_bytes";
    pub const UPLOAD_MAX_BATCH: &str = "upload.max_batch";
    pub const UPLOAD_MAX_PIXELS: &str = "upload.max_pixels";
    /// 长宽上限（像素）。0 = 不限；与 `upload.max_pixels` 一起判，三者都过才收
    pub const UPLOAD_MAX_WIDTH: &str = "upload.max_width";
    pub const UPLOAD_MAX_HEIGHT: &str = "upload.max_height";
    pub const UPLOAD_QUEUE_DEPTH: &str = "upload.queue_depth";
    pub const UPLOAD_FORMATS: &str = "upload.formats";
    pub const UPLOAD_REQUIRE_LOGIN: &str = "upload.require_login";
    pub const THUMB_CLIENT: &str = "thumb.client";
    pub const THUMB_SERVER_FALLBACK: &str = "thumb.server_fallback";
    /// 缩略图重算的游标：处理到哪个 image id 了（0 = 从头来）
    pub const THUMB_REGEN_CURSOR: &str = "thumb.regen_cursor";
    /// 这一轮重算的总张数与已完成张数（界面上看进度用）
    pub const THUMB_REGEN_TOTAL: &str = "thumb.regen_total";
    pub const THUMB_REGEN_DONE: &str = "thumb.regen_done";
    pub const CAPACITY_MODE: &str = "capacity.mode";
    pub const CAPACITY_MANUAL_TOTAL: &str = "capacity.manual_total";
    pub const QUOTA_DEFAULT: &str = "quota.default_bytes";
    pub const QUOTA_PER_USER_MAX: &str = "quota.per_user_max";
    /// 未登录访客的下载限额（带 `?download=1` 的直链请求）。登录用户的那两格
    /// 在限流策略里（`rate_limit_policies.download_*`），游客没有账号，只能放这儿。
    /// `0` = 不限。
    pub const DOWNLOAD_ANON_PER_MINUTE: &str = "download.anon_per_minute";
    pub const DOWNLOAD_ANON_PER_DAY: &str = "download.anon_per_day";
    /// 第三方登录的开关与客户端凭据。`client_secret` 加密存储，界面只显示
    /// 「已配置」。每家一套：`login.github.enabled` / `.client_id` / `.client_secret`
    pub const LOGIN_PHONE_ENABLED: &str = "login.phone.enabled";
    /// 短信服务（阿里云短信）：AccessKey、签名与模板编号
    pub const SMS_ENABLED: &str = "sms.enabled";
    pub const SMS_ACCESS_KEY_ID: &str = "sms.access_key_id";
    pub const SMS_ACCESS_KEY_SECRET: &str = "sms.access_key_secret";
    pub const SMS_SIGN_NAME: &str = "sms.sign_name";
    pub const SMS_TEMPLATE_CODE: &str = "sms.template_code";
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
    /// 站点默认头像：没设过头像的账号（含新注册的）用它。外链或上传的那张
    pub const SITE_DEFAULT_AVATAR: &str = "site.default_avatar";
    /// 回收站保留天数。0 = 关闭回收站（删除即彻底删）
    pub const RECYCLE_RETENTION_DAYS: &str = "recycle.retention_days";
    /// 会话闲置超时（分钟）。0 = 不按闲置判
    pub const SESSION_IDLE_MINUTES: &str = "session.idle_minutes";
    /// 普通会话的最长寿命（天）。0 = 不限
    pub const SESSION_ABSOLUTE_DAYS: &str = "session.absolute_days";
    /// 「记住我」签发的会话活多久（天）
    pub const SESSION_REMEMBER_DAYS: &str = "session.remember_days";
    /// 防盗链：直链要不要校验来源
    pub const HOTLINK_ENABLED: &str = "security.hotlink_enabled";
    /// 防盗链：允许空 Referer（直接打开链接、部分客户端不带）
    pub const HOTLINK_ALLOW_EMPTY: &str = "security.hotlink_allow_empty";
    /// 防盗链：允许的来源主机白名单，逗号分隔；空 = 只放本站
    pub const HOTLINK_DOMAINS: &str = "security.hotlink_domains";
    /// 内容审核总开关
    pub const MODERATION_ENABLED: &str = "moderation.enabled";
    /// 审核服务商：aliyun | tencent | baidu | nsfwjs
    pub const MODERATION_PROVIDER: &str = "moderation.provider";
    /// 违规分数阈值（0-100，达到即拦）
    pub const MODERATION_THRESHOLD: &str = "moderation.threshold";
    /// 自建服务返回结构里哪个字段是分数（也认 nsfwjs 的 predictions 数组）
    pub const MODERATION_SCORE_FIELD: &str = "moderation.score_field";
    /// 自建服务的接口地址
    pub const MODERATION_ENDPOINT: &str = "moderation.endpoint";
    pub const MODERATION_REGION: &str = "moderation.region";
    pub const MODERATION_APP_ID: &str = "moderation.app_id";
    /// 密钥类：阿里云 AccessKeyId / 腾讯云 SecretId / 百度 API Key
    pub const MODERATION_ACCESS_KEY: &str = "moderation.access_key";
    /// 密钥类：阿里云 AccessKeySecret / 腾讯云 SecretKey / 百度 Secret Key
    pub const MODERATION_SECRET_KEY: &str = "moderation.secret_key";
    /// 自建服务的 Bearer Token（可选）
    pub const MODERATION_TOKEN: &str = "moderation.token";
    /// 普通用户是否过审
    pub const MODERATION_USER_ENABLED: &str = "moderation.user_enabled";
    /// 管理员是否过审（默认不过：管理员改配置时不该被自己的图挡在门外）
    pub const MODERATION_ADMIN_ENABLED: &str = "moderation.admin_enabled";
    /// 累计多少次违规就临时暂停账号。0 = 不自动封
    pub const MODERATION_BAN_AFTER: &str = "moderation.ban_after";
    /// 自动暂停多久（分钟）
    pub const MODERATION_BAN_MINUTES: &str = "moderation.ban_minutes";
    /// 违规是否发邮件通知管理员
    pub const MODERATION_NOTIFY: &str = "moderation.notify_admin";
    /// 审核服务不可用（超时、5xx）时是否放行。默认放行：审核是增强，
    /// 让它变成单点故障会直接掐掉整站上传
    pub const MODERATION_FAIL_OPEN: &str = "moderation.fail_open";
}

/// 默认值。设计稿里写死的那些数字都在这里。
pub mod defaults {
    /// 单文件 20MiB
    pub const MAX_FILE_BYTES: i64 = 20 * 1024 * 1024;
    /// 单批 1000 张
    pub const MAX_BATCH: i64 = 1000;
    /// 8000 万像素（解压炸弹防线）
    pub const MAX_PIXELS: i64 = 80_000_000;
    /// 长宽上限默认不限：像素数上限已经拦住了「解压炸弹」，
    /// 长宽是给「不想让人传 20000×200 这种怪图」的场景留的
    pub const MAX_WIDTH: i64 = 0;
    pub const MAX_HEIGHT: i64 = 0;
    /// 未登录访客的下载：每分钟 60 次、每天 1000 次。按 IP 计数，
    /// 这个量级够正常看图/存图，挡不住的是成规模的抓取（那种要靠 IP 规则）。
    pub const DOWNLOAD_ANON_PER_MINUTE: i64 = 60;
    pub const DOWNLOAD_ANON_PER_DAY: i64 = 1000;
    /// 队列深度阈值，超过就返回 429
    pub const QUEUE_DEPTH: i64 = 500;
    /// 会话闲置超时：两小时没动就要求重新登录（与云控制台的默认口径一致）
    pub const SESSION_IDLE_MINUTES: i64 = 120;
    /// 普通会话最长 30 天：一直挂着不动的会话总得有个头
    pub const SESSION_ABSOLUTE_DAYS: i64 = 30;
    /// 「记住我」的会话活 30 天
    pub const SESSION_REMEMBER_DAYS: i64 = 30;
    /// 会话项的可配置范围。上限给一年，再长不如关掉这一项
    pub const SESSION_MINUTES_MAX: i64 = 525_600;
    pub const SESSION_DAYS_MAX: i64 = 365;
    /// 审核阈值默认 70 分：主流云厂商的「疑似」档就在这一带
    pub const MODERATION_THRESHOLD: i64 = 70;
    pub const MODERATION_BAN_AFTER: i64 = 3;
    pub const MODERATION_BAN_MINUTES: i64 = 1440;
    pub const MODERATION_SCORE_FIELD: &str = "nsfw";
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
    /// 回收站默认保留 30 天。够把「手滑删了」捞回来，又不至于让磁盘长期背着。
    pub const RECYCLE_RETENTION_DAYS: i64 = 30;
    /// 保留天数的上限：365 天（一年）。再长不如关掉回收站。
    pub const RECYCLE_RETENTION_MAX: i64 = 365;
    /// 广场网格默认 3 列：比设计稿的 4/5 列大一圈，预览更清楚
    pub const GRID_COLUMNS: i64 = 3;
    /// 网格列数的可配置范围。少于 2 列不成网格，多于 6 列缩略图就糊了
    pub const GRID_COLUMNS_MIN: i64 = 2;
    pub const GRID_COLUMNS_MAX: i64 = 6;
    /// 广场自动漫游默认 5 秒：打开页面就能看到下一批图
    pub const PLAZA_AUTO_REFRESH_SECONDS: i64 = 5;
    /// 自动刷新间隔的可配置范围。0 = 关闭；太短会打断浏览，太长就不叫漫游了
    pub const PLAZA_AUTO_REFRESH_MIN: i64 = 5;
    pub const PLAZA_AUTO_REFRESH_MAX: i64 = 3600;
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
    /// 广场自动漫游间隔（秒）。0 = 关闭，其余钳在 `PLAZA_AUTO_REFRESH_MIN..=MAX`
    pub plaza_auto_refresh_seconds: i64,
    /// 允许注册的邮箱域名（白名单，空 = 不限）
    pub email_domains: RegisterDomains,
}

/// 允许注册的邮箱域名白名单。
///
/// 空清单就是「不限域名」，保持原样；填了之后只收清单里的域名，
/// 别的邮箱在发验证码那一步就被挡回去。子域算命中：清单里写
/// `example.com`，`mail.example.com` 也能注册——填的人想的是「这个域」，
/// 不是某一个主机名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisterDomains {
    domains: Vec<String>,
}

impl RegisterDomains {
    /// 从逗号（或空白）分隔的文本解析。允许写成 `@example.com` 的样子，
    /// 顺手小写、去重、丢掉明显不是域名的项（没有点的、带空格的）。
    pub fn parse(raw: Option<&str>) -> Self {
        let mut domains: Vec<String> = Vec::new();
        for piece in raw.unwrap_or("").split([',', ';', '\n', ' ']) {
            let cleaned = piece
                .trim()
                .trim_start_matches('@')
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if cleaned.is_empty() || !cleaned.contains('.') {
                continue;
            }
            if !domains.contains(&cleaned) {
                domains.push(cleaned);
            }
        }
        Self { domains }
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// 这个邮箱的域名在清单里吗？清单为空时一律放行。
    pub fn allows(&self, email: &str) -> bool {
        if self.domains.is_empty() {
            return true;
        }
        let Some(domain) = email.rsplit('@').next() else {
            return false;
        };
        let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        self.domains
            .iter()
            .any(|allowed| domain == *allowed || domain.ends_with(&format!(".{allowed}")))
    }

    /// 回写成设置值（逗号分隔）。
    pub fn to_value(&self) -> String {
        self.domains.join(",")
    }
}

/// 一组上传设置。
#[derive(Debug, Clone)]
pub struct UploadSettings {
    pub max_file_bytes: i64,
    pub max_batch: i64,
    pub max_pixels: i64,
    /// 长宽上限（像素），0 = 不限
    pub max_width: i64,
    pub max_height: i64,
    pub queue_depth: i64,
    pub formats: Vec<String>,
    pub thumb_client: bool,
    pub thumb_server_fallback: bool,
}

/**
 * 未登录访客的下载限额。
 *
 * 登录用户走限流策略（可按用户覆盖），游客没有账号可挂，只能读系统设置。
 * `0` 表示不限。
 */
#[derive(Debug, Clone, Copy)]
pub struct DownloadSettings {
    pub anon_per_minute: i64,
    pub anon_per_day: i64,
}

/// 会话有效期。
#[derive(Debug, Clone, Copy)]
pub struct SessionSettings {
    pub idle_minutes: i64,
    pub absolute_days: i64,
    pub remember_days: i64,
}

impl SessionSettings {
    /// 仓储层要的策略形状。
    pub fn policy(&self) -> pixora_repo::model::SessionPolicy {
        pixora_repo::model::SessionPolicy {
            idle_minutes: self.idle_minutes,
            absolute_days: self.absolute_days,
        }
    }
}

/// 防盗链设置。
#[derive(Debug, Clone)]
pub struct HotlinkSettings {
    pub enabled: bool,
    pub allow_empty: bool,
    /// 允许的来源主机（小写，不含协议与路径）。空表示只放本站
    pub domains: Vec<String>,
}

/// 内容审核设置。密钥单独存放在 settings 里，这里带的是解密后的明文——
/// 它只在服务端内部流转，绝不出接口（接口只回「配没配」）。
#[derive(Debug, Clone)]
pub struct ModerationSettings {
    pub enabled: bool,
    pub provider: String,
    pub threshold: i64,
    pub score_field: String,
    pub endpoint: String,
    pub region: String,
    pub app_id: String,
    pub access_key: String,
    pub secret_key: String,
    pub token: String,
    pub user_enabled: bool,
    pub admin_enabled: bool,
    pub ban_after: i64,
    pub ban_minutes: i64,
    pub notify_admin: bool,
    pub fail_open: bool,
}

impl ModerationSettings {
    /// 这个角色要不要过审。
    pub fn applies_to(&self, is_admin: bool) -> bool {
        if !self.enabled {
            return false;
        }
        if is_admin {
            self.admin_enabled
        } else {
            self.user_enabled
        }
    }

    /// 服务商是否已经配齐凭据。缺凭据时审核没法跑，就按「放行 + 记日志」处理。
    pub fn provider_ready(&self) -> bool {
        match self.provider.as_str() {
            "aliyun" | "tencent" | "baidu" => {
                !self.access_key.trim().is_empty() && !self.secret_key.trim().is_empty()
            }
            "nsfwjs" => !self.endpoint.trim().is_empty(),
            _ => false,
        }
    }
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

    /// 用主密钥签一段载荷（OAuth 的 state、注册补全邮箱的令牌）。
    ///
    /// 载荷是明文（别往里塞敏感信息），要的只是「没被改过、不是别人造的」。
    pub fn sign_payload(&self, payload: &str) -> Result<String> {
        let key = self.master_key.as_ref().ok_or_else(|| {
            AppError::Config("缺少主密钥，无法签发登录令牌。请设置 PIXORA_MASTER_KEY".into())
        })?;
        Ok(pixora_crypto::sign::sign(key, payload)?)
    }

    /// 验签并取回载荷。
    pub fn verify_payload(&self, token: &str) -> Result<String> {
        let key = self.master_key.as_ref().ok_or_else(|| {
            AppError::Config("缺少主密钥，无法校验登录令牌。请设置 PIXORA_MASTER_KEY".into())
        })?;
        Ok(pixora_crypto::sign::verify(key, token)?)
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
            plaza_auto_refresh_seconds: normalize_auto_refresh_seconds(
                self.integer(
                    keys::PLAZA_AUTO_REFRESH_SECONDS,
                    defaults::PLAZA_AUTO_REFRESH_SECONDS,
                )
                .await?,
            ),
            email_domains: RegisterDomains::parse(
                self.raw(keys::REGISTER_EMAIL_DOMAINS).await?.as_deref(),
            ),
        })
    }

    /// 访客的下载限额。
    pub async fn download(&self) -> Result<DownloadSettings> {
        Ok(DownloadSettings {
            anon_per_minute: self
                .integer(keys::DOWNLOAD_ANON_PER_MINUTE, defaults::DOWNLOAD_ANON_PER_MINUTE)
                .await?
                .max(0),
            anon_per_day: self
                .integer(keys::DOWNLOAD_ANON_PER_DAY, defaults::DOWNLOAD_ANON_PER_DAY)
                .await?
                .max(0),
        })
    }

    pub async fn upload(&self) -> Result<UploadSettings> {
        let formats = match self.raw(keys::UPLOAD_FORMATS).await? {
            Some(value) if !value.trim().is_empty() => parse_format_list(&value),
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
            max_width: self
                .integer(keys::UPLOAD_MAX_WIDTH, defaults::MAX_WIDTH)
                .await?
                .max(0),
            max_height: self
                .integer(keys::UPLOAD_MAX_HEIGHT, defaults::MAX_HEIGHT)
                .await?
                .max(0),
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

    /// 回收站保留天数。0 表示关闭回收站（删除即彻底删除）。
    pub async fn retention_days(&self) -> Result<i64> {
        let raw = self
            .integer(
                keys::RECYCLE_RETENTION_DAYS,
                defaults::RECYCLE_RETENTION_DAYS,
            )
            .await?;
        Ok(raw.clamp(0, defaults::RECYCLE_RETENTION_MAX))
    }

    /// 会话有效期。三个值都钳在合法区间里：手工改库改坏了也不该让谁都能登录或谁都不能登录。
    ///
    /// 一次查询取齐三行，而不是逐个 `integer()`：这个函数在**每个已登录请求**上
    /// 都会被调用一次（鉴权时要判过期），三次往返换成一次是实打实的收益。
    pub async fn session(&self) -> Result<SessionSettings> {
        let rows = self.repo.list_settings().await?;
        let read = |name: &str, fallback: i64| -> i64 {
            rows.iter()
                .find(|row| row.name == name)
                .and_then(|row| row.value.as_deref())
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(fallback)
        };
        Ok(SessionSettings {
            idle_minutes: read(keys::SESSION_IDLE_MINUTES, defaults::SESSION_IDLE_MINUTES)
                .clamp(0, defaults::SESSION_MINUTES_MAX),
            absolute_days: read(keys::SESSION_ABSOLUTE_DAYS, defaults::SESSION_ABSOLUTE_DAYS)
                .clamp(0, defaults::SESSION_DAYS_MAX),
            remember_days: read(keys::SESSION_REMEMBER_DAYS, defaults::SESSION_REMEMBER_DAYS)
                .clamp(1, defaults::SESSION_DAYS_MAX),
        })
    }

    /// 防盗链。默认关闭：这功能一开，别人的站引用你的图就会挂，
    /// 得由管理员明确决定，不能靠默认值替他决定。
    pub async fn hotlink(&self) -> Result<HotlinkSettings> {
        let raw = self.string(keys::HOTLINK_DOMAINS, "").await?;
        let domains = raw
            .split(',')
            .map(|item| item.trim().trim_start_matches("https://").trim_start_matches("http://"))
            .map(|item| item.split('/').next().unwrap_or("").trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect::<Vec<String>>();
        Ok(HotlinkSettings {
            enabled: self.bool(keys::HOTLINK_ENABLED, false).await?,
            allow_empty: self.bool(keys::HOTLINK_ALLOW_EMPTY, true).await?,
            domains,
        })
    }

    /// 内容审核设置。密钥在这里解密出来，只在服务端内部用。
    pub async fn moderation(&self) -> Result<ModerationSettings> {
        let read_secret = |value: Option<String>| value.unwrap_or_default();
        let access_key = read_secret(self.get_secret(keys::MODERATION_ACCESS_KEY).await?);
        let secret_key = read_secret(self.get_secret(keys::MODERATION_SECRET_KEY).await?);
        let token = read_secret(self.get_secret(keys::MODERATION_TOKEN).await?);

        let provider = self
            .string(keys::MODERATION_PROVIDER, "aliyun")
            .await?
            .trim()
            .to_ascii_lowercase();
        let provider = match provider.as_str() {
            "aliyun" | "tencent" | "baidu" | "nsfwjs" => provider,
            _ => "aliyun".to_string(),
        };

        Ok(ModerationSettings {
            enabled: self.bool(keys::MODERATION_ENABLED, false).await?,
            provider,
            threshold: self
                .integer(keys::MODERATION_THRESHOLD, defaults::MODERATION_THRESHOLD)
                .await?
                .clamp(1, 100),
            score_field: self
                .string(keys::MODERATION_SCORE_FIELD, defaults::MODERATION_SCORE_FIELD)
                .await?,
            endpoint: self.string(keys::MODERATION_ENDPOINT, "").await?,
            region: self.string(keys::MODERATION_REGION, "").await?,
            app_id: self.string(keys::MODERATION_APP_ID, "").await?,
            access_key,
            secret_key,
            token,
            user_enabled: self.bool(keys::MODERATION_USER_ENABLED, true).await?,
            admin_enabled: self.bool(keys::MODERATION_ADMIN_ENABLED, false).await?,
            ban_after: self
                .integer(keys::MODERATION_BAN_AFTER, defaults::MODERATION_BAN_AFTER)
                .await?
                .max(0),
            ban_minutes: self
                .integer(
                    keys::MODERATION_BAN_MINUTES,
                    defaults::MODERATION_BAN_MINUTES,
                )
                .await?
                .max(0),
            notify_admin: self.bool(keys::MODERATION_NOTIFY, true).await?,
            fail_open: self.bool(keys::MODERATION_FAIL_OPEN, true).await?,
        })
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

    /// 背景图的对外地址。
    ///
    /// 两种来源都支持：外部 URL 原样返回；上传的那张用本站地址，
    /// 并带上资产更新时间做版本号——换了图旧缓存立刻失效。
    pub async fn background_public_url(&self) -> Result<Option<String>> {
        let Some(raw) = self.raw(keys::APPEARANCE_BACKGROUND).await? else {
            return Ok(None);
        };
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        if raw != keys::BACKGROUND_ASSET_MARKER {
            return Ok(Some(raw));
        }
        let version = self
            .repo
            .asset_version(keys::BACKGROUND_ASSET_KEY)
            .await?
            .unwrap_or_default();
        // 时间戳里的分隔符去掉，只留数字：查询串里不需要转义
        let version: String = version.chars().filter(|ch| ch.is_ascii_digit()).collect();
        Ok(Some(format!(
            "{}?v={}",
            keys::BACKGROUND_PUBLIC_PATH,
            version
        )))
    }

    /// 站点默认头像的对外地址。
    ///
    /// 与背景图同一套：外链原样返回；上传的那张用本站地址并带版本号，
    /// 换图之后旧缓存立刻失效。没配过返回 `None`——前端回落到首字母方块。
    pub async fn default_avatar_url(&self) -> Result<Option<String>> {
        let Some(raw) = self.raw(keys::SITE_DEFAULT_AVATAR).await? else {
            return Ok(None);
        };
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return Ok(None);
        }
        if raw != keys::AVATAR_ASSET_MARKER {
            return Ok(Some(raw));
        }
        let version = self
            .repo
            .asset_version(keys::AVATAR_ASSET_KEY)
            .await?
            .unwrap_or_default();
        let version: String = version.chars().filter(|ch| ch.is_ascii_digit()).collect();
        Ok(Some(format!("{}?v={}", keys::AVATAR_PUBLIC_PATH, version)))
    }
}

/// 保持原顺序去重。
fn dedupe(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|item| seen.insert(item.clone())).collect()
}

/// 解析逗号分隔的格式白名单：扩展名先归一化（`jpeg`→`jpg`、`tif`→`tiff`），
/// 丢掉不认识的，再去重。
///
/// `jpeg` 折成 `jpg` 这一步不能省：嗅探出来的扩展名只会是 `jpg`，白名单里
/// 留着 `jpeg` 的话，管理员勾了 JPEG 反而传不上任何 jpg。settings 里的
/// `upload.formats` 与策略行的 `formats` 共用这一套解释，同一份清单写在
/// 两处不会算出不同结果。
pub fn parse_format_list(raw: &str) -> Vec<String> {
    dedupe(
        raw.split(',')
            // 手写的清单常带空格（「jpg, png」），不 trim 的话整项会被当成
            // 认不出来的扩展名丢掉
            .map(str::trim)
            .map(crate::media::normalize_ext)
            .map(|ext| if ext == "jpeg" { "jpg".to_string() } else { ext })
            .filter(|item| crate::media::rule_for_ext(item).is_some()),
    )
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

/// 广场自动漫游间隔的归一化。
///
/// 0 表示关闭；正数钳在 5 秒到 1 小时之间。库里的值可能被手工改坏，
/// 也可能来自旧版本的 API，因此出库和写入都走同一套规则。
pub fn normalize_auto_refresh_seconds(value: i64) -> i64 {
    if value <= 0 {
        0
    } else {
        value.clamp(
            defaults::PLAZA_AUTO_REFRESH_MIN,
            defaults::PLAZA_AUTO_REFRESH_MAX,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册域名白名单：空 = 不限，填了按域与子域命中。
    #[test]
    fn register_domains_parse_and_match() {
        let empty = RegisterDomains::parse(None);
        assert!(empty.is_empty());
        assert!(empty.allows("anyone@anywhere.net"), "没配清单就不限域名");
        assert!(RegisterDomains::parse(Some("   ")).is_empty());

        // 写法上宽容一点：@ 前缀、大小写、多余空白、分号分隔
        let list = RegisterDomains::parse(Some(" @Example.com; corp.example.com ,, "));
        assert_eq!(list.domains(), ["example.com", "corp.example.com"]);
        assert_eq!(list.to_value(), "example.com,corp.example.com");

        assert!(list.allows("a@example.com"));
        assert!(list.allows("A@EXAMPLE.COM"));
        assert!(list.allows("b@mail.example.com"), "子域算命中");
        assert!(list.allows("c@corp.example.com"));
        assert!(!list.allows("d@example.com.evil.net"), "后缀像但不是子域");
        assert!(!list.allows("e@example.org"));
        assert!(!list.allows("没有at符号"));
    }

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
        assert_eq!(defaults::PLAZA_AUTO_REFRESH_SECONDS, 5);
    }

    #[test]
    fn auto_refresh_interval_is_normalized() {
        assert_eq!(normalize_auto_refresh_seconds(-1), 0);
        assert_eq!(normalize_auto_refresh_seconds(0), 0);
        assert_eq!(normalize_auto_refresh_seconds(1), 5);
        assert_eq!(normalize_auto_refresh_seconds(30), 30);
        assert_eq!(normalize_auto_refresh_seconds(3600), 3600);
        assert_eq!(normalize_auto_refresh_seconds(9999), 3600);
    }
}
