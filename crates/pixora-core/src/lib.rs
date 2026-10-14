//! 共享上下文：装配一次，全进程复用。
//!
//! 这个结构体是各服务之间的枢纽——仓储、存储驱动注册表、限额、IP 规则缓存、
//! 邮件发送器都在这里。刻意不做全局单例：显式传 `Arc<AppState>`，
//! 测试里可以随时装一个只跑 SQLite + 本地目录的实例出来。

pub mod admin;
pub mod albums;
pub mod antispam;
pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod images;
pub mod install;
pub mod ipguard;
pub mod media;
pub mod quota;
pub mod ratelimit;
pub mod settings;
pub mod stats;
pub mod storage_svc;
pub mod terms;
pub mod uploads;

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use pixora_mail::Mailer;
use pixora_repo::{db::Db, DbDriver, Repo};
use pixora_storage::StorageRegistry;

pub use config::Config;
pub use error::{AppError, Result};
pub use settings::Settings;

use crate::ipguard::IpGuard;
use crate::ratelimit::RateLimits;
use crate::storage_svc::StorageService;

/// 应用上下文。
pub struct AppState {
    pub config: Arc<Config>,
    pub repo: Repo,
    pub settings: Settings,
    /// 存储服务：管后端配置、驱动装配与容量探测
    pub storage: Arc<StorageService>,
    /// 进程内令牌桶与窗口计数
    pub limits: Arc<RateLimits>,
    /// IP 规则与自动封禁
    pub ip_guard: Arc<IpGuard>,
    /// 邮件发送器。设置改了要重建，所以放在锁里。
    mailer: RwLock<Option<Arc<Mailer>>>,
    /// 没能装配出邮件器的原因。测试发信用它说明白「是配置缺了哪块」，
    /// 而不是笼统回一句「SMTP 未配置完整」。
    mailer_problem: RwLock<Option<String>>,
    /// 远端驱动与第三方校验共用的 HTTP 客户端（连接池共享）
    pub http: Arc<reqwest::Client>,
    pub started_at: DateTime<Utc>,
    pub db_driver: DbDriver,
}

impl AppState {
    /// 连接数据库、跑迁移、装配驱动。进程启动时调一次。
    pub async fn bootstrap(config: Config) -> Result<Arc<Self>> {
        let db = Db::connect(config.db_driver, &config.db_dsn, config.db_max_connections).await?;
        db.migrate().await?;

        let db = Arc::new(db);
        let repo = Repo::new(db);
        let settings = Settings::new(repo.clone(), config.master_key.clone());

        let http = Arc::new(
            reqwest::Client::builder()
                .user_agent("Pixora")
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|err| AppError::internal(format!("HTTP 客户端构建失败：{err}")))?,
        );

        let storage = Arc::new(StorageService::new(
            repo.clone(),
            settings.clone(),
            config.clone(),
            http.clone(),
        ));
        // 装驱动：读库里的后端配置；一张表都没有（全新实例）就只装本地驱动
        if let Err(err) = storage.reload().await {
            tracing::warn!(error = %err, "存储后端装配失败，先回退到本地目录");
            storage.install_local_fallback().await?;
        }

        let limits = Arc::new(RateLimits::new(repo.clone()));
        let ip_guard = Arc::new(IpGuard::new(repo.clone(), settings.clone()));

        let db_driver = config.db_driver;
        let state = Arc::new(Self {
            config: Arc::new(config),
            repo,
            settings,
            storage,
            limits,
            ip_guard,
            mailer: RwLock::new(None),
            mailer_problem: RwLock::new(None),
            http,
            started_at: Utc::now(),
            db_driver,
        });

