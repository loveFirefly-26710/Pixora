//! 安装向导（设计稿 9.4）：环境展示、数据库连通验证、管理员创建、站点配置与收尾。
//!
//! 向导阶段库里的表可能还一张都没有，所以两条路线刻意分开：
//!
//!   - **数据库这一步自己建临时连接**（[`check_database`]）：填的连接串还不是
//!     当前进程用的库，拿 `AppState` 的池去验证等于没验证。连上、跑完迁移、
//!     报一句人话，然后把临时池关掉。
//!   - 其余几步走 `AppState`：它们操作的（管理员、设置项）就是本进程后面要用的
//!     那一份数据。
//!
//! 装完之后整组接口一律 404（[`AppState::ensure_install_open`]）。这不是洁癖：
//! 留着 `/install/admin` 等于给了一条改管理员密码的匿名通道。

use serde::{Deserialize, Serialize};

use pixora_repo::db::Db;
use pixora_repo::model::{role, status};
use pixora_repo::{Args, DbDriver, NewUser, PageRequest, UserFilter};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::settings::{self, keys};
use crate::AppState;

/// 步骤编号：1 数据库 / 2 管理员 / 3 站点 / 4 完成。
const STEP_DONE: i64 = 4;

/// 向导状态。界面用它决定打开哪一步；`install_status` 本身不 404——
/// 它是前端判断「要不要进向导」的依据，装完了也要能拿到 `installed: true`。
#[derive(Debug, Clone, Serialize)]
pub struct InstallStatus {
    pub installed: bool,
    /// 当前停留的步骤编号（1-4）
    pub step: i64,
    pub env: InstallEnv,
    /// 已有的迁移记录数，用来判断是不是全新库
    pub tables: i64,
}

/// 环境只读信息，界面直接展示。存储路径不在向导里填（ADR-0003），
/// 所以这里显示的是「从哪来、值是多少」，管理员照这个去改部署文件。
#[derive(Debug, Clone, Serialize)]
pub struct InstallEnv {
    pub data_dir: String,
    pub env_source: String,
    pub db_driver: String,
    pub db_dsn_set: bool,
    pub master_key_set: bool,
    pub master_key_source: String,
}

/// 数据库连接测试的入参。SQLite 时 `database` 是文件路径，其余是库名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbInput {
    pub driver: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: i64,
    pub database: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

/// 管理员账号入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminInput {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub site_name: String,
}

/// 管理员创建结果。密码不回显任何形态。
#[derive(Debug, Clone, Serialize)]
pub struct InstallAccount {
    pub id: i64,
    pub uid: String,
    pub email: String,
    pub default_album_id: i64,
}

/// 站点配置入参。`smtp` 可以不填——先装起来、之后再在后台补邮件配置是允许的，
/// 但那样注册验证码发不出去，界面上要提示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteInput {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub upload_prefix: String,
    #[serde(default)]
    pub smtp: Option<SmtpInput>,
}

/// SMTP 配置入参。字段与后台「邮件与注册验证」表单一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpInput {
    pub host: String,
    #[serde(default)]
    pub port: i64,
    /// 留空按 SSL（465）处理，与设计稿的默认值一致
    #[serde(default)]
    pub security: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub from_name: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
}

/// 收尾结果。
#[derive(Debug, Clone, Serialize)]
pub struct InstallResult {
    pub ok: bool,
    pub admin_uid: String,
    pub login_url: String,
}

impl AppState {
    /// 装完之后整组向导接口一律 404。
    async fn ensure_install_open(&self) -> Result<()> {
        if self.settings.is_installed().await? {
            return Err(AppError::not_found("安装向导"));
        }
        Ok(())
    }

    /// 向导当前状态。
    pub async fn install_status(&self) -> Result<InstallStatus> {
        let installed = self.settings.is_installed().await?;
        let tables = migration_count(self.repo.db()).await;
        let env = InstallEnv {
            data_dir: self.config.data_dir.display().to_string(),
            env_source: data_dir_source(),
            db_driver: self.db_driver.dialect().as_str().to_string(),
            db_dsn_set: !self.config.db_dsn.trim().is_empty(),
            master_key_set: self.config.master_key.is_some(),
            master_key_source: self.config.key_source.describe(),
        };
        let step = if installed {
            STEP_DONE
        } else {
            self.current_step(tables).await?
        };
        Ok(InstallStatus {
            installed,
            step,
            env,
            tables,
        })
    }

