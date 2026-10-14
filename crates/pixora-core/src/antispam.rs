//! 反临时邮箱三层拦截。
//!
//! 设计稿（7.1）要求把临时邮箱挡在注册门外，分三层，命中即拒：
//!
//!   1. **格式校验**：本地部分与域名都要合法，域名必须有点号。不查库也不联网，
//!      所以任何入口都能先过一遍，也只有它能被安装向导复用（离线环境没有 DNS）。
//!   2. **内置一次性域名清单**：查 `disposable_domains` 表。清单随镜像打包，
//!      通过 [`AppState::seed_disposable_domains`] 幂等写入，不硬编码在判断逻辑里。
//!   3. **MX/A 记录校验**：域名解析不出任何地址就拒绝。用系统解析器而不是专门的
//!      MX 查询——绝大多数部署里能收到信的域名都有 A/AAAA 记录，而系统解析器
//!      在每个平台上都可用，不引入 DNS 客户端依赖。
//!
//! 第四件事是**别名归一化**：Gmail 的 `+` 后缀与点号变体、其它域名的 `+` 后缀在判重前
//! 先归一化，否则同一只信箱换个写法就能重复注册。归一化只提供纯函数，不在这里查库：
//! 调用方可能拿它去查 `users.email`，也可能去查封禁名单，绑死一种用法反而是负担。

use std::time::Duration;

use crate::error::{AppError, Result};
use crate::settings::keys;
use crate::AppState;

/// 内置清单在 `disposable_domains.source` 里的标记。
pub const BUILTIN_SOURCE: &str = "builtin";

/// 随镜像打包的基线清单。这是「第一层」的数据来源，幂等写入后由运维在线上更新；
/// 只收长期活跃的一次性邮箱服务，避免把整份网络清单抄进代码后无人维护。
pub const BUILTIN_DOMAINS: &[&str] = &[
    "10minutemail.com",
    "10minutemail.net",
    "20minutemail.com",
    "1secmail.com",
    "1secmail.net",
    "1secmail.org",
    "discard.email",
    "dispostable.com",
    "emailondeck.com",
    "fakeinbox.com",
    "getnada.com",
    "grr.la",
    "guerrillamail.com",
    "guerrillamail.net",
    "guerrillamail.org",
    "guerrillamailblock.com",
    "inboxkitten.com",
    "mailcatch.com",
    "maildrop.cc",
    "mailinator.com",
    "mailinator.net",
    "mailnesia.com",
    "mailsac.com",
    "mintemail.com",
    "moakt.com",
    "mohmal.com",
    "pokemail.net",
    "sharklasers.com",
    "spam4.me",
    "spambog.com",
    "temp-mail.io",
    "temp-mail.org",
    "tempmail.com",
    "tempmail.net",
    "tempmailo.com",
    "tempr.email",
    "throwam.com",
    "throwawaymail.com",
    "trashmail.com",
    "trashmail.de",
    "trashmail.net",
    "yopmail.com",
    "yopmail.fr",
    "yopmail.net",
];

/// DNS 解析的超时。系统解析器在断网环境下可能挂到几十秒，
/// 注册接口不能陪着一起等；超时按「解析不出来」处理（拒绝）。
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

impl AppState {
    /// 注册、找回密码这类发信入口都用这一关。
    ///
    /// 后两层看设置开关（`antispam.disposable` / `antispam.mx_check`），关掉的层
    /// 直接跳过——内网自建邮件服务解析不出外网记录时，管理员需要能关掉第三层。
    pub async fn check_email_allowed(&self, email: &str) -> Result<()> {
        check_email_allowed(self, email).await
    }