        state.reload_mailer().await;
        // 灌一次内置的一次性邮箱域名清单（幂等）。不灌的话「反临时邮箱」的
        // 第一层拦截是空的，只剩 MX 校验在干活。
        if let Err(err) = state.seed_disposable_domains(&crate::antispam::BUILTIN_DOMAINS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>())
            .await
        {
            tracing::warn!(error = %err, "内置一次性邮箱清单写入失败，反垃圾第一层暂不可用");
        }
        Ok(state)
    }

    /// 按当前设置重建邮件发送器。设置保存后调一次。
    pub async fn reload_mailer(&self) {
        let mail = match self.settings.mail().await {
            Ok(mail) => mail,
            Err(err) => {
                tracing::warn!(error = %err, "读取邮件设置失败");
                return;
            }
        };

        let password = match self
            .settings
            .get_secret(settings::keys::MAIL_SMTP_PASSWORD)
            .await
        {
            Ok(value) => value.unwrap_or_default(),
            Err(err) => {
                // 拿不到密码就别拿空密码去撞认证：那会把「主密钥不匹配」
                // 显示成「认证失败」，人照着账号密码改一圈也修不好。
                tracing::warn!(error = %err, "读取 SMTP 密码失败");
                *self.mailer.write().expect("邮件器锁不该中毒") = None;
                *self.problem_mut() = Some(format!(
                    "库里的 SMTP 密码读不出来（{err}）；多半是当前主密钥与写入时不一致，重新填一次密码即可"
                ));
                return;
            }
        };

        if mail.smtp_host.trim().is_empty() || mail.from_email.trim().is_empty() {
            *self.mailer.write().expect("邮件器锁不该中毒") = None;
            *self.problem_mut() = Some(
                "SMTP 服务器与发信邮箱都要填，邮件功能才会启用".to_string(),
            );
            return;
        }

        let mut config = pixora_mail::MailConfig::new(
            &mail.smtp_host,
            &mail.smtp_user,
            &password,
            &mail.from_email,
        );
        config.port = mail.smtp_port.max(1) as u16;
        config.security = pixora_mail::MailSecurity::parse(&mail.smtp_security);
        config.from_name = mail.from_name.clone();

        *self.mailer.write().expect("邮件器锁不该中毒") = Some(Arc::new(Mailer::new(config)));
        *self.problem_mut() = None;
    }

    pub fn mailer(&self) -> Option<Arc<Mailer>> {
        self.mailer.read().expect("邮件器锁不该中毒").clone()
    }

    /// 邮件器没能装配出来时，卡在哪一步。
    pub fn mailer_problem(&self) -> Option<String> {
        self.mailer_problem
            .read()
            .expect("邮件器原因锁不该中毒")
            .clone()
    }

    fn problem_mut(&self) -> std::sync::RwLockWriteGuard<'_, Option<String>> {
        self.mailer_problem
            .write()
            .expect("邮件器原因锁不该中毒")
    }

    /// 站点地址。设置里有就用设置的，其次环境变量，最后从监听地址推断。
    pub async fn base_url(&self) -> String {
        if let Ok(site) = self.settings.site(self.config.base_url.as_deref()).await {
            if !site.base_url.trim().is_empty() {
                return site.base_url.trim_end_matches('/').to_string();
            }
        }
        self.config.fallback_base_url()
    }

    /// 拼直链。缩略图走 `?thumb=1`，一级目录可在后台改。
    pub async fn direct_url(&self, sha256: &str, ext: &str, thumb: bool) -> String {
        let base = self.base_url().await;
        let prefix = self
            .settings
            .site(self.config.base_url.as_deref())
            .await
            .map(|site| site.upload_prefix)
            .unwrap_or_else(|_| settings::defaults::UPLOAD_PREFIX.to_string());
        let suffix = if thumb { "?thumb=1" } else { "" };
        format!("{base}/{prefix}/{sha256}.{ext}{suffix}")
    }

    /// 资源归属校验的便捷封装：非管理员只能碰自己的东西。
    pub fn ensure_can_touch(
        &self,
        actor_id: i64,
        is_admin: bool,
        owner_id: i64,
        what: &str,
    ) -> Result<()> {
        if actor_id == owner_id || is_admin {
            return Ok(());
        }
        Err(AppError::forbidden(format!("{what}不属于当前账号")))
    }

    pub async fn shutdown(&self) {
        self.repo.db().close().await;
    }
}

/// 存储注册表的便捷访问。
impl AppState {
    pub fn drivers(&self) -> &StorageRegistry {
        self.storage.registry()
    }
}
