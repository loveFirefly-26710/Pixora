//! 共享上下文：装配一次，全进程复用。
//!
//! 这个结构体是各服务之间的枢纽——仓储、存储驱动注册表、限额、IP 规则缓存、
//! 邮件发送器都在这里。刻意不做全局单例：显式传 `Arc<AppState>`，
//! 测试里可以随时装一个只跑 SQLite + 本地目录的实例出来。

pub mod account_log;
pub mod admin;
pub mod albums;
pub mod announcements;
pub mod antispam;
pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod guest;
pub mod identities;
pub mod images;
pub mod install;
pub mod ipguard;
pub mod mail;
pub mod mail_templates;
pub mod media;
pub mod moderation;
pub mod music;
pub mod proxy;
pub mod quota;
pub mod ratelimit;
pub mod settings;
pub mod stats;
pub mod storage_svc;
pub mod terms;
/// 测试脚手架（只在 `cfg(test)` 下编译）
#[cfg(test)]
pub mod tests_support;
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
    /// 「现在就去回收」的信号。删除操作与定时扫描都走这里，
    /// 物理文件始终只由 GC 那个任务删。
    gc_notify: tokio::sync::Notify,
    /// 远端驱动与第三方校验共用的 HTTP 客户端（连接池共享）
    pub http: Arc<reqwest::Client>,
    /// 出网代理解析结果：启动时读一次，后台自检按同一份结果解释链路
    pub proxy: crate::proxy::ProxyInfo,
    /// 系统 DNS 解析器，惰性建一份全局共用（自带 TTL 缓存）。发信前查收件
    /// 域名的 MX 记录用它，见 `antispam`。
    resolver: tokio::sync::RwLock<Option<hickory_resolver::TokioResolver>>,
    /// 口令哈希/校验的并发闸门。
    ///
    /// Argon2id 每次要 19 MiB 内存与一段 CPU；不限制并发时，一波登录就能
    /// 把容器内存和 tokio worker 一起拖住。哈希本身放到 `spawn_blocking`，
    /// 这里只控制同时跑几份。
    password_slots: tokio::sync::Semaphore,
    /// 内容哈希 → 分片锁。上传去重与 GC 删除同一份实体时串行化，
    /// 避免「GC 刚把文件删了，上传又给旧行加了一条引用」。
    blob_locks: Vec<tokio::sync::Mutex<()>>,
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

        // 出网代理：服务器到 Google / GitHub / 存储后端的链路可能需要走代理。
        // Windows 用户开的是「Internet 选项」里的系统代理，reqwest 不读那里，
        // 所以先自己解析（环境变量 / 系统代理），再把结果显式交给客户端。
        let proxy = proxy::detect();
        let mut http_builder = reqwest::Client::builder()
            .user_agent("Pixora")
            // 建连单独限制：DNS 卡住或对端不接时尽快失败，别等整个请求超时
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(60));
        if let Some(url) = proxy.url.as_deref() {
            let mut rule = reqwest::Proxy::all(url).map_err(|err| {
                AppError::Config(format!(
                    "出网代理地址不合法（来源：{}）：{err}",
                    proxy.source.as_deref().unwrap_or("未知")
                ))
            })?;
            if let Some(no_proxy) = proxy.no_proxy.as_deref() {
                rule = rule.no_proxy(reqwest::NoProxy::from_string(no_proxy));
            }
            http_builder = http_builder.proxy(rule);
        }
        if let (Some(url), Some(source)) = (proxy.display.as_deref(), proxy.source.as_deref()) {
            tracing::info!(proxy = url, source, "出网代理已启用");
        } else {
            tracing::debug!("未检测到出网代理，按直连处理");
        }
        let http = Arc::new(
            http_builder
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
        let ip_guard = Arc::new(IpGuard::new(
            repo.clone(),
            settings.clone(),
            config.trust_proxy,
        ));

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
            gc_notify: tokio::sync::Notify::new(),
            http,
            proxy,
            resolver: tokio::sync::RwLock::new(None),
            password_slots: tokio::sync::Semaphore::new(2),
            blob_locks: (0..256).map(|_| tokio::sync::Mutex::new(())).collect(),
            started_at: Utc::now(),
            db_driver,
        });

        state.reload_mailer().await;
        // 灌一次内置的一次性邮箱域名清单（幂等）。不灌的话「反临时邮箱」的
        // 第一层拦截是空的，只剩 MX 校验在干活。
        if let Err(err) = state
            .seed_disposable_domains(
                &crate::antispam::BUILTIN_DOMAINS
                    .iter()
                    .map(|item| item.to_string())
                    .collect::<Vec<_>>(),
            )
            .await
        {
            tracing::warn!(error = %err, "内置一次性邮箱清单写入失败，反垃圾第一层暂不可用");
        }
        // 未安装时公开的安装向导必须先拿到口令；否则谁先访问谁就能建管理员。
        // 口令来自环境变量或数据目录里的 install.token，这里只在未安装时打印。
        if !state.settings.is_installed().await.unwrap_or(false) {
            tracing::warn!(
                install_token = %state.config.install_token,
                source = %state.config.install_token_source,
                "实例尚未安装：访问 /#/install 需要安装口令；口令也会写入数据目录的 install.token"
            );
        }
        Ok(state)
    }

    /// 进程内共用一份系统 DNS 解析器（自带 TTL 缓存）。第一次用到时构建。
    ///
    /// 用系统配置而不是另指一台 DNS 服务器：容器、内网、运营商 DNS 各是各的，
    /// 系统怎么解析，这里就怎么解析。解析器本身出问题时（超时、连不上配置里
    /// 的那台 DNS），`antispam` 会把它丢掉重建，免得一直用一份过期的配置。
    pub(crate) async fn dns_resolver(&self) -> Result<hickory_resolver::TokioResolver> {
        if let Some(resolver) = self.resolver.read().await.clone() {
            return Ok(resolver);
        }
        let built = hickory_resolver::Resolver::builder_tokio()
            .and_then(|builder| builder.build())
            .map_err(|err| AppError::Config(format!("DNS 解析器构建失败：{err}")))?;
        let mut slot = self.resolver.write().await;
        Ok(slot.get_or_insert(built).clone())
    }

    /// 丢掉缓存的那份解析器，下次调用重新读系统配置。
    pub(crate) async fn drop_dns_resolver(&self) {
        *self.resolver.write().await = None;
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
            *self.problem_mut() = Some("SMTP 服务器与发信邮箱都要填，邮件功能才会启用".to_string());
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

    /// 催一次 Blob 回收。删除类操作调用它：让物理文件尽快（而不是等下一轮扫描）
    /// 被回收。真正的删除仍由 GC 任务做，这里只是叫醒它。
    pub fn kick_gc(&self) {
        self.gc_notify.notify_one();
    }

    /// GC 任务等这个信号（或超时）。返回一个 future，与定时器一起 select。
    pub async fn wait_gc_kick(&self) {
        self.gc_notify.notified().await;
    }

    /// 邮件器没能装配出来时，卡在哪一步。
    pub fn mailer_problem(&self) -> Option<String> {
        self.mailer_problem
            .read()
            .expect("邮件器原因锁不该中毒")
            .clone()
    }

    fn problem_mut(&self) -> std::sync::RwLockWriteGuard<'_, Option<String>> {
        self.mailer_problem.write().expect("邮件器原因锁不该中毒")
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

    /// 拼直链。缩略图走 `?thumb=<版本>`，一级目录可在后台改。
    ///
    /// 那个数字是「缩略图怎么生成」的版本号：直链响应带 `immutable` 一年缓存，
    /// 换了缩略图的尺寸之后，光靠刷新浏览器拿不到新图——版本号一抬，URL 就变了，
    /// 旧缓存自然让位。服务端只认「有没有 thumb 这个参数」，值本身不重要。
    pub async fn direct_url(&self, sha256: &str, ext: &str, thumb: bool) -> String {
        let base = self.base_url().await;
        let prefix = self
            .settings
            .site(self.config.base_url.as_deref())
            .await
            .map(|site| site.upload_prefix)
            .unwrap_or_else(|_| settings::defaults::UPLOAD_PREFIX.to_string());
        let suffix = if thumb { "?thumb=2" } else { "" };
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

    /// 按内容哈希取一把分片锁。上传的「查重 → 落盘 → 建引用」与 GC 的
    /// 「复查 → 删文件 → 删行」必须在同一把锁里，否则新引用可能挂到
    /// 一个刚被删掉的文件上。
    pub fn blob_lock(&self, sha256: &str) -> &tokio::sync::Mutex<()> {
        let index =
            sha256.as_bytes().first().copied().unwrap_or(0) as usize % self.blob_locks.len();
        &self.blob_locks[index]
    }

    /// 口令哈希。放到阻塞线程池并限制并发，避免 Argon2 把 async worker 占住。
    pub async fn hash_password(&self, plain: &str) -> Result<String> {
        let _permit = self
            .password_slots
            .acquire()
            .await
            .map_err(|_| AppError::internal("口令处理队列已关闭"))?;
        let plain = plain.to_string();
        tokio::task::spawn_blocking(move || pixora_crypto::password::hash_password(&plain))
            .await
            .map_err(|err| AppError::internal(format!("口令哈希任务失败：{err}")))?
            .map_err(AppError::from)
    }

    /// 口令校验。口径与 [`Self::hash_password`] 相同。
    pub async fn verify_password(&self, plain: &str, stored: &str) -> Result<bool> {
        let _permit = self
            .password_slots
            .acquire()
            .await
            .map_err(|_| AppError::internal("口令处理队列已关闭"))?;
        let plain = plain.to_string();
        let stored = stored.to_string();
        tokio::task::spawn_blocking(move || {
            pixora_crypto::password::verify_password(&plain, &stored)
        })
        .await
        .map_err(|err| AppError::internal(format!("口令校验任务失败：{err}")))?
        .map_err(AppError::from)
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
