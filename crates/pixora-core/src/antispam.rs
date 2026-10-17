//! 反临时邮箱三层拦截。
//!
//! 设计稿（7.1）要求把临时邮箱挡在注册门外，分三层，命中即拒：
//!
//!   1. **格式校验**：本地部分与域名都要合法，域名必须有点号。不查库也不联网，
//!      所以任何入口都能先过一遍，也只有它能被安装向导复用（离线环境没有 DNS）。
//!   2. **内置一次性域名清单**：查 `disposable_domains` 表。清单随镜像打包，
//!      通过 [`AppState::seed_disposable_domains`] 幂等写入，不硬编码在判断逻辑里。
//!   3. **MX 记录校验**：查这个域名有没有邮件服务器。没有 MX 才按 RFC 5321
//!      回落到域名本身的 A/AAAA 记录；发布 null MX（`MX 0 .`）的域名是明确
//!      宣告不收信。这一层不只守注册：改邮箱、找回密码、登录码这些**发信前**
//!      都过一遍，别把信发给一个注定退回来的地址——退信会堆在发信用的企业
//!      邮箱里，发信信誉也跟着掉。
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

/// DNS 解析的超时。内网 DNS 挂掉时可能拖到几十秒，注册接口不能陪着一起等；
/// 超时按「判断不了」处理，保守拒绝（见 `ensure_domain_can_receive`）。
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// 逐个探 MX 主机的上限。按优先级排下来，前几台都解析不出来基本就是域名有问题，
/// 不必把一整串都查一遍。
const MAX_MX_PROBES: usize = 3;

/// 收件域名的收信能力。
enum MailRoute {
    /// 收得到：有 MX，或者没有 MX 但域名本身有地址记录（RFC 5321 的回落）
    Deliverable,
    /// 收不到信。附一句能直接给用户看的理由。
    NoMailServer(String),
    /// 判断不了：解析器故障或超时。不等于收不到，但对外一律按拒处理——
    /// 宁可让域名临时抖动的人稍后重试，也不把信发去一个查不出来的地址。
    Unknown(String),
}

impl AppState {
    /// 注册、找回密码这类发信入口都用这一关。
    ///
    /// 后两层看设置开关（`antispam.disposable` / `antispam.mx_check`），关掉的层
    /// 直接跳过——内网自建邮件服务解析不出外网记录时，管理员需要能关掉第三层。
    pub async fn check_email_allowed(&self, email: &str) -> Result<()> {
        check_email_allowed(self, email).await
    }

    /// 这个地址收得到信吗：形状 + 域名能不能收信。不看一次性清单。
    ///
    /// 与 [`AppState::check_email_allowed`] 的区别就在这一条：一次性清单是挡
    /// 注册的规则（**谁能注册**），而这里是「这封信发得出去吗」（**信往哪发**）。
    /// 管理员替人改邮箱、管理员测试发信这些入口要的是后者。
    pub async fn ensure_deliverable(&self, email: &str) -> Result<()> {
        let domain = validate_email_format(email)?;
        self.ensure_domain_can_receive(&domain).await
    }

    /// 域名收不收得到信。
    ///
    /// 判定顺序：MX 记录 → 没有 MX 时回落到域名本身的 A/AAAA（RFC 5321 §5.1）。
    /// 发布 null MX 的域名是明确宣告「这个域名不收信」，直接拒。
    ///
    /// 开关（`antispam.mx_check`）关掉就整层跳过：域名只在内网有记录时，这一层
    /// 会把所有人拦在外面，管理员得能关掉它——注册与发信两条路一起放行。
    async fn ensure_domain_can_receive(&self, domain: &str) -> Result<()> {
        if !self.settings.bool(keys::ANTISPAM_MX, true).await? {
            return Ok(());
        }
        let route = match tokio::time::timeout(DNS_TIMEOUT, self.probe_mail_route(domain)).await {
            Ok(route) => route,
            Err(_) => MailRoute::Unknown(format!(
                "解析 {domain} 超时，暂时判断不了这个邮箱能不能收信，请稍后重试"
            )),
        };
        match route {
            MailRoute::Deliverable => Ok(()),
            MailRoute::NoMailServer(reason) => Err(AppError::forbidden(reason)),
            MailRoute::Unknown(reason) => Err(AppError::Config(format!(
                "{reason}；若本站的邮件走内网解析不出去，可在后台关掉「收件域名校验」"
            ))),
        }
    }