    /// 域名能不能解析出地址。失败与超时都按「收不到信」处理：
    /// 宁可让极少数域名解析临时抖动的人稍后重试，也不放行一个收不到验证码的邮箱。
    async fn ensure_domain_deliverable(&self, domain: &str) -> Result<()> {
        let resolved =
            tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((domain, 0))).await;
        let deliverable = match resolved {
            Ok(Ok(mut addresses)) => addresses.next().is_some(),
            Ok(Err(err)) => {
                tracing::debug!(domain, error = %err, "域名解析失败，拒绝该邮箱");
                false
            }
            Err(_) => {
                tracing::debug!(domain, "域名解析超时，拒绝该邮箱");
                false
            }
        };
        if !deliverable {
            return Err(AppError::forbidden(
                "域名解析不出邮件服务器，请使用可正常收信的邮箱",
            ));
        }
        Ok(())
    }

    /// 批量写入一次性域名清单。幂等：重复导入同一份清单不会报错也不重复计数。
    pub async fn seed_disposable_domains(&self, domains: &[String]) -> Result<u64> {
        Ok(self
            .repo
            .insert_disposable_domains(domains, BUILTIN_SOURCE)
            .await?)
    }

    /// 返回（是否启用清单校验，清单条数），后台设置页展示用。
    pub async fn disposable_status(&self) -> Result<(bool, i64)> {
        let enabled = self.settings.bool(keys::ANTISPAM_DISPOSABLE, true).await?;
        let total = self.repo.count_disposable_domains().await?;
        Ok((enabled, total))
    }
}

/// 跨模块的入口写法：`antispam::check_email_allowed(state, email)`。
/// 实现只有这一份，`AppState` 上的同名方法是它的薄包装；
/// 两种写法并存是因为调用点风格不一，与其让各处记住用哪种，不如都提供。
pub async fn check_email_allowed(state: &AppState, email: &str) -> Result<()> {
    // 1. 格式：不合法就没有后面的必要
    let domain = validate_email_format(email)?;

    // 2. 内置清单
    let disposable_enabled = state
        .settings
        .bool(keys::ANTISPAM_DISPOSABLE, true)
        .await?;
    if disposable_enabled && state.repo.disposable_domain_exists(&domain).await? {
        return Err(AppError::forbidden(format!(
            "该邮箱属于一次性邮箱（{domain}），请使用常用邮箱注册"
        )));
    }

    // 3. MX/A 记录
    if state.settings.bool(keys::ANTISPAM_MX, true).await? {
        state.ensure_domain_deliverable(&domain).await?;
    }

    Ok(())
}

/// 邮箱归一化，供「同邮箱重复注册」判重使用。不用于落库——
/// 落库的地址要保持用户输入的样子，否则用户下次照着输入登不进来。
///
/// 三条规则：
///   - 本地部分里的 `+` 之后是别名标签（Gmail、Outlook、QQ 都支持），一律去掉；
///   - Gmail 的点号也被忽略：`a.b@gmail.com` 与 `ab@gmail.com` 是同一只信箱，
///     且 `googlemail.com` 与 `gmail.com` 是同一家服务，统一归一化到 `gmail.com`；
///   - 其余域名只去 `+` 后缀，点号保留——`user.name@example.com` 在很多系统里
///     是两个人，不能替它们做合并。
pub fn normalize_email(email: &str) -> String {
    let trimmed = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return trimmed;
    };

    let without_tag = local.split('+').next().unwrap_or(local);
    // `+tag@x.com` 这种以别名开头的地址去掉后缀就什么都不剩了，此时保留原样，
    // 归一化的前提是「还能认出本地部分」
    let local = if without_tag.is_empty() {
        local
    } else {
        without_tag
    };
    let domain = domain.trim_end_matches('.');

    if is_gmail_domain(domain) {
        format!("{}@gmail.com", local.replace('.', ""))
    } else {
        format!("{local}@{domain}")
    }
}