    /// 验证数据库连接并按需要建表，返回一句能直接展示给管理员的话。
    pub async fn install_check_database(&self, input: DbInput) -> Result<String> {
        self.ensure_install_open().await?;

        // 先认驱动名：填了不支持的驱动要当场报错，而不是拿正在用的库糊弄过去。
        // 驱动名是用户填的，所以是 400 而不是 500。
        let requested = DbDriver::parse(&input.driver)
            .map_err(|err| AppError::bad_request(err.to_string()))?;

        // 再验证**当前进程正在用的那个连接池**，不另开一个池。两个原因：
        //   1. 数据库配置只走环境变量（ADR-0003），重启后生效的仍是环境变量里的值，
        //      另连一个库只是「试一下」而不会真的切换；
        //   2. 两个池连同一个 SQLite 文件会互相抢写锁（切 WAL 需要独占锁），
        //      启动初期后台任务正在写，第二个池会一直连不上（实测表现为
        //      「pool timed out」，而且只在启动后头几秒出现，最难查）。
        let description = probe_and_migrate(self.repo.db(), self.db_driver).await?;

        // 填的驱动与当前不一致时如实说明：改完环境变量重启才会切换
        if requested != self.db_driver {
            return Ok(format!(
                "{description} 注意：当前进程连接的是 {}，数据库配置由环境变量决定，改完 PIXORA_DB_DRIVER / PIXORA_DB_DSN 并重启后才会切换。",
                self.db_driver.dialect().as_str()
            ));
        }
        Ok(description)
    }

    /// 创建第一个管理员：账号 + 默认相册 + 站点名。
    pub async fn install_admin(&self, input: AdminInput) -> Result<InstallAccount> {
        self.ensure_install_open().await?;

        // 落库的地址要与注册流程一致（去空白 + 小写），否则用户照着输入的
        // 大小写登不进来。形状校验复用反临时邮箱那一层，这里刻意不做
        // 一次性域名与 MX 校验：安装环境往往是内网，装不起来比收不到信更糟。
        let email = input.email.trim().to_ascii_lowercase();
        crate::antispam::validate_email_format(&email)?;
        pixora_crypto::password::check_strength(&input.password)
            .map_err(|err| AppError::bad_request(err.to_string()))?;

        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已经注册过".into()));
        }
        if self.has_admin().await? {
            return Err(AppError::bad_request("管理员账号已创建，请进入下一步"));
        }

        let password_hash = pixora_crypto::password::hash_password(&input.password)?;
        let (quota_default, _) = self.settings.quota_values().await?;
        let user = self
            .create_first_admin(&email, &password_hash, quota_default)
            .await?;

        // 默认相册是上传不指定相册时的落点，缺了它第一次上传就得报错
        let album = self.repo.insert_default_album(user.id, "默认相册").await?;

        // 记下这个账号：它是安装账号，别的管理员动不了它（改资料、处置、切换登录、删除）
        self.settings
            .set(keys::INSTALL_ADMIN_UID, &user.uid)
            .await?;

        let site_name = input.site_name.trim();
        if !site_name.is_empty() {
            self.settings.set(keys::SITE_NAME, site_name).await?;
        }

        self.audit_detail(
            actions::REGISTER,
            Some(user.id),
            user.uid.clone(),
            serde_json::json!({ "role": "admin", "via": "install" }),
            None,
        )
        .await;