    /// 查一遍 MX/A。不判超时与开关，那些在调用方。
    async fn probe_mail_route(&self, domain: &str) -> MailRoute {
        let resolver = match self.dns_resolver().await {
            Ok(resolver) => resolver,
            Err(err) => return MailRoute::Unknown(err.message().to_string()),
        };

        match resolver.mx_lookup(domain).await {
            Ok(lookup) => {
                let mut exchanges: Vec<(u16, hickory_resolver::proto::rr::Name)> = lookup
                    .answers()
                    .iter()
                    .filter_map(|record| match &record.data {
                        hickory_resolver::proto::rr::RData::MX(mx) => {
                            Some((mx.preference, mx.exchange.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                if exchanges.iter().any(|(_, name)| name.is_root()) {
                    return MailRoute::NoMailServer(format!(
                        "域名 {domain} 声明了不收邮件（null MX），请换一个邮箱"
                    ));
                }
                // 记录是按优先级给收信方的，探测顺序照它来
                exchanges.sort_by_key(|(preference, _)| *preference);
                if exchanges.is_empty() {
                    return self.probe_address_fallback(domain).await;
                }
                for (_, host) in exchanges.iter().take(MAX_MX_PROBES) {
                    if resolver
                        .lookup_ip(host.clone())
                        .await
                        .map(|ips| ips.iter().next().is_some())
                        .unwrap_or(false)
                    {
                        return MailRoute::Deliverable;
                    }
                }
                MailRoute::NoMailServer(format!(
                    "域名 {domain} 的邮件服务器解析不出来，这个地址收不到信"
                ))
            }
            // 没有 MX 记录：按 RFC 5321 回落到域名自身的地址记录
            Err(err) if err.is_no_records_found() => self.probe_address_fallback(domain).await,
            Err(err) if err.is_nx_domain() => {
                MailRoute::NoMailServer(format!("域名 {domain} 不存在，请检查地址是否写错"))
            }
            Err(err) => {
                // 解析器自己出了问题（连不上配置里的 DNS、超时、系统换了 DNS 之后
                // 的旧配置），丢掉它，下次调用重新读系统配置
                self.drop_dns_resolver().await;
                MailRoute::Unknown(format!("查询 {domain} 的邮件服务器失败：{err}"))
            }
        }
    }

    /// 没有 MX 时的回落：域名本身有没有 A/AAAA。有就按「信发给它自己」处理。
    async fn probe_address_fallback(&self, domain: &str) -> MailRoute {
        let Ok(resolver) = self.dns_resolver().await else {
            return MailRoute::Unknown(format!("判断不了 {domain} 能不能收信"));
        };
        match resolver.lookup_ip(domain).await {
            Ok(ips) if ips.iter().next().is_some() => MailRoute::Deliverable,
            Ok(_) => MailRoute::NoMailServer(format!(
                "域名 {domain} 没有邮件服务器记录（MX），也没有地址记录，这个地址收不到信"
            )),
            Err(err) if err.is_nx_domain() => {
                MailRoute::NoMailServer(format!("域名 {domain} 不存在，请检查地址是否写错"))
            }
            Err(err) => {
                self.drop_dns_resolver().await;
                MailRoute::Unknown(format!("解析 {domain} 失败：{err}"))
            }
        }
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

    // 3. MX 记录（这一层的开关在 ensure_domain_can_receive 里）
    state.ensure_domain_can_receive(&domain).await?;

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
            samesite: crate::config::SameSite::Lax,
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
        // 联网时返回 NXDOMAIN。两种情况都算「收不到信」这一类：断网时按
        // 「查不出来」保守拒绝（码是 INTERNAL），联网时域名不存在（FORBIDDEN）
        let error = state
            .check_email_allowed("someone@pixora-nonexistent-host.invalid")
            .await
            .expect_err("解析不出的域名应该被拒绝");
        assert!(
            matches!(error.code(), "FORBIDDEN" | "INTERNAL"),
            "{} / {}",
            error.code(),
            error.message()
        );
    }

    /// 收信判定：先看 MX。
    ///
    /// `example.com` 发布的是 null MX（`MX 0 .`，RFC 7505），也就是明确宣告
    /// 「这个域名不收邮件」——正是「地址看着像真的、其实收不到信」的那一类，
    /// 也是以前只看 A 记录时最容易被放过去的。有 MX 的域名照旧放行。
    #[tokio::test]
    async fn null_mx_is_treated_as_undeliverable() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        state
            .settings
            .set_bool(keys::ANTISPAM_DISPOSABLE, false)
            .await
            .unwrap();

        let error = state
            .ensure_deliverable("someone@example.com")
            .await
            .expect_err("null MX 的域名应当被拒绝");
        // 断网时走的是「查不出来」那一类，一样拒，只是错误码不同
        assert!(
            matches!(error.code(), "FORBIDDEN" | "INTERNAL"),
            "{} / {}",
            error.code(),
            error.message()
        );
        if error.code() == "FORBIDDEN" {
            assert!(error.message().contains("不收邮件"), "{}", error.message());
        }

        // 有 MX 的域名放行；断网时这里会是「查不出来」，绝不能是「收不到信」
        match state.ensure_deliverable("someone@qq.com").await {
            Ok(()) => {}
            Err(err) => assert_eq!(
                err.code(),
                "INTERNAL",
                "有 MX 的域名不该被判成收不到信：{}",
                err.message()
            ),
        }

        // 开关关掉就整层跳过：内网邮件服务要靠它放行
        state
            .settings
            .set_bool(keys::ANTISPAM_MX, false)
            .await
            .unwrap();
        assert!(
            state.ensure_deliverable("someone@example.com").await.is_ok(),
            "关掉校验后不该再拦人"
        );
    }
}