/// 形状校验。通过时返回小写域名，调用方接着拿它查清单或做 DNS——
/// 免得校验与查清单两处各自从字符串里切域名，切法迟早不一致。
pub fn validate_email_format(email: &str) -> Result<String> {
    let trimmed = email.trim();
    if trimmed.is_empty() {
        return Err(AppError::bad_request("邮箱不能为空"));
    }
    if trimmed.len() > 254 {
        return Err(AppError::bad_request("邮箱格式不正确：长度超出上限"));
    }

    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return Err(AppError::bad_request("邮箱格式不正确：缺少 @"));
    };

    // 本地部分
    if local.is_empty() || local.len() > 64 {
        return Err(AppError::bad_request(
            "邮箱格式不正确：@ 之前的长度不合法",
        ));
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(AppError::bad_request(
            "邮箱格式不正确：@ 之前不能以点号开头结尾或出现连续点号",
        ));
    }
    if !local
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "._%+-".contains(ch))
    {
        return Err(AppError::bad_request(
            "邮箱格式不正确：@ 之前含有不支持的字符",
        ));
    }

    // 域名：必须有点号，每一段都要是合法标签
    let domain = domain.to_ascii_lowercase();
    if domain.is_empty() || domain.len() > 253 {
        return Err(AppError::bad_request(
            "邮箱格式不正确：@ 之后的长度不合法",
        ));
    }
    if !domain.contains('.') {
        return Err(AppError::bad_request(
            "邮箱格式不正确：域名必须包含点号",
        ));
    }
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(AppError::bad_request("邮箱格式不正确：域名分段长度不合法"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(AppError::bad_request(
                "邮箱格式不正确：域名分段不能以连字符开头或结尾",
            ));
        }
        if !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(AppError::bad_request("邮箱格式不正确：域名含有不支持的字符"));
        }
    }

    // 顶级域名：至少两个字符；中文等国际化域名要求调用方先转成 punycode（`xn--`）
    let tld = domain.rsplit('.').next().unwrap_or_default();
    let tld_ok = tld.len() >= 2
        && (tld.chars().all(|ch| ch.is_ascii_alphabetic()) || tld.starts_with("xn--"));
    if !tld_ok {
        return Err(AppError::bad_request("邮箱格式不正确：顶级域名不合法"));
    }

    Ok(domain)
}