        Ok(InstallAccount {
            id: user.id,
            uid: user.uid,
            email: user.email,
            default_album_id: album.id,
        })
    }

    /// 写站点地址、直链前缀与 SMTP 配置，然后按新配置重建邮件发送器。
    pub async fn install_site(&self, input: SiteInput) -> Result<()> {
        self.ensure_install_open().await?;

        if let Some(base_url) = normalize_base_url(&input.base_url)? {
            self.settings.set(keys::SITE_BASE_URL, &base_url).await?;
        }
        // 前缀会成为直链的第一段路径，必须净化（去掉斜杠点号、限制长度）
        let prefix = settings::normalize_prefix(&input.upload_prefix);
        self.settings.set(keys::UPLOAD_PREFIX, &prefix).await?;

        if let Some(smtp) = &input.smtp {
            self.save_smtp(smtp).await?;
        }
        // 设置改完要让正在用的发送器跟上，不然本进程还拿旧配置发信
        self.reload_mailer().await;

        self.audit_detail(
            actions::SETTINGS_CHANGE,
            None,
            "install.site",
            serde_json::json!({
                "upload_prefix": prefix,
                "smtp_configured": input.smtp.is_some(),
            }),
            None,
        )
        .await;
        Ok(())
    }

    /// 收尾：标记已安装，返回登录入口。
    ///
    /// 先确认管理员存在再落标记——标记一旦写下去，整组向导接口就 404 了，
    /// 那时才发现没有管理员账号，只能手工改库。
    pub async fn install_finish(&self) -> Result<InstallResult> {
        self.ensure_install_open().await?;

        let admin = self
            .repo
            .list_users(
                &UserFilter {
                    role: Some(role::ADMIN.to_string()),
                    ..Default::default()
                },
                PageRequest::new(Some(1), Some(1), 1, 1),
            )
            .await?
            .items
            .into_iter()
            .next()
            .ok_or_else(|| AppError::bad_request("请先创建管理员账号"))?;

        self.settings.mark_installed().await?;
        self.audit_detail(
            actions::SETTINGS_CHANGE,
            Some(admin.id),
            keys::INSTALLED,
            serde_json::json!({ "admin_uid": admin.uid }),
            None,
        )
        .await;

        Ok(InstallResult {
            ok: true,
            admin_uid: admin.uid,
            login_url: "/login".to_string(),
        })
    }

    /// 已完成的步数决定停在第几步：表结构就绪、管理员已建、站点已配各算一步。
    /// 启动流程本身就跑过迁移，所以全新实例通常从第 2 步（管理员）开始。
    async fn current_step(&self, tables: i64) -> Result<i64> {
        let mut done = 0;
        if tables > 0 {
            done += 1;
        }
        if self.has_admin().await? {
            done += 1;
        }
        if self.site_configured().await? {
            done += 1;
        }
        Ok((1 + done).min(STEP_DONE))
    }

    async fn has_admin(&self) -> Result<bool> {
        let page = self
            .repo
            .list_users(
                &UserFilter {
                    role: Some(role::ADMIN.to_string()),
                    ..Default::default()
                },
                PageRequest::new(Some(1), Some(1), 1, 1),
            )
            .await?;
        Ok(page.total > 0)
    }

    /// 站点这一步写没写过：地址或 SMTP 任一落了库就算配过。
    async fn site_configured(&self) -> Result<bool> {
        let base_url = self.settings.string(keys::SITE_BASE_URL, "").await?;
        if !base_url.trim().is_empty() {
            return Ok(true);
        }
        let smtp_host = self.settings.string(keys::MAIL_SMTP_HOST, "").await?;
        Ok(!smtp_host.trim().is_empty())
    }

    /// 建第一个管理员。UID 是四位随机数，理论上会撞，撞了就换一个再试。
    async fn create_first_admin(
        &self,
        email: &str,
        password_hash: &str,
        quota_bytes: i64,
    ) -> Result<pixora_repo::model::User> {
        for _ in 0..8 {
            let uid = pixora_crypto::token::new_uid(4)?;
            if self.repo.uid_exists(&uid).await? {
                continue;
            }
            let new = NewUser {
                uid,
                email: email.to_string(),
                password_hash: password_hash.to_string(),
                role: role::ADMIN.to_string(),
                status: status::ACTIVE.to_string(),
                quota_bytes,
                email_verified: true,
            };
            match self.repo.create_user(&new).await {
                Ok(user) => return Ok(user),
                // 唯一键冲突（UID 或邮箱）：换一个 UID 再来
                Err(err) if err.is_unique_violation() => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(AppError::internal("分配 UID 失败，请重试"))
    }

    /// 写 SMTP 设置。主机留空表示「这一步先跳过」，不算错误——
    /// 但要提示注册验证码发不出去，界面负责说这件事。
    async fn save_smtp(&self, smtp: &SmtpInput) -> Result<()> {
        let host = smtp.host.trim();
        if host.is_empty() {
            return Ok(());
        }
        self.settings.set(keys::MAIL_SMTP_HOST, host).await?;

        let security = pixora_mail::MailSecurity::parse(&smtp.security);
        let port = match smtp.port {
            0 => i64::from(security.default_port()),
            value if (1..=65535).contains(&value) => value,
            _ => return Err(AppError::bad_request("SMTP 端口不合法")),
        };
        self.settings.set_integer(keys::MAIL_SMTP_PORT, port).await?;
        self.settings
            .set(keys::MAIL_SMTP_SECURITY, security.as_str())
            .await?;

        let user = smtp.user.trim();
        self.settings.set(keys::MAIL_SMTP_USER, user).await?;
        // 密码留空表示「这次不设置」而不是「清空」：向导重跑时不该把
        // 已经配好的口令抹掉
        if !smtp.password.is_empty() {
            self.settings
                .set_secret(keys::MAIL_SMTP_PASSWORD, &smtp.password)
                .await?;
        }

        if let Some(name) = smtp.from_name.as_deref().map(str::trim) {
            if !name.is_empty() {
                self.settings.set(keys::MAIL_FROM_NAME, name).await?;
            }
        }
        // 发件地址缺省就是 SMTP 账号本身（企业邮箱的用户名通常就是完整地址）
        let from_email = smtp
            .from_email
            .as_deref()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .unwrap_or(user);
        if !from_email.is_empty() {
            crate::antispam::validate_email_format(from_email)?;
            self.settings
                .set(keys::MAIL_FROM_EMAIL, from_email)
                .await?;
        }
        Ok(())
    }
}

/// 独立入口：向导第一步可能在数据库还没接上、`AppState` 还不存在时调用，
/// 所以这里自己建临时连接，用完即关，不碰进程里的池。
pub async fn check_database(input: &DbInput) -> Result<String> {
    let driver = DbDriver::parse(&input.driver).map_err(|err| AppError::bad_request(err.to_string()))?;
    let dsn = build_dsn(input, driver)?;

    let db = Db::connect(driver, &dsn, 2)
        .await
        .map_err(|err| AppError::bad_request(format!("数据库连接失败：{err}")))?;

    let result = probe_and_migrate(&db, driver).await;
    db.close().await;
    result
}

/// 建表之前先数一遍迁移记录：0 表示是全新库，提示语要说「将执行建表」；
/// 已有记录说明是增量升级，提示语要相应改口。
async fn probe_and_migrate(db: &Db, driver: DbDriver) -> Result<String> {
    let existing = migration_count(db).await;
    db.migrate()
        .await
        .map_err(|err| AppError::bad_request(format!("建表失败：{err}")))?;

    let name = server_description(db, driver).await;
    let action = if existing == 0 {
        "将执行建表"
    } else {
        "将执行增量迁移"
    };
    Ok(format!("{name} · 已检测到 {existing} 张表，{action}"))
}

/// 用 `_sqlx_migrations` 的行数当「表结构是否已就绪」的代理指标：
/// 它由 sqlx 在第一次迁移时创建，查不到（表还没建）就按 0 处理。
async fn migration_count(db: &Db) -> i64 {
    db.count(
        "SELECT COUNT(*) AS total FROM _sqlx_migrations",
        Args::new(),
    )
    .await
    .unwrap_or(0)
}

/// 产品名 + 版本 + 字符集。探测失败的段落直接省略——版本号读不出来
/// 不该让连接验证失败，管理员要看的是「连得上、能建表」。
async fn server_description(db: &Db, driver: DbDriver) -> String {
    match driver {
        DbDriver::Postgres => {
            // version() 返回 "PostgreSQL 16.4 on x86_64…"，取前两个词就够用
            let version = scalar(db, "SELECT version() AS v")
                .await
                .map(|raw| {
                    raw.split_whitespace()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let charset = scalar(db, "SELECT current_setting('server_encoding') AS v")
                .await
                .unwrap_or_default();
            compose("PostgreSQL", &version, &charset)
        }
        DbDriver::MySql => {
            let version = scalar(db, "SELECT VERSION() AS v").await.unwrap_or_default();
            let charset = scalar(db, "SELECT @@character_set_server AS v")
                .await
                .unwrap_or_default();
            compose("MySQL", &version, &charset)
        }
        DbDriver::Sqlite => {
            let version = scalar(db, "SELECT sqlite_version() AS v")
                .await
                .unwrap_or_default();
            // SQLite 只有 UTF-8/UTF-16 两种内部编码，默认就是 UTF-8
            compose("SQLite", &version, "UTF-8")
        }
    }
}

fn compose(product: &str, version: &str, charset: &str) -> String {
    let mut out = if version.trim().is_empty() {
        product.to_string()
    } else {
        format!("{product} {}", version.trim())
    };
    if !charset.trim().is_empty() {
        out.push_str(&format!(" · 字符集 {}", charset.trim()));
    }
    out
}

/// 读一个单值查询的文本结果。
async fn scalar(db: &Db, sql: &str) -> Option<String> {
    let row = db.fetch_one(sql, Args::new()).await.ok()?;
    pixora_repo::model::get_str(&row, "v").ok()
}

/// 拼连接串。SQLite 用 `database` 字段当文件路径，其余按 用户名:密码@主机:端口/库名 拼。
fn build_dsn(input: &DbInput, driver: DbDriver) -> Result<String> {
    let database = input.database.trim();
    if database.is_empty() {
        return Err(AppError::bad_request(if driver == DbDriver::Sqlite {
            "SQLite 需要填写数据库文件路径"
        } else {
            "请填写数据库名"
        }));
    }
    if driver == DbDriver::Sqlite {
        return Ok(database.to_string());
    }

    let scheme = if driver == DbDriver::Postgres {
        "postgres"
    } else {
        "mysql"
    };
    let host = {
        let trimmed = input.host.trim();
        if trimmed.is_empty() {
            "127.0.0.1".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let port = match input.port {
        0 => {
            if driver == DbDriver::Postgres {
                5432
            } else {
                3306
            }
        }
        value if (1..=65535).contains(&value) => value,
        _ => return Err(AppError::bad_request("数据库端口不合法")),
    };

    let user = input.username.trim();
    let mut credentials = String::new();
    if !user.is_empty() {
        credentials.push_str(&encode_userinfo(user));
        if !input.password.is_empty() {
            credentials.push(':');
            credentials.push_str(&encode_userinfo(&input.password));
        }
        credentials.push('@');
    }

    Ok(format!(
        "{scheme}://{credentials}{host}:{port}/{database}"
    ))
}

/// 连接串里的用户名与密码要做百分号编码：密码里出现 `@`、`:`、`/` 这类字符
/// 会直接改写连接串的语义（解析出的主机变成后半段），编码之后才安全。
fn encode_userinfo(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || "-._~".contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 站点地址净化：留空表示按请求推断；填了就必须是 http/https，并去掉尾部斜杠
/// （直链是 `{base_url}/{prefix}/…` 拼出来的，多一条斜杠就是双斜杠 URL）。
fn normalize_base_url(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(AppError::bad_request(
            "站点地址需要以 http:// 或 https:// 开头",
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// 数据目录的来源，界面上如实展示（ADR-0003：路径不在界面写入，只能看）。
fn data_dir_source() -> String {
    match std::env::var("PIXORA_DATA_DIR") {
        Ok(value) if !value.trim().is_empty() => "PIXORA_DATA_DIR".to_string(),
        _ => "默认值（当前工作目录下的 ./data）".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::config::{Config, KeySource};

    /// 装一个只跑 SQLite + 本地目录的 AppState。
    async fn test_state(dir: &TempDir) -> Arc<AppState> {
        let config = Config {
            bind: "127.0.0.1:0".parse().expect("监听地址"),
            db_driver: pixora_repo::DbDriver::Sqlite,
            db_dsn: dir
                .path()
                .join("pixora-test.db")
                .to_string_lossy()
                .into_owned(),
            db_max_connections: 2,
            data_dir: dir.path().to_path_buf(),
            master_key: Some(pixora_crypto::MasterKey::generate().expect("主密钥")),
            key_source: KeySource::Generated(dir.path().join("master.key")),
            workers: 1,
            base_url: None,
            trust_proxy: 0,
            web_dir: None,
            accel_prefix: None,
            secure_cookies: false,
            dev_return_codes: false,
            log_level: "warn".into(),
        };
        AppState::bootstrap(config).await.expect("装配 AppState")
    }

    fn admin_input(email: &str, password: &str) -> AdminInput {
        AdminInput {
            email: email.to_string(),
            password: password.to_string(),
            site_name: "Pixora 自建".to_string(),
        }
    }

    /// 走完前三步，测试收尾与「装完之后 404」用。
    async fn install_through_site(state: &AppState) {
        state
            .install_admin(admin_input("admin@example.com", "Pixora2026Abc"))
            .await
            .expect("建管理员");
        state
            .install_site(SiteInput {
                base_url: "http://127.0.0.1:8080".to_string(),
                upload_prefix: "images".to_string(),
                smtp: None,
            })
            .await
            .expect("写站点设置");
    }

    #[tokio::test]
    async fn status_walks_the_wizard_steps() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        let fresh = state.install_status().await.unwrap();
        assert!(!fresh.installed);
        assert_eq!(fresh.step, 2, "启动时已经跑过迁移，应从第 2 步开始");
        assert!(fresh.tables > 0, "迁移记录要数得出来");
        assert_eq!(fresh.env.db_driver, "sqlite");
        assert!(fresh.env.db_dsn_set);
        assert!(fresh.env.master_key_set);
        assert!(
            fresh.env.master_key_source.contains("master.key"),
            "密钥来源要如实展示：{}",
            fresh.env.master_key_source
        );
        assert!(!fresh.env.data_dir.is_empty());
        assert!(!fresh.env.env_source.is_empty());

        state
            .install_admin(admin_input("admin@example.com", "Pixora2026Abc"))
            .await
            .unwrap();
        assert_eq!(state.install_status().await.unwrap().step, 3);

        state
            .install_site(SiteInput {
                base_url: "https://img.example.com".to_string(),
                upload_prefix: "images".to_string(),
                smtp: None,
            })
            .await
            .unwrap();
        assert_eq!(state.install_status().await.unwrap().step, 4);

        let result = state.install_finish().await.unwrap();
        assert!(result.ok);
        assert_eq!(result.login_url, "/login");
        assert!(!result.admin_uid.is_empty());

        let done = state.install_status().await.unwrap();
        assert!(done.installed);
        assert_eq!(done.step, 4);
    }

    #[tokio::test]
    async fn database_probe_migrates_and_reports_readable_text() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        // 向导这一步验证的是**当前进程正在用的连接池**（数据库配置只走环境变量，
        // 另连一个库只是「试一下」，还会与后台任务抢 SQLite 写锁）。
        // 所以这里的探测结果描述的是 state 自己的库：迁移已经跑过 → 增量迁移。
        let input = DbInput {
            driver: "sqlite".to_string(),
            host: String::new(),
            port: 0,
            database: dir.path().join("whatever.db").to_string_lossy().into_owned(),
            username: String::new(),
            password: String::new(),
        };

        let probed = state.install_check_database(input).await.unwrap();
        assert!(probed.contains("SQLite"), "{probed}");
        assert!(probed.contains("字符集"), "{probed}");
        assert!(probed.contains("增量迁移"), "已迁移过的库应当报增量迁移：{probed}");

        // 填的驱动与当前不一致时要如实说明「重启后才切换」
        let mismatch = state
            .install_check_database(DbInput {
                driver: "postgres".to_string(),
                host: "127.0.0.1".to_string(),
                port: 5432,
                database: "pixora".to_string(),
                username: "pixora".to_string(),
                password: "secret".to_string(),
            })
            .await
            .unwrap();
        assert!(
            mismatch.contains("重启") || mismatch.contains("环境变量"),
            "驱动不一致时要说明何时生效：{mismatch}"
        );

        // 独立入口（不开服务时验证连接串）仍然真的去连一次并建表
        let fresh_path = dir.path().join("probe.db");
        let standalone = check_database(&DbInput {
            driver: "sqlite".to_string(),
            host: String::new(),
            port: 0,
            database: fresh_path.to_string_lossy().into_owned(),
            username: String::new(),
            password: String::new(),
        })
        .await
        .unwrap();
        assert!(standalone.contains("已检测到 0 张表"), "{standalone}");
        assert!(standalone.contains("将执行建表"), "{standalone}");
        assert!(fresh_path.exists(), "探测时会真的建库");

        // 不支持的驱动要给人话错误
        let error = state
            .install_check_database(DbInput {
                driver: "oracle".to_string(),
                host: String::new(),
                port: 0,
                database: "x".to_string(),
                username: String::new(),
                password: String::new(),
            })
            .await
            .expect_err("不支持的驱动");
        assert_eq!(error.code(), "BAD_REQUEST");
    }

    #[test]
    fn dsn_building_encodes_credentials() {
        let postgres = DbInput {
            driver: "postgres".to_string(),
            host: "db.internal".to_string(),
            port: 5432,
            database: "pixora".to_string(),
            username: "pixora".to_string(),
            password: "p@ss:word/1".to_string(),
        };
        let dsn = build_dsn(&postgres, DbDriver::Postgres).unwrap();
        assert_eq!(
            dsn,
            "postgres://pixora:p%40ss%3Aword%2F1@db.internal:5432/pixora"
        );

        // 端口留空按默认值补，主机留空落到回环地址
        let mysql = DbInput {
            driver: "mysql".to_string(),
            host: String::new(),
            port: 0,
            database: "pixora".to_string(),
            username: "pixora".to_string(),
            password: String::new(),
        };
        assert_eq!(
            build_dsn(&mysql, DbDriver::MySql).unwrap(),
            "mysql://pixora@127.0.0.1:3306/pixora"
        );

        // SQLite 的 database 就是文件路径，原样透传
        let sqlite = DbInput {
            driver: "sqlite".to_string(),
            host: String::new(),
            port: 0,
            database: "E:/data/pixora.db".to_string(),
            username: String::new(),
            password: String::new(),
        };
        assert_eq!(
            build_dsn(&sqlite, DbDriver::Sqlite).unwrap(),
            "E:/data/pixora.db"
        );

        // 缺库名、端口越界都要挡住
        let missing = DbInput {
            driver: "postgres".to_string(),
            host: "h".to_string(),
            port: 5432,
            database: "  ".to_string(),
            username: "u".to_string(),
            password: "p".to_string(),
        };
        assert!(build_dsn(&missing, DbDriver::Postgres).is_err());
        let bad_port = DbInput {
            port: 99999,
            ..postgres.clone()
        };
        assert!(build_dsn(&bad_port, DbDriver::Postgres).is_err());
    }

    #[tokio::test]
    async fn admin_creation_validates_and_seeds_defaults() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        let error = state
            .install_admin(admin_input("nodomain", "Pixora2026Abc"))
            .await
            .expect_err("邮箱非法");
        assert_eq!(error.code(), "BAD_REQUEST");

        let error = state
            .install_admin(admin_input("admin@example.com", "short"))
            .await
            .expect_err("口令太弱");
        assert_eq!(error.code(), "BAD_REQUEST");

        let account = state
            .install_admin(admin_input("Admin@Example.com", "Pixora2026Abc"))
            .await
            .unwrap();
        assert_eq!(account.email, "admin@example.com", "落库地址去空白并小写");
        assert!(!account.uid.is_empty());

        let user = state
            .repo
            .find_user_by_id(account.id)
            .await
            .unwrap()
            .expect("管理员存在");
        assert_eq!(user.role, role::ADMIN);
        assert_eq!(user.status, status::ACTIVE);
        assert!(user.email_verified);
        assert!(user.is_admin());

        // 默认相册要建好，否则首次上传没有落点
        let albums = state.repo.list_albums_all(account.id).await.unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "默认相册");
        assert_eq!(albums[0].id, account.default_album_id);

        // 站点名落到设置里
        let site_name = state
            .settings
            .string(keys::SITE_NAME, "")
            .await
            .unwrap();
        assert_eq!(site_name, "Pixora 自建");

        // 重复提交不能再建一个管理员，也不能改掉现有账号
        let error = state
            .install_admin(admin_input("other@example.com", "Pixora2026Abc"))
            .await
            .expect_err("管理员已存在");
        assert_eq!(error.code(), "BAD_REQUEST");
    }

    #[tokio::test]
    async fn site_settings_and_mailer_reload() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        let error = state
            .install_site(SiteInput {
                base_url: "img.example.com".to_string(),
                upload_prefix: "images".to_string(),
                smtp: None,
            })
            .await
            .expect_err("地址缺协议头");
        assert_eq!(error.code(), "BAD_REQUEST");

        state
            .install_site(SiteInput {
                base_url: "https://img.example.com/".to_string(),
                upload_prefix: "../i/".to_string(),
                smtp: Some(SmtpInput {
                    host: "smtp.qiye.aliyun.com".to_string(),
                    port: 465,
                    security: "ssl".to_string(),
                    user: "noreply@example.com".to_string(),
                    password: "smtp-secret".to_string(),
                    from_name: Some("Pixora 管理团队".to_string()),
                    from_email: None,
                }),
            })
            .await
            .unwrap();

        let site = state.settings.site(None).await.unwrap();
        assert_eq!(site.base_url, "https://img.example.com", "尾部斜杠要去掉");
        assert_eq!(site.upload_prefix, "i", "前缀要净化成合法路径段");

        let mail = state.settings.mail().await.unwrap();
        assert_eq!(mail.smtp_host, "smtp.qiye.aliyun.com");
        assert_eq!(mail.smtp_port, 465);
        assert_eq!(mail.smtp_security, "ssl");
        assert!(mail.password_set, "密码要以密文入库");
        assert_eq!(
            mail.from_email, "noreply@example.com",
            "发件地址缺省用 SMTP 账号"
        );
        // 明文读回来要和写进去的一致
        let secret = state
            .settings
            .get_secret(keys::MAIL_SMTP_PASSWORD)
            .await
            .unwrap();
        assert_eq!(secret.as_deref(), Some("smtp-secret"));

        // 设置保存后发送器要跟着重建，否则本进程还在用旧配置
        assert!(state.mailer().is_some(), "SMTP 配好之后邮件器应该可用");
    }

    #[tokio::test]
    async fn finish_requires_admin_and_locks_the_wizard() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        let error = state.install_finish().await.expect_err("还没有管理员");
        assert_eq!(error.code(), "BAD_REQUEST");
        assert!(
            error.message().contains("管理员"),
            "{}",
            error.message()
        );

        install_through_site(&state).await;
        let result = state.install_finish().await.unwrap();
        assert!(result.ok);

        // 装完之后整组接口一律 404：不然等于留了一条改管理员密码的通道
        for error in [
            state.install_finish().await.expect_err("重复收尾"),
            state
                .install_admin(admin_input("second@example.com", "Pixora2026Abc"))
                .await
                .expect_err("装完还能建号"),
            state
                .install_site(SiteInput {
                    base_url: "https://evil.example.com".to_string(),
                    upload_prefix: "images".to_string(),
                    smtp: None,
                })
                .await
                .expect_err("装完还能改站点"),
            state
                .install_check_database(DbInput {
                    driver: "sqlite".to_string(),
                    host: String::new(),
                    port: 0,
                    database: dir.path().join("x.db").to_string_lossy().into_owned(),
                    username: String::new(),
                    password: String::new(),
                })
                .await
                .expect_err("装完还能探库"),
        ] {
            assert_eq!(error.code(), "NOT_FOUND", "{}", error.message());
        }

        // 注册表里的账号角色仍是管理员，只有第一个
        assert_eq!(state.repo.count_users().await.unwrap(), 1);
        // 状态接口不 404，前端靠它判断要不要进向导
        assert!(state.install_status().await.unwrap().installed);
    }
}