/// Gmail 的两个域名等价，归一化时合并到同一个。
fn is_gmail_domain(domain: &str) -> bool {
    domain == "gmail.com" || domain == "googlemail.com"
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

    #[test]
    fn gmail_aliases_collapse_to_one_address() {
        // 点号与 + 后缀两个花样都要被抹平，否则同一只信箱能注册两次
        let collapsed = normalize_email("a.b+tag@gmail.com");
        assert_eq!(collapsed, "ab@gmail.com");
        assert_eq!(normalize_email("ab@gmail.com"), collapsed);
        assert_eq!(normalize_email("A.B+Tag@Gmail.com"), collapsed);
        assert_eq!(normalize_email("  a.b+tag@gmail.com  "), collapsed);
        // googlemail 与 gmail 是同一家服务，换域也算重复
        assert_eq!(normalize_email("a.b+tag@googlemail.com"), collapsed);
    }

    #[test]
    fn other_domains_only_drop_the_plus_tag() {
        // 点号在别的域名里是有意义的，只去 + 之后的部分
        assert_eq!(normalize_email("user+tag@example.com"), "user@example.com");
        assert_eq!(
            normalize_email("user.name+tag@example.com"),
            "user.name@example.com"
        );
        assert_eq!(normalize_email("User@Example.COM"), "user@example.com");
        // 去掉别名后本地部分为空时保留原样，归一化不能把人认丢
        assert_eq!(normalize_email("+tag@example.com"), "+tag@example.com");
        // 没有 @ 的输入原样返回（小写去空白），交给格式校验去拒
        assert_eq!(normalize_email("  GMAIL  "), "gmail");
    }

    #[test]
    fn email_shape_validation() {
        assert_eq!(
            validate_email_format("Chen.Yu@Example.com").unwrap(),
            "example.com"
        );
        assert!(validate_email_format("a@b.co").is_ok());
        assert!(validate_email_format("a.b+x@mail.example.com").is_ok());

        for bad in [
            "",
            "   ",
            "nodomain",
            "a@localhost", // 域名必须有点号
            "a@.com",
            "a@com.",
            "a@a..com",
            "a@-a.com",
            "a@a-.com",
            "a@a.c",  // 顶级域名太短
            "a@a.12", // 顶级域名不能是数字
            ".a@example.com",
            "a.@example.com",
            "a..b@example.com",
            "a b@example.com",
            "a@ex ample.com",
            "@example.com",
            "a@",
        ] {
            assert!(validate_email_format(bad).is_err(), "{bad:?} 应该被拒绝");
        }

        // punycode 顶级域名要放行，中文域名先转码再用
        assert!(validate_email_format("a@example.xn--p1ai").is_ok());
    }

    #[tokio::test]
    async fn disposable_list_blocks_when_enabled() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        // 离线环境：关掉 MX 校验，只验证清单这一层
        state
            .settings
            .set_bool(keys::ANTISPAM_MX, false)
            .await
            .unwrap();
        state
            .settings
            .set_bool(keys::ANTISPAM_DISPOSABLE, true)
            .await
            .unwrap();

        // 启动时已经灌过一次内置清单，所以初始条数就是内置清单的长度
        let (enabled, seeded) = state.disposable_status().await.unwrap();
        assert!(enabled);
        assert_eq!(seeded as usize, BUILTIN_DOMAINS.len());

        let domains: Vec<String> = BUILTIN_DOMAINS.iter().map(|item| item.to_string()).collect();
        // 幂等：再灌一次不新增
        let inserted = state.seed_disposable_domains(&domains).await.unwrap();
        assert_eq!(inserted, 0);

        // 新增一条自定义域名才算新增
        let again = state.seed_disposable_domains(&domains).await.unwrap();
        assert_eq!(again, 0);

        let error = state
            .check_email_allowed("spam@mailinator.com")
            .await
            .expect_err("一次性邮箱应该被拦下");
        assert_eq!(error.code(), "FORBIDDEN");
        assert!(error.message().contains("一次性邮箱"), "{}", error.message());

        // 列表里没有的域名放行（此时 MX 校验是关的）
        assert!(state.check_email_allowed("chenyu@example.com").await.is_ok());

        // 关掉开关后清单不再拦人
        state
            .settings
            .set_bool(keys::ANTISPAM_DISPOSABLE, false)
            .await
            .unwrap();
        assert!(state.check_email_allowed("spam@mailinator.com").await.is_ok());
    }

    #[tokio::test]
    async fn bad_shape_is_rejected_before_any_lookup() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;

        let error = state
            .check_email_allowed("nodomain")
            .await
            .expect_err("格式非法要先被拦下");
        assert_eq!(error.code(), "BAD_REQUEST");
        assert!(error.message().contains("格式"), "{}", error.message());

        // 跨模块调用点写的是 `antispam::check_email_allowed(state, email)`
        // （auth.rs 的注册流程就是这么调的），两种写法必须走同一条路径
        let error = check_email_allowed(&state, "nodomain")
            .await
            .expect_err("自由函数入口同样要拦");
        assert_eq!(error.code(), "BAD_REQUEST");
    }

    #[tokio::test]
    async fn unresolvable_domain_is_rejected_when_mx_check_on() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        state
            .settings
            .set_bool(keys::ANTISPAM_DISPOSABLE, false)
            .await
            .unwrap();
        state
            .settings
            .set_bool(keys::ANTISPAM_MX, true)
            .await
            .unwrap();

        // `.invalid` 是 RFC 6761 保留的不可解析顶级域名：断网时解析失败、
        // 联网时返回 NXDOMAIN，两种情况下都应该被拒绝。
        let error = state
            .check_email_allowed("someone@pixora-nonexistent-host.invalid")
            .await
            .expect_err("解析不出的域名应该被拒绝");
        assert_eq!(error.code(), "FORBIDDEN");
    }
}
