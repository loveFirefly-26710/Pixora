//! 认证与会话。
//!
//! 一条主线：**账号状态只在鉴权这一处判断**。设计稿把这条写进了代码评审清单——
//! 判断散落到各 handler，漏一处就等于给暂停的账号留了个后门。所以
//! [`AppState::authenticate_session`] 与 [`AppState::authenticate_token`] 是
//! 仅有的两个入口，它们都调用同一个 [`AppState::ensure_loginable`]。
//!
//! 另外两条容易踩的地方：
//!   - 会话吊销只写 `revoked_at`，**不删 API Token**。鉴权时按账号状态拦，
//!     解封即自动恢复，不用重新签发一遍。
//!   - 登录失败要分两种提示：账号不存在与密码错误给同一个文案（不给撞库者线索），
//!     而账号被暂停/封禁要给明确原因与申诉入口。

use chrono::{Duration, Utc};
use pixora_crypto::password;
use pixora_crypto::token as crypto_token;
use pixora_repo::model::{now, role, status, User};
use pixora_repo::repo::users::NewUser;
use pixora_repo::repo::verifications::purpose;

use crate::antispam;
use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::mail::MailDelivery;
use crate::AppState;

/// 连续登录失败多少次锁定。
const LOGIN_FAIL_LIMIT: i32 = 5;
/// 锁定多久（分钟）。
const LOGIN_LOCK_MINUTES: i64 = 15;
/// 验证码有效期（分钟）。
const CODE_TTL_MINUTES: i64 = 10;

/// 发码的有效期（分钟）。管理员代为发重置邮件时要用同一个值——
/// 两处各写一个数字，迟早会出现「邮件说 10 分钟、校验按 5 分钟算」。
pub fn code_ttl_minutes() -> i64 {
    CODE_TTL_MINUTES
}

/// 重置链接的有效期（分钟）。
///
/// 比验证码宽：链接常常要经过转发、等用户回来才点，10 分钟太紧；而链接是
/// 32 字节随机数，猜不出来，放长一点不削弱安全性。
pub const RESET_LINK_TTL_MINUTES: i64 = 60;

/// `user@example.com` → `u***@e***.com`。
///
/// 重置页上只要让人认出「这是我那个账号」，不必把完整地址摊在屏幕上。
fn mask_email(email: &str) -> String {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    let local_head = local.chars().next().unwrap_or('*');
    let domain_head = domain.chars().next().unwrap_or('*');
    let suffix = match domain.rsplit_once('.') {
        Some((_, tld)) => format!(".{tld}"),
        None => String::new(),
    };
    format!("{local_head}***@{domain_head}***{suffix}")
}
/// 验证码最多试几次。
const CODE_MAX_ATTEMPTS: i32 = 5;
/// 同一邮箱每小时最多发几次验证码。
const CODE_PER_EMAIL_PER_HOUR: i64 = 5;
/// 同一 IP 每小时最多发几次验证码。
const CODE_PER_IP_PER_HOUR: i64 = 20;
/// 重发冷却（秒），界面上的「重新发送 (58s)」按它倒计时。
const RESEND_COOLDOWN_SECONDS: i64 = 60;
/// 恢复码个数。
const RECOVERY_CODE_COUNT: usize = 8;

/// 请求来源的设备与环境信息。会话表要留痕，安全页要展示。
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl DeviceInfo {
    /// 把 `User-Agent` 压成「Chrome · Windows」这样的人话。
    pub fn device_label(&self) -> Option<String> {
        let agent = self.user_agent.as_deref()?.trim();
        if agent.is_empty() {
            return None;
        }
        Some(format!(
            "{} · {}",
            browser_of(agent).unwrap_or("未知客户端"),
            platform_of(agent).unwrap_or("未知系统")
        ))
    }
}

fn browser_of(agent: &str) -> Option<&'static str> {
    // 顺序有讲究：Edge 的 UA 里同时含 Chrome 与 Safari，Chrome 又含 Safari
    if agent.contains("Edg/") {
        Some("Edge")
    } else if agent.contains("OPR/") || agent.contains("Opera") {
        Some("Opera")
    } else if agent.contains("Firefox/") {
        Some("Firefox")
    } else if agent.contains("Chrome/") {
        Some("Chrome")
    } else if agent.contains("Safari/") {
        Some("Safari")
    } else if agent.contains("PicGo") || agent.contains("PicList") {
        Some("PicGo")
    } else if agent.contains("curl") {
        Some("curl")
    } else {
        None
    }
}

fn platform_of(agent: &str) -> Option<&'static str> {
    if agent.contains("iPhone") || agent.contains("iPad") {
        Some("iOS")
    } else if agent.contains("Android") {
        Some("Android")
    } else if agent.contains("Macintosh") || agent.contains("Mac OS X") {
        Some("macOS")
    } else if agent.contains("Windows") {
        Some("Windows")
    } else if agent.contains("Linux") {
        Some("Linux")
    } else {
        None
    }
}

/// 登录或注册成功后的令牌。
#[derive(Debug, Clone)]
pub struct IssuedSession {
    pub user: User,
    pub session_token: String,
    pub csrf_token: String,
}

/// 邮箱在「登录方式」里的名字。它不在 `user_identities` 表里——邮箱是账号本身
/// （`users.email`），必需且不可解绑，列在这里只为让界面与接口有个统一的叫法。
pub const PASSWORD_IDENTITY: &str = "email";

/// 登录第二步用哪种二次验证码。
///
/// 动态码、邮箱验证码、恢复码……都是同一串 6 位（或 8 位）字符，服务端分不清，
/// 所以由客户端说明这次填的是哪种；不说就按默认的动态码验。
pub mod code_method {
    pub const TOTP: &str = "totp";
    pub const EMAIL: &str = "email";
}

/// 重置链接换来的信息（重置页用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResetLinkInfo {
    /// 打了码的邮箱：`u***@e***.com`
    pub email: String,
    pub expires_at: String,
}

/// 发码结果。
#[derive(Debug, Clone)]
pub struct CodeSent {
    pub expires_in: i64,
    pub resend_after: i64,
    /// 开发环境（没有 SMTP）下把码带出来，正式环境恒为 None
    pub debug_code: Option<String>,
}

/// 注册结果。
#[derive(Debug, Clone)]
pub struct RegisterOutcome {
    pub user: User,
    pub default_album_id: i64,
}

/// 安全页展示用的会话行。
#[derive(Debug, Clone)]
pub struct SessionView {
    pub id: i64,
    pub device: Option<String>,
    pub ip: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub last_seen_at: chrono::DateTime<Utc>,
    pub current: bool,
}

/// 二次验证的密钥。
#[derive(Debug, Clone)]
pub struct TotpSetup {
    pub secret: String,
    pub otpauth_url: String,
}

impl AppState {
    // ============================================================
    // 注册
    // ============================================================

    /// 发注册验证码。三层反临时邮箱 + 频率限制都在这里。
    pub async fn send_register_code(&self, email: &str, device: &DeviceInfo) -> Result<CodeSent> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::REGISTER)
            .await?;
        self.ensure_registration_open().await?;

        let email = normalize_email_input(email)?;
        // 白名单排在最前面：它对用户来说是「本站收不收这个邮箱」的直接答案，
        // 比后面那些「域名解析不出来」的口径清楚得多
        self.ensure_register_domain_allowed(&email).await?;
        antispam::check_email_allowed(self, &email).await?;
        // 发信这一关先过：这里不涉及「邮箱注册过没有」，如实报错不泄露什么
        self.ensure_mail_ready()?;

        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已被注册".into()));
        }
        if self.repo.email_banned(&email).await? {
            return Err(AppError::AccountBanned {
                reason: Some("该邮箱曾被封禁，无法重新注册".into()),
            });
        }

        self.ensure_verification_quota(&email, device.ip.as_deref(), purpose::REGISTER)
            .await?;

        let code = crypto_token::new_numeric_code(6)?;
        let code_hash = crypto_token::token_hash(&code);
        let expires_at = now() + chrono::Duration::minutes(CODE_TTL_MINUTES);
        self.repo
            .create_email_verification(
                &email,
                &code_hash,
                purpose::REGISTER,
                expires_at,
                device.ip.as_deref(),
            )
            .await?;

        let delivery = self
            .dispatch_mail(
                &email,
                pixora_mail::template::names::VERIFY_CODE,
                serde_json::json!({ "email": email, "code": code }),
            )
            .await?;
        Self::ensure_delivered(delivery)?;

        Ok(CodeSent {
            expires_in: CODE_TTL_MINUTES * 60,
            resend_after: RESEND_COOLDOWN_SECONDS,
            debug_code: self.debug_code(&code).await,
        })
    }

    /// 注册。验证码校验通过后账号直接激活（邮箱已经验证过了）。
    pub async fn register(
        &self,
        email: &str,
        code: &str,
        password_plain: &str,
        device: &DeviceInfo,
    ) -> Result<RegisterOutcome> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::REGISTER)
            .await?;
        self.ensure_registration_open().await?;

        let email = normalize_email_input(email)?;
        password::check_strength(password_plain)?;
        // 发码时已经查过一次，这里再查一次：中间这一小段时间里白名单可能被改过，
        // 而这一关决定「谁能进来」
        self.ensure_register_domain_allowed(&email).await?;

        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已被注册".into()));
        }
        if self.repo.email_banned(&email).await? {
            return Err(AppError::AccountBanned {
                reason: Some("该邮箱曾被封禁，无法重新注册".into()),
            });
        }

        self.consume_code(&email, purpose::REGISTER, code).await?;

        let password_hash = password::hash_password(password_plain)?;
        let (default_quota, _) = self.settings.quota_values().await?;

        let user = self
            .create_user_with_uid(NewUser {
                uid: String::new(),
                email: email.clone(),
                password_hash,
                role: role::USER.to_string(),
                status: status::ACTIVE.to_string(),
                quota_bytes: default_quota,
                email_verified: true,
            })
            .await?;

        let album = self
            .repo
            .insert_default_album(user.id, "默认相册")
            .await?;

        self.audit_detail(
            actions::REGISTER,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "email": email }),
            device.ip.as_deref(),
        )
        .await;

        Ok(RegisterOutcome {
            user,
            default_album_id: album.id,
        })
    }

    /// 找回密码发码。账号不存在也返回成功，不给「这个邮箱有没有注册」的探针。
    pub async fn send_password_reset_code(
        &self,
        email: &str,
        device: &DeviceInfo,
    ) -> Result<CodeSent> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let email = normalize_email_input(email)?;
        // 发信能力在查账号之前判：这一句对所有人一样，不构成「注册过没有」的探针
        self.ensure_mail_ready()?;
        // 地址收不收得到信，同样与账号无关，放这个位置不泄露任何东西
        antispam::check_email_allowed(self, &email).await?;

        if !self.repo.email_exists(&email).await? {
            // 静默成功：前端提示「如果该邮箱已注册，验证码已发出」
            return Ok(CodeSent {
                expires_in: CODE_TTL_MINUTES * 60,
                resend_after: RESEND_COOLDOWN_SECONDS,
                debug_code: None,
            });
        }

        self.ensure_verification_quota(&email, device.ip.as_deref(), purpose::RESET)
            .await?;

        let code = crypto_token::new_numeric_code(6)?;
        let code_hash = crypto_token::token_hash(&code);
        let expires_at = now() + chrono::Duration::minutes(CODE_TTL_MINUTES);
        self.repo
            .create_email_verification(
                &email,
                &code_hash,
                purpose::RESET,
                expires_at,
                device.ip.as_deref(),
            )
            .await?;

        // 到这里账号一定存在，所以投递失败可以照实说：注册接口的
        // CONFLICT 本来就能问出「这个邮箱注册过没有」，这里再藏没有意义，
        // 而「一直显示已发送、信永远不来」的代价大得多
        let delivery = self.send_reset_mail(&email, &code).await?;
        Self::ensure_delivered(delivery)?;

        Ok(CodeSent {
            expires_in: CODE_TTL_MINUTES * 60,
            resend_after: RESEND_COOLDOWN_SECONDS,
            debug_code: self.debug_code(&code).await,
        })
    }

    /// 发一封带验证码与重置链接的邮件。返回真实投递结果。
    ///
    /// 自助找回密码与管理员代发共用这一处：两条路发出去的邮件要一模一样，
    /// 否则「我点的没收到 / 他自己点的收到了」这种事查起来毫无头绪。
    pub async fn send_reset_mail(&self, email: &str, code: &str) -> Result<MailDelivery> {
        let link = self.issue_reset_link(email).await?;
        self.dispatch_mail(
            email,
            pixora_mail::template::names::RESET_PASSWORD,
            serde_json::json!({ "email": email, "code": code, "link": link }),
        )
        .await
    }

    /// 重置密码。成功后吊销该用户全部会话——密码都泄露了，旧会话不能留着。
    pub async fn reset_password(
        &self,
        email: &str,
        code: &str,
        password_plain: &str,
        device: &DeviceInfo,
    ) -> Result<()> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let email = normalize_email_input(email)?;
        password::check_strength(password_plain)?;

        self.consume_code(&email, purpose::RESET, code).await?;

        let Some(user) = self.repo.find_user_by_email(&email).await? else {
            // 码都消费掉了却查不到账号：只可能是管理员在发码之后删了账号
            return Err(AppError::bad_request("验证码无效或已过期"));
        };

        self.apply_new_password(&user, password_plain, device, "code")
            .await?;
        Ok(())
    }

    // ============================================================
    // 登录与登出
    // ============================================================

    pub async fn login(
        &self,
        email: &str,
        password_plain: &str,
        totp_code: Option<&str>,
        recovery_code: Option<&str>,
        device: &DeviceInfo,
        remember: bool,
    ) -> Result<IssuedSession> {
        self.login_with_method(
            email,
            password_plain,
            code_method::TOTP,
            totp_code,
            recovery_code,
            device,
            remember,
        )
        .await
    }

    /// 带「用哪种二次验证」的登录。
    ///
    /// 两种码都是 6 位数字，服务端分不清，所以由客户端说明：`email` 表示这次
    /// 填的是邮箱验证码，其余一律按动态码验（`recovery_code` 仍然单独走）。
    pub async fn login_with_method(
        &self,
        email: &str,
        password_plain: &str,
        method: &str,
        totp_code: Option<&str>,
        recovery_code: Option<&str>,
        device: &DeviceInfo,
        remember: bool,
    ) -> Result<IssuedSession> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let email = normalize_email_input(email)?;

        let Some(user) = self.repo.find_user_by_email(&email).await? else {
            // 账号不存在与密码错误给同一个错误：不给撞库者任何线索
            self.register_ip_failure(device.ip.as_deref(), actions::LOGIN_FAIL)
                .await;
            return Err(AppError::InvalidCredentials);
        };

        // 访客账号不接受登录。它是游客上传的归属地（见 guest.rs），不是给人用的
        // 账号：放行一次，进去的人看到的就全是别人的游客上传。这道判断放在密码校验
        // 之前，是因为那个账号根本没有可用口令，让校验器去碰它只会得到一句内部错误
        crate::guest::ensure_not_guest(&user)?;

        if user.is_locked(Utc::now()) {
            return Err(AppError::AccountLocked {
                locked_until: user.locked_until.map(|at| at.to_rfc3339()),
            });
        }

        if !password::verify_password(password_plain, &user.password_hash)? {
            let failures = user.login_fail_count + 1;
            let lock_until = if failures >= LOGIN_FAIL_LIMIT {
                Some(Utc::now() + chrono::Duration::minutes(LOGIN_LOCK_MINUTES))
            } else {
                None
            };
            self.repo
                .record_login_failure(user.id, failures, lock_until)
                .await?;
            self.audit_detail(
                actions::LOGIN_FAIL,
                Some(user.id),
                &user.uid,
                serde_json::json!({ "fail_count": failures }),
                device.ip.as_deref(),
            )
            .await;
            self.register_ip_failure(device.ip.as_deref(), actions::LOGIN_FAIL)
                .await;
            return Err(AppError::InvalidCredentials);
        }

        if user.totp_enabled {
            let code = totp_code.map(str::trim).filter(|value| !value.is_empty());
            let recovery = recovery_code
                .map(str::trim)
                .filter(|value| !value.is_empty());

            if method == code_method::EMAIL {
                // 用邮箱验证码替代动态码：验证器不在手边时的退路。邮箱是注册时
                // 就验过的，信任级别与「找回密码」一致；两种码都是 6 位数字，
                // 服务端分不清，所以由客户端说明用的是哪种
                let Some(code) = code else {
                    return Err(AppError::TotpRequired);
                };
                self.consume_code(&user.email, purpose::LOGIN_CODE, code)
                    .await?;
            } else {
                match (code, recovery) {
                    (None, None) => return Err(AppError::TotpRequired),
                        (Some(code), _) => {
                        let secret = user
                            .totp_secret
                            .clone()
                            .ok_or_else(|| AppError::internal("账号开启了二次验证但密钥缺失"))?;
                        if !pixora_crypto::totp::verify(&secret, code, 1)? {
                            self.audit_detail(
                                actions::LOGIN_FAIL,
                                Some(user.id),
                                &user.uid,
                                serde_json::json!({ "reason": "totp_mismatch" }),
                                device.ip.as_deref(),
                            )
                            .await;
                            return Err(AppError::TotpInvalid("验证码不匹配或已过期".into()));
                        }
                    }
                    (None, Some(code)) => {
                        self.consume_recovery_code(&user, code).await?;
                    }
                }
            }
        }

        // 状态检查放在密码验证之后：账号是否存在、口令是否正确这类信息不该
        // 因为状态不同而泄露给未通过验证的人
        //
        // 先看内容审核的自动暂停是否已经到期：到期就放行。少了这一步，
        // 被自动暂停的人如果一直没登录，暂停到期后仍然进不来（没有别的入口
        // 去触发那次恢复）
        let user = match self.lift_expired_suspension(&user).await {
            Some(fresh) => fresh,
            None => user,
        };
        self.ensure_loginable(&user)?;

        self.repo
            .record_login_success(user.id, device.ip.as_deref())
            .await?;
        let fresh = self
            .repo
            .find_user_by_id(user.id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;

        let issued = self.issue_session(&fresh, device, remember).await?;
        self.audit_action(actions::LOGIN_SUCCESS, Some(user.id), device.ip.as_deref())
            .await;
        Ok(issued)
    }

    pub async fn logout(&self, session_token: &str, device: &DeviceInfo) -> Result<()> {
        let hash = crypto_token::token_hash(session_token);
        if let Some(session) = self.repo.find_session_by_hash(&hash).await? {
            self.repo
                .revoke_session(session.id, session.user_id, Utc::now())
                .await?;
            self.audit_action(actions::LOGOUT, Some(session.user_id), device.ip.as_deref())
                .await;
        }
        Ok(())
    }

    /// 签发会话：建会话行 + 生成 CSRF 令牌。
    ///
    /// `remember` 为真时写死绝对到期时间（设置里的 `session.remember_days`），
    /// 之后不再按闲置时长判——选了「记住我」就该隔几天打开还是登录状态。
    pub(crate) async fn issue_session(
        &self,
        user: &User,
        device: &DeviceInfo,
        remember: bool,
    ) -> Result<IssuedSession> {
        let session_token = crypto_token::new_session_token()?;
        let hash = crypto_token::token_hash(&session_token);
        let expires_at = if remember {
            let session = self.settings.session().await?;
            Some(Utc::now() + chrono::Duration::days(session.remember_days.max(1)))
        } else {
            None
        };
        self.repo
            .create_session(
                user.id,
                &hash,
                device.device_label().as_deref(),
                device.ip.as_deref(),
                device.user_agent.as_deref(),
                expires_at,
            )
            .await?;
        let csrf_token = crypto_token::new_csrf_token()?;
        Ok(IssuedSession {
            user: user.clone(),
            session_token,
            csrf_token,
        })
    }

    /// 会话有效期的当前策略。设置读不出来时用保守默认（闲置两小时、最长 30 天），
    /// 不让一次设置读取失败变成「谁都能永久登录」。
    pub async fn session_policy(&self) -> pixora_repo::model::SessionPolicy {
        match self.settings.session().await {
            Ok(session) => session.policy(),
            Err(err) => {
                tracing::warn!(error = %err, "读取会话设置失败，按默认有效期处理");
                pixora_repo::model::SessionPolicy::fallback()
            }
        }
    }

    // ============================================================
    // 鉴权入口（只有这两个）
    // ============================================================

    /// 会话鉴权。返回 (用户, 会话)。
    ///
    /// 每次请求都刷新 `last_seen_at`，顺带更新 IP（换网了）。状态判断统一走
    /// [`Self::ensure_loginable`]。
    ///
    /// 过期判定用设置里的策略（闲置超时 + 最长寿命）；判到过期就地吊销这一行，
    /// 于是「活跃会话」列表里也就自然没有它了。
    pub async fn authenticate_session(
        &self,
        session_token: &str,
        ip: Option<&str>,
    ) -> Result<(User, pixora_repo::model::Session)> {
        let hash = crypto_token::token_hash(session_token);
        let Some(session) = self.repo.find_session_by_hash(&hash).await? else {
            return Err(AppError::Unauthorized);
        };
        if !session.is_live() {
            return Err(AppError::Unauthorized);
        }
        let policy = self.session_policy().await;
        if session.is_expired(policy, Utc::now()) {
            // 过期就顺手吊销：列表按同一条尺子过滤，不再显示已经掉线的会话
            let _ = self
                .repo
                .revoke_session(session.id, session.user_id, Utc::now())
                .await;
            return Err(AppError::Unauthorized);
        }

        let Some(mut user) = self.repo.find_user_by_id(session.user_id).await? else {
            return Err(AppError::Unauthorized);
        };
        // 内容审核的自动暂停到期就放人：用户下次访问就是恢复的时刻
        if let Some(fresh) = self.lift_expired_suspension(&user).await {
            user = fresh;
        }

        // 被暂停或封禁的账号：会话立即失效（管理员处置时也会批量吊销，
        // 这里再判一次是防「吊销之前就已经建立的请求」）
        self.ensure_loginable(&user)?;

        if let Err(err) = self.repo.touch_session(session.id, ip, Utc::now()).await {
            tracing::warn!(error = %err, "刷新会话活动时间失败");
        }
        Ok((user, session))
    }

    /// API Token 鉴权。返回 (用户, 令牌)。
    pub async fn authenticate_token(
        &self,
        token_plain: &str,
    ) -> Result<(User, pixora_repo::model::ApiToken)> {
        let hash = crypto_token::token_hash(token_plain);
        let Some(record) = self.repo.find_api_token_by_hash(&hash).await? else {
            return Err(AppError::TokenRevoked);
        };
        if !record.is_usable(Utc::now()) {
            return Err(AppError::TokenRevoked);
        }
        let Some(mut user) = self.repo.find_user_by_id(record.user_id).await? else {
            return Err(AppError::TokenRevoked);
        };
        // 自动暂停到期就放人（管理员手工封禁没有到期时间，不受影响）
        if let Some(fresh) = self.lift_expired_suspension(&user).await {
            user = fresh;
        }

        // Token 不删，靠这里的状态判断实现「暂停即失效、解封即恢复」
        self.ensure_loginable(&user)?;

        if let Err(err) = self.repo.touch_api_token(record.id).await {
            tracing::warn!(error = %err, "更新 Token 最后使用时间失败");
        }
        Ok((user, record))
    }

    /// 账号状态的唯一判断处。只有 `active` 能登录与调用 API。
    pub fn ensure_loginable(&self, user: &User) -> Result<()> {
        match user.status.as_str() {
            status::ACTIVE => Ok(()),
            status::PENDING => Err(AppError::AccountPending),
            status::SUSPENDED => Err(AppError::AccountSuspended {
                reason: user.status_reason.clone(),
                suspended_at: user.status_changed_at.map(|at| at.to_rfc3339()),
            }),
            status::BANNED => Err(AppError::AccountBanned {
                reason: user.status_reason.clone(),
            }),
            other => Err(AppError::forbidden(format!("账号状态异常：{other}"))),
        }
    }

    // ============================================================
    // 会话管理
    // ============================================================

    pub async fn list_sessions(
        &self,
        user_id: i64,
        current_session_id: i64,
    ) -> Result<Vec<SessionView>> {
        let policy = self.session_policy().await;
        let now = Utc::now();
        // 过期的会话先吊销掉，再把剩下的列出来。
        // 「活跃会话」只该有真正还能用的那些：长期未活动、或者「记住我」已到期的
        // 行还挂在列表里，用户会以为自己没退出过。
        let idle_before = if policy.idle_minutes > 0 {
            Some(now - chrono::Duration::minutes(policy.idle_minutes))
        } else {
            None
        };
        let created_before = if policy.absolute_days > 0 {
            Some(now - chrono::Duration::days(policy.absolute_days))
        } else {
            None
        };
        if let Err(err) = self
            .repo
            .revoke_expired_sessions(user_id, now, idle_before, created_before)
            .await
        {
            tracing::warn!(error = %err, "吊销过期会话失败，列表仍按策略过滤");
        }

        let sessions = self.repo.list_sessions(user_id).await?;
        Ok(sessions
            .into_iter()
            .filter(|session| !session.is_expired(policy, now))
            .map(|session| SessionView {
                current: session.id == current_session_id,
                device: session.device.clone(),
                ip: session.ip.clone(),
                created_at: session.created_at,
                last_seen_at: session.last_seen_at,
                id: session.id,
            })
            .collect())
    }

    pub async fn revoke_session(
        &self,
        user_id: i64,
        session_id: i64,
        device: &DeviceInfo,
    ) -> Result<u64> {
        let revoked = self
            .repo
            .revoke_session(session_id, user_id, Utc::now())
            .await?;
        self.audit_detail(
            actions::LOGOUT,
            Some(user_id),
            format!("session:{session_id}"),
            serde_json::json!({ "revoked": revoked }),
            device.ip.as_deref(),
        )
        .await;
        Ok(revoked)
    }

    pub async fn revoke_other_sessions(
        &self,
        user_id: i64,
        keep_session_id: i64,
        device: &DeviceInfo,
    ) -> Result<u64> {
        let revoked = self
            .repo
            .revoke_other_sessions(user_id, Some(keep_session_id), Utc::now())
            .await?;
        self.audit_detail(
            actions::LOGOUT,
            Some(user_id),
            "sessions:others",
            serde_json::json!({ "revoked": revoked }),
            device.ip.as_deref(),
        )
        .await;
        Ok(revoked)
    }

    // ============================================================
    // 免登录切换用户（管理员）
    // ============================================================

    /// 管理员切换到目标用户的会话。
    ///
    /// 返回目标用户的会话令牌与 CSRF；调用方负责把 Cookie 换成它，并把管理员
    /// 自己的令牌塞进 `pixora_impersonator` Cookie。刷新之后还能切回来，
    /// 靠的正是那个 Cookie——它是「免登录切换」成立的前提，所以必须传原件。
    pub async fn impersonate_start(
        &self,
        admin: &User,
        admin_session_token: &str,
        target_user_id: i64,
        device: &DeviceInfo,
    ) -> Result<IssuedSession> {
        if !admin.is_admin() {
            return Err(AppError::forbidden("只有管理员可以使用免登录切换"));
        }
        // 先证「能切回来」：管理员自己的会话必须是活的，否则切过去就回不来了
        let hash = crypto_token::token_hash(admin_session_token);
        let Some(session) = self.repo.find_session_by_hash(&hash).await? else {
            return Err(AppError::bad_request(
                "当前会话已失效，请重新登录后再切换账号",
            ));
        };
        if session.user_id != admin.id {
            return Err(AppError::forbidden("会话与当前管理员对不上"));
        }

        let Some(target) = self.repo.find_user_by_id(target_user_id).await? else {
            return Err(AppError::not_found("用户"));
        };
        if target.id == admin.id {
            return Err(AppError::bad_request("目标是当前登录的账号，无需切换"));
        }
        // 安装账号不给别人切进去用：它是这套部署的主钥匙
        self.ensure_not_install_admin(admin, &target).await?;
        // 访客账号也不给切：免登录切换发出来的就是一张登录凭据，而那个账号
        // 不接受任何形式的登录（见 guest.rs）。它名下是全站游客的上传，
        // 想看内容走「用户视图」，不必借它的身份
        crate::guest::ensure_not_guest(&target)?;
        if target.is_banned() {
            // 封禁账号连管理员也不该切进去：切进去什么也做不了，
            // 只会把「封禁」这件事看起来像没生效
            return Err(AppError::forbidden("该账号已被封禁，无法切换登录"));
        }

        let issued = self.issue_session(&target, device, false).await?;
        self.audit_detail(
            actions::IMPERSONATE_START,
            Some(admin.id),
            &target.uid,
            serde_json::json!({
                "target_email": target.email,
                "target_role": target.role,
                "target_status": target.status,
            }),
            device.ip.as_deref(),
        )
        .await;
        Ok(issued)
    }

    /// 解析「切回管理员」用的令牌。校验它对应的会话仍然有效、且属于活跃管理员。
    pub async fn impersonator_user(&self, token: &str) -> Result<User> {
        let hash = crypto_token::token_hash(token);
        let Some(session) = self.repo.find_session_by_hash(&hash).await? else {
            return Err(AppError::Unauthorized);
        };
        let policy = self.session_policy().await;
        if !session.is_usable(policy, Utc::now()) {
            return Err(AppError::Unauthorized);
        }
        let Some(user) = self.repo.find_user_by_id(session.user_id).await? else {
            return Err(AppError::Unauthorized);
        };
        if !user.is_admin() {
            return Err(AppError::forbidden("只有管理员的会话能作为切换来源"));
        }
        self.ensure_loginable(&user)?;
        Ok(user)
    }

    // ============================================================
    // 口令与二次验证
    // ============================================================

    pub async fn change_password(
        &self,
        user: &User,
        current: &str,
        next: &str,
        keep_session_id: Option<i64>,
        device: &DeviceInfo,
    ) -> Result<u64> {
        if !password::verify_password(current, &user.password_hash)? {
            return Err(AppError::bad_request("当前密码不正确"));
        }
        password::check_strength(next)?;
        let hash = password::hash_password(next)?;
        self.repo.update_user_password(user.id, &hash).await?;
        // 用户自己选的密码，从此不必再被拦在改密码页
        self.repo.set_must_change_password(user.id, false).await?;

        // 改完密码把其它设备踢下线，当前会话留着（否则用户当场被自己踢出去就懵了）
        let revoked = match keep_session_id {
            Some(keep) => {
                self.repo
                    .revoke_other_sessions(user.id, Some(keep), Utc::now())
                    .await?
            }
            None => self.repo.revoke_all_sessions(user.id, Utc::now()).await?,
        };

        self.audit_detail(
            actions::PASSWORD_RESET,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "revoked_sessions": revoked, "self_change": true }),
            device.ip.as_deref(),
        )
        .await;
        Ok(revoked)
    }

    /// 二次验证第一步：生成密钥。此时还没启用，用户扫码后要再提交一次动态码。
    pub async fn totp_setup(&self, user: &User) -> Result<TotpSetup> {
        let secret = pixora_crypto::totp::generate_secret()?;
        let site = self.settings.site(self.config.base_url.as_deref()).await?;
        let url = pixora_crypto::totp::otpauth_url(&secret, &site.name, &user.email);
        // 密钥先存下但不启用：enable 那一步才把它打开
        self.repo.set_totp(user.id, Some(&secret), false).await?;
        Ok(TotpSetup {
            secret,
            otpauth_url: url,
        })
    }

    /// 开启二次验证：动态码对上才算真的开。
    ///
    /// 顺带发一组恢复码带回去——验证器丢了的时候这是唯一的自救凭据，
    /// 而「刚开完」这一刻是用户最愿意把它存下来的时候。明文只出现在这次
    /// 返回值里，库里只存哈希（见 `regenerate_recovery_codes`）。
    pub async fn totp_enable(
        &self,
        user: &User,
        code: &str,
        device: &DeviceInfo,
    ) -> Result<Vec<String>> {
        let secret = user
            .totp_secret
            .clone()
            .ok_or_else(|| AppError::bad_request("请先生成密钥再提交动态码"))?;
        if !pixora_crypto::totp::verify(&secret, code, 1)? {
            return Err(AppError::TotpInvalid("动态验证码不正确".into()));
        }
        self.repo.set_totp(user.id, Some(&secret), true).await?;
        let codes = self.regenerate_recovery_codes(user).await?;
        self.audit_action(actions::SETTINGS_CHANGE, Some(user.id), device.ip.as_deref())
            .await;
        Ok(codes)
    }

    pub async fn totp_disable(&self, user: &User, code: &str, device: &DeviceInfo) -> Result<()> {
        let secret = user
            .totp_secret
            .clone()
            .ok_or_else(|| AppError::bad_request("账号尚未开启二次验证"))?;
        let code = code.trim();
        if code.is_empty() || !pixora_crypto::totp::verify(&secret, code, 1)? {
            // 验证器丢了的人走这条路：换手机、应用没导出，TOTP 就再也算不出来。
            // 邮箱是注册时就验过的，用它证明「是本人」——两种码都认，任一对上就放行。
            // 没发过邮箱验证码时给一句能照着做的提示，用户才知道还有这条路
            let pending = self
                .repo
                .find_latest_verification(&user.email, purpose::TOTP_DISABLE)
                .await?
                .map(|record| record.is_usable(Utc::now()))
                .unwrap_or(false);
            if !pending {
                // TotpInvalid 的消息前缀已经是「动态验证码不正确：」，这里只补后半个句子
                return Err(AppError::TotpInvalid(
                    "验证器不可用时，可改用邮箱验证码关闭".into(),
                ));
            }
            self.consume_code(&user.email, purpose::TOTP_DISABLE, code)
                .await?;
        }
        self.repo.set_totp(user.id, None, false).await?;
        // 关掉二次验证时清掉恢复码，避免留下「已经作废但看起来还有效」的凭据
        self.repo.replace_recovery_codes(user.id, &[]).await?;
        self.audit_action(actions::SETTINGS_CHANGE, Some(user.id), device.ip.as_deref())
            .await;
        Ok(())
    }

    /// 登录第二步的邮箱验证码。
    ///
    /// 账号不存在、或没开二次验证时一律静默返回成功，且限流检查排在查账号
    /// 之前：这条路径最容易被拿来试探「这个邮箱注册过没有、开没开二次验证」，
    /// 回答必须和大家一样。码只真的发给该收到的那个人。
    pub async fn send_login_code(&self, email: &str, device: &DeviceInfo) -> Result<()> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let email = normalize_email_input(email)?;
        // 发信能力同样先判：它跟具体账号无关，对所有人一样
        self.ensure_mail_ready()?;
        // 账号自己的地址也要能收信：地址过期、域名没了的时候，与其发一封注定
        // 退回来的信，不如在登录这一步就说清楚（这条与账号是否存在无关）
        self.ensure_deliverable(&email).await?;
        self.ensure_verification_quota(&email, device.ip.as_deref(), purpose::LOGIN_CODE)
            .await?;

        let Some(user) = self.repo.find_user_by_email(&email).await? else {
            return Ok(());
        };
        if !user.totp_enabled {
            return Ok(());
        }

        let code = crypto_token::new_numeric_code(6)?;
        self.repo
            .create_email_verification(
                &email,
                &crypto_token::token_hash(&code),
                purpose::LOGIN_CODE,
                Utc::now() + Duration::minutes(code_ttl_minutes()),
                device.ip.as_deref(),
            )
            .await?;
        // 投递失败在这里不报错（报错就分得出账号存不存在）：留给发件箱重试，
        // 日志里也留了痕
        self.dispatch_mail(
            &email,
            pixora_mail::template::names::VERIFY_CODE,
            serde_json::json!({ "email": email, "code": code }),
        )
        .await?;
        Ok(())
    }

    /// 关闭二次验证前先发一个邮箱验证码。
    ///
    /// 只给已经登录的本人用：拿到会话才谈得上「验证器丢了要自救」，
    /// 而邮箱验证码能证明他是账号主人（邮箱是注册时就验过的）。
    pub async fn send_totp_disable_code(
        &self,
        user: &User,
        device: &DeviceInfo,
    ) -> Result<MailDelivery> {
        // 发信这关先过：发不出去就别留一个用户永远收不到的验证码
        self.ensure_mail_ready()?;
        // 账号自己的地址也要能收信，理由与发送登录码那处相同
        self.ensure_deliverable(&user.email).await?;
        let code = self
            .issue_totp_disable_code(user, device.ip.as_deref())
            .await?;
        let delivery = self
            .dispatch_mail(
                &user.email,
                pixora_mail::template::names::VERIFY_CODE,
                serde_json::json!({ "email": user.email, "code": code }),
            )
            .await?;
        Self::ensure_delivered(delivery)
    }

    /// 落一条「关闭二次验证」的验证码，返回明文（只在这一刻存在）。
    ///
    /// 与发信分开：投递是否成功不影响这条码的有效性判断，测试也能直接
    /// 拿明文走一遍「用它关掉」的路径，不必搭一台 SMTP。
    pub async fn issue_totp_disable_code(
        &self,
        user: &User,
        ip: Option<&str>,
    ) -> Result<String> {
        self.ensure_verification_quota(&user.email, ip, purpose::TOTP_DISABLE)
            .await?;
        let code = crypto_token::new_numeric_code(6)?;
        self.repo
            .create_email_verification(
                &user.email,
                &crypto_token::token_hash(&code),
                purpose::TOTP_DISABLE,
                Utc::now() + Duration::minutes(code_ttl_minutes()),
                ip,
            )
            .await?;
        Ok(code)
    }

    // ============================================================
    // 自助换绑邮箱
    // ============================================================

    /// 换绑邮箱的第一步：把验证码发到**新**地址。
    ///
    /// 要当前密码：邮箱是「找回账号」的那把备用钥匙，光有会话还不够——
    /// 会话被借用的场合不该顺手把账号的邮箱也改走。新地址先收到码并验过，
    /// 才证明这个地址确实由提出更换的人在手上。
    pub async fn send_email_change_code(
        &self,
        user: &User,
        new_email: &str,
        password_plain: &str,
        device: &DeviceInfo,
    ) -> Result<MailDelivery> {
        let new_email = normalize_email_input(new_email)?;
        if !password::verify_password(password_plain, &user.password_hash)? {
            return Err(AppError::forbidden("当前密码不正确"));
        }
        if new_email.eq_ignore_ascii_case(&user.email) {
            return Err(AppError::bad_request("新邮箱与当前邮箱相同，无需更换"));
        }
        // 注册用的域名白名单对换绑同样成立：站点只收这些域名的邮箱
        self.ensure_register_domain_allowed(&new_email).await?;
        if self.repo.email_exists(&new_email).await? {
            return Err(AppError::Conflict("该邮箱已被其他账号使用".into()));
        }
        self.ensure_mail_ready()?;
        // 待换上的地址先验一遍：用户手打一个不存在的域名，这封信注定退回来，
        // 退信全都会落进发信用的那个邮箱
        antispam::check_email_allowed(self, &new_email).await?;
        self.ensure_verification_quota(&new_email, device.ip.as_deref(), purpose::CHANGE_EMAIL)
            .await?;

        let code = crypto_token::new_numeric_code(6)?;
        self.repo
            .create_email_verification(
                &new_email,
                &crypto_token::token_hash(&code),
                purpose::CHANGE_EMAIL,
                Utc::now() + Duration::minutes(code_ttl_minutes()),
                device.ip.as_deref(),
            )
            .await?;
        let delivery = self
            .dispatch_mail(
                &new_email,
                pixora_mail::template::names::VERIFY_CODE,
                serde_json::json!({ "email": new_email, "code": code }),
            )
            .await?;
        Self::ensure_delivered(delivery)
    }

    /// 换绑邮箱的第二步：验码、改地址、通知新旧地址、留档。
    ///
    /// 返回新的地址与两封信的投递结果：界面要如实说「换成了哪个」，
    /// 也要如实说「通知发出去了没有」。
    pub async fn change_own_email(
        &self,
        user: &User,
        new_email: &str,
        code: &str,
        device: &DeviceInfo,
    ) -> Result<(String, MailDelivery)> {
        let new_email = normalize_email_input(new_email)?;
        if new_email.eq_ignore_ascii_case(&user.email) {
            return Err(AppError::bad_request("新邮箱与当前邮箱相同，无需更换"));
        }
        self.consume_code(&new_email, purpose::CHANGE_EMAIL, code)
            .await?;
        // 发码之后可能被别人抢先占了这个地址，落库前再确认一次
        if self.repo.email_exists(&new_email).await? {
            return Err(AppError::Conflict("该邮箱已被其他账号使用".into()));
        }

        let old_email = user.email.clone();
        // 新地址是刚验过码的，标成已验证；旧地址的「已验证」也随之失效
        self.repo.set_email_verified(user.id, &new_email, true).await?;
        self.repo
            .insert_user_action(
                user.id,
                Some(user.id),
                "change_email",
                Some("本人更换邮箱"),
                &serde_json::json!({ "from": old_email, "to": new_email, "via": "self" }),
                false,
                Utc::now(),
            )
            .await?;
        // 「谁在什么时候把哪个账号的邮箱改成了什么」：账号历史里要看得见是本人操作的
        self.audit_detail(
            actions::ACCOUNT_EMAIL_CHANGE,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "from": old_email, "to": new_email, "via": "self", "actor_email": old_email }),
            device.ip.as_deref(),
        )
        .await;

        let updated = self
            .repo
            .find_user_by_id(user.id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        let delivery = self.notify_email_change(&updated, &new_email).await?;
        Ok((new_email, delivery))
    }

    /// 生成恢复码。明文只返回一次，库里存哈希。
    pub async fn regenerate_recovery_codes(&self, user: &User) -> Result<Vec<String>> {
        let mut plain = Vec::with_capacity(RECOVERY_CODE_COUNT);
        let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
        for _ in 0..RECOVERY_CODE_COUNT {
            // 8 位数字分两段显示，用户抄写时不容易串行
            let raw = crypto_token::new_numeric_code(8)?;
            let code = format!("{}-{}", &raw[..4], &raw[4..]);
            hashes.push(crypto_token::token_hash(&normalize_recovery_code(&code)));
            plain.push(code);
        }
        self.repo.replace_recovery_codes(user.id, &hashes).await?;
        Ok(plain)
    }

    pub async fn recovery_code_count(&self, user_id: i64) -> Result<i64> {
        Ok(self.repo.count_unused_recovery_codes(user_id).await?)
    }

    /// 用一个恢复码登录。命中即标记已用（并发下只会成功一次）。
    async fn consume_recovery_code(&self, user: &User, code: &str) -> Result<()> {
        let normalized = normalize_recovery_code(code);
        let candidates = self.repo.list_unused_recovery_codes(user.id).await?;
        let target = crypto_token::token_hash(&normalized);
        for (id, hash) in candidates {
            if crypto_token::constant_time_eq(&hash, &target) {
                let affected = self.repo.consume_recovery_code(id).await?;
                if affected == 0 {
                    return Err(AppError::bad_request("恢复码已被使用"));
                }
                return Ok(());
            }
        }
        Err(AppError::TotpInvalid("恢复码不正确".into()))
    }

    // ============================================================
    // 内部工具
    // ============================================================

    pub async fn ensure_ip_allowed(&self, ip: Option<&str>, scene: &str) -> Result<()> {
        self.ip_guard.ensure_allowed(ip, scene).await
    }

    /// 注册入口是否开放：设置开关 + 容量触顶自动关闭。
    async fn ensure_registration_open(&self) -> Result<()> {
        let (open, reason) = self.register_open().await?;
        if open {
            return Ok(());
        }
        Err(AppError::forbidden(
            reason.unwrap_or_else(|| "当前未开放注册".into()),
        ))
    }

    /// 注册域名白名单。没配就放行，配了只收清单里的域名。
    ///
    /// 只挂在注册的两个入口（发验证码、提交注册）。找回密码不看它：
    /// 域名后来被移出白名单的老账号还得能改密码。
    pub(crate) async fn ensure_register_domain_allowed(&self, email: &str) -> Result<()> {
        let site = self.settings.site(self.config.base_url.as_deref()).await?;
        if site.email_domains.allows(email) {
            return Ok(());
        }
        Err(AppError::forbidden(format!(
            "本站只接受 {} 的邮箱注册",
            site.email_domains.domains().join("、")
        )))
    }

    /// 创建用户并保证 UID 唯一。
    ///
    /// UID 是对外可见的短标识，也是物理目录的一级分类，所以冲突必须重试而不是报错。
    /// 4 位数字空间只有 9000 个，用满之后自动加长到 5 位、6 位。
    pub(crate) async fn create_user_with_uid(&self, mut new: NewUser) -> Result<User> {
        for digits in [4usize, 5, 6] {
            for attempt in 0..12 {
                let uid = crypto_token::new_uid(digits)?;
                if self.repo.uid_exists(&uid).await? {
                    // 前几次冲突就重试；同一长度连撞 12 次说明这个空间快满了，加长
                    if attempt < 11 {
                        continue;
                    }
                    break;
                }
                new.uid = uid;
                match self.repo.create_user(&new).await {
                    Ok(user) => return Ok(user),
                    Err(err) if err.is_unique_violation() => continue,
                    Err(err) => return Err(err.into()),
                }
            }
        }
        Err(AppError::internal("无法分配唯一的 UID，请检查用户数量"))
    }

    /// 校验并消费验证码。
    pub(crate) async fn consume_code(&self, email: &str, purpose_value: &str, code: &str) -> Result<()> {
        let Some(record) = self
            .repo
            .find_latest_verification(email, purpose_value)
            .await?
        else {
            return Err(AppError::bad_request("验证码无效或已过期"));
        };
        if !record.is_usable(Utc::now()) {
            return Err(AppError::bad_request("验证码无效或已过期"));
        }
        if record.attempts >= CODE_MAX_ATTEMPTS {
            return Err(AppError::bad_request("验证码错误次数过多，请重新获取"));
        }

        let target = crypto_token::token_hash(code.trim());
        if !crypto_token::constant_time_eq(&record.code_hash, &target) {
            let attempts = self
                .repo
                .increment_verification_attempts(record.id)
                .await
                .unwrap_or(record.attempts + 1);
            return Err(AppError::bad_request(if attempts >= CODE_MAX_ATTEMPTS {
                "验证码错误次数过多，请重新获取".to_string()
            } else {
                format!(
                    "验证码不正确（还可尝试 {} 次）",
                    CODE_MAX_ATTEMPTS - attempts
                )
            }));
        }

        self.repo.consume_verification(record.id, Utc::now()).await?;
        Ok(())
    }

    /// 发码频率限制：同邮箱每小时 N 次、同 IP 每小时 M 次、以及 60 秒重发冷却。
    async fn ensure_verification_quota(
        &self,
        email: &str,
        ip: Option<&str>,
        purpose_value: &str,
    ) -> Result<()> {
        let hour_ago = Utc::now() - chrono::Duration::hours(1);
        let email_count = self
            .repo
            .count_recent_verifications_for_email(email, purpose_value, hour_ago)
            .await?;
        if email_count >= CODE_PER_EMAIL_PER_HOUR {
            return Err(AppError::RateLimited {
                retry_after: 3600,
                detail: format!("同一邮箱每小时最多发送 {CODE_PER_EMAIL_PER_HOUR} 次验证码"),
            });
        }

        if let Some(ip) = ip {
            let ip_count = self
                .repo
                .count_recent_verifications_for_ip(ip, hour_ago)
                .await?;
            if ip_count >= CODE_PER_IP_PER_HOUR {
                return Err(AppError::RateLimited {
                    retry_after: 3600,
                    detail: format!("同一网络地址每小时最多发送 {CODE_PER_IP_PER_HOUR} 次验证码"),
                });
            }
        }

        // 60 秒冷却：界面上「重新发送 (58s)」的倒计时与这里对齐
        let cooldown_start = Utc::now() - chrono::Duration::seconds(RESEND_COOLDOWN_SECONDS);
        let recent = self
            .repo
            .count_recent_verifications_for_email(email, purpose_value, cooldown_start)
            .await?;
        if recent > 0 {
            return Err(AppError::RateLimited {
                retry_after: RESEND_COOLDOWN_SECONDS as u64,
                detail: format!("请等待 {RESEND_COOLDOWN_SECONDS} 秒后再重新发送"),
            });
        }
        Ok(())
    }

    /// 把邮件写进发件箱并尝试立即投递。投递失败由发件箱的重试机制兜底。
    ///
    /// 公开（`pub`）是因为内容审核的上报、自动暂停通知与管理员发重置邮件
    /// 也要走同一条路径——几处各写一遍渲染与重试，迟早会漂移。
    ///
    /// 返回投递结果而不是「成功/失败」：注册、找回密码那几处对用户只能
    /// 说「已发送」（否则就是「这个邮箱存不存在」的探针），但管理员点的
    /// 那几下必须知道邮件到底出去没有。
    pub async fn dispatch_mail(
        &self,
        to: &str,
        template: &str,
        vars: serde_json::Value,
    ) -> Result<MailDelivery> {
        let outbox_id = self.repo.enqueue_mail(to, template, &vars).await?;

        let Some(mailer) = self.mailer() else {
            tracing::warn!(to, template, "SMTP 未配置，邮件只留在发件箱里");
            return Ok(MailDelivery {
                delivered: false,
                detail: Some("SMTP 未配置，邮件已留在发件箱，配好之后会自动投递".into()),
            });
        };

        let stored = self
            .repo
            .find_email_template(template)
            .await?
            .map(|item| (item.subject, item.body));
        let (subject, body) = pixora_mail::template::resolve(template, stored);

        let mut map = pixora_mail::MailVars::new();
        if let Some(object) = vars.as_object() {
            for (key, value) in object {
                map.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
            }
        }
        let subject = pixora_mail::render_subject(&subject, &map);
        let body = pixora_mail::render(&body, &map);

        match mailer.send(to, &subject, &body).await {
            Ok(()) => {
                self.repo.mark_mail_sent(outbox_id, Utc::now()).await?;
                Ok(MailDelivery {
                    delivered: true,
                    detail: None,
                })
            }
            Err(err) => {
                // 拒收（地址不存在、被对方拉黑）与认证失败是一次性的结论：
                // 不进重试队列，直接把服务器那句话带到界面上
                let permanent = err.is_permanent();
                let next = if permanent {
                    None
                } else {
                    Some(Utc::now() + chrono::Duration::minutes(2))
                };
                self.repo
                    .mark_mail_failed(outbox_id, &err.to_string(), next, permanent)
                    .await?;
                if permanent {
                    tracing::warn!(to, template, error = %err, "邮件被拒收，不再重试");
                } else {
                    tracing::warn!(to, template, error = %err, "邮件投递失败，已进入重试队列");
                }
                Ok(MailDelivery {
                    delivered: false,
                    detail: Some(if permanent {
                        format!("投递失败：{err}")
                    } else {
                        format!("投递失败：{err}。已进入重试队列，配置准确的话几分钟后会自动重发")
                    }),
                })
            }
        }
    }

    /// 站点对外的地址，用来拼邮件里的链接。
    ///
    /// 设置里优先，其次是环境变量；两边都没配就返回 `None`——内网试装时
    /// 常见，那种情况下邮件里只给验证码，不硬塞一个点不开的链接。
    pub async fn public_base_url(&self) -> Result<Option<String>> {
        let site = self.settings.site(self.config.base_url.as_deref()).await?;
        let base = site.base_url.trim().trim_end_matches('/').to_string();
        Ok(if base.is_empty() { None } else { Some(base) })
    }

    /// 邮件服务能不能用。发验证码之前先问一句。
    ///
    /// SMTP 没配时对外说「验证码已发出」是句假话：用户会一直等一封永远不会
    /// 来的信，管理员却看不到任何异常。这一道对所有人一视同仁——不按邮箱
    /// 是否存在分岔，就不构成「这个邮箱注册过没有」的探针。
    fn ensure_mail_ready(&self) -> Result<()> {
        if self.mailer().is_some() {
            return Ok(());
        }
        Err(AppError::Config(
            "发信服务（SMTP）尚未配置，验证码邮件无法发送，请联系管理员".into(),
        ))
    }

    /// 投递失败就如实报错：用户等的是一封信，不是一句「已发送」。
    pub(crate) fn ensure_delivered(delivery: MailDelivery) -> Result<MailDelivery> {
        if delivery.delivered {
            return Ok(delivery);
        }
        Err(AppError::Config(
            delivery
                .detail
                .unwrap_or_else(|| "验证码邮件没能发出去，请稍后重试".into()),
        ))
    }

    /// 生成一条重置链接：一次性令牌，过期时间比验证码宽一些。
    ///
    /// 链接里放的是随机令牌（库里只存哈希），不是邮箱与验证码——那两个是明文
    /// 参数，转发一次链接就等于把「谁的账号 + 当前有效的验证码」一起递了出去。
    /// 站点地址没配时返回空串，邮件里就只剩验证码。
    async fn issue_reset_link(&self, email: &str) -> Result<String> {
        let Some(base) = self.public_base_url().await? else {
            return Ok(String::new());
        };
        let token = crypto_token::new_reset_token()?;
        self.repo
            .create_email_verification(
                email,
                &crypto_token::token_hash(&token),
                purpose::RESET_LINK,
                Utc::now() + Duration::minutes(RESET_LINK_TTL_MINUTES),
                None,
            )
            .await?;
        Ok(format!(
            "点开这个链接直接设置新密码（{RESET_LINK_TTL_MINUTES} 分钟内有效）：\n{base}/#/reset-password?token={token}"
        ))
    }

    /// 凭令牌找出这条重置链接。无效、用过、过期都归到同一句话里。
    async fn find_reset_link(
        &self,
        token: &str,
    ) -> Result<pixora_repo::model::EmailVerification> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AppError::bad_request("链接不完整，请从邮件里重新打开"));
        }
        let hash = crypto_token::token_hash(token);
        let Some(record) = self
            .repo
            .find_verification_by_hash(purpose::RESET_LINK, &hash)
            .await?
        else {
            return Err(AppError::bad_request(
                "这个链接已经用过或已失效，请重新发一封重置邮件",
            ));
        };
        if !record.is_usable(Utc::now()) {
            return Err(AppError::bad_request(
                "这个链接已经过期，请重新发一封重置邮件",
            ));
        }
        Ok(record)
    }

    /// 头像地址归一：站内直链折成相对路径，外链原样存。
    ///
    /// 站点基础地址是会变的（换域名、加反代前缀），存绝对地址的话改一次配置
    /// 满库头像全裂。外链没法折，只能原样存——那种裂了也不归我们管。
    pub async fn normalize_avatar(&self, input: &str) -> Result<Option<String>> {
        let value = input.trim();
        if value.is_empty() {
            return Ok(None);
        }
        if value.len() > 500 {
            return Err(AppError::bad_request("头像地址太长了"));
        }
        if let Some(base) = self.public_base_url().await? {
            if let Some(rest) = value.strip_prefix(&base) {
                // 只有真正落在本域名下的路径才折：`base` 恰好是别人域名前缀
                // （https://img.example.com 与 https://img.example.com.evil.net）
                // 时 rest 不以 / 开头，会落到下面的外链分支
                let path = rest.trim();
                if path.starts_with('/') {
                    return Ok(Some(path.to_string()));
                }
            }
        }
        if value.starts_with('/') || value.starts_with("http://") || value.starts_with("https://") {
            return Ok(Some(value.to_string()));
        }
        Err(AppError::bad_request(
            "头像地址需以 http(s):// 或 / 开头；上传的图片直接使用上传结果中的直链即可",
        ))
    }

    /// 换头像之后，把不再当头像的那张本地图片收掉。
    ///
    /// 上传头像的流程是「传一张图 → 打上 `avatar` 标签 → 把它的直链存进用户行」。
    /// 换成别的头像（换成外链、或干脆移除）之后，那张图就只剩在图库里占地方了，
    /// 而用户并不记得自己传过它。
    ///
    /// 三条边界：
    ///   - 只动**自己**名下、带 `avatar` 标记的那张：手动把相册里某张照片的直链
    ///     填成头像的人，换头像不该被删掉照片；
    ///   - 同一份内容还有别的引用（比如那张图也在别的相册里存着一份）时，只删这条
    ///     引用，物理文件留给另一条——引用计数归零才回收，与「彻底删除」同一条路；
    ///   - 失败只记警告：头像是主要目的，清理是顺手做的事，不该让换头像失败。
    pub async fn release_avatar_image(&self, user: &User, avatar: &str) -> Option<i64> {
        let (sha, _ext) = parse_direct_link(avatar)?;
        let blob = match self
            .repo
            .find_blob_by_sha(&sha, pixora_repo::model::storage_kind::ORIGINAL)
            .await
        {
            Ok(Some(blob)) => blob,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(error = %err, "找头像实体失败，跳过清理");
                return None;
            }
        };
        let image = match self.repo.find_avatar_image(user.id, blob.id).await {
            Ok(Some(image)) => image,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(error = %err, "找头像图片失败，跳过清理");
                return None;
            }
        };
        // 两步走：先软删（引用计数在这一步减一），再清掉墓碑。不留在回收站里——
        // 这张图是「设头像」的副产品，用户并没有打算把它当图片留着。
        // `purge_image_row` 只认墓碑行，少走第一步的话什么也不会发生
        let deletion = pixora_repo::repo::images::ImageDeletion {
            image_id: image.id,
            owner_id: image.owner_id,
            album_id: image.album_id,
            blob_id: image.blob_id,
            thumb_blob_id: image.thumb_blob_id,
            size: blob.size,
            by_album: false,
        };
        if let Err(err) = self
            .repo
            .delete_image_reference(&deletion, chrono::Utc::now())
            .await
        {
            tracing::warn!(image_id = image.id, error = %err, "头像图片软删失败");
            return None;
        }
        match self.repo.purge_image_row(image.id).await {
            Ok(Some(targets)) => {
                let reclaimed = self.reclaim_purged(vec![targets]).await;
                self.kick_gc();
                tracing::info!(
                    image_id = image.id,
                    reclaimed_files = reclaimed,
                    "换头像后已清理旧的头像图片"
                );
                Some(image.id)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(image_id = image.id, error = %err, "清理旧头像图片失败");
                None
            }
        }
    }

    /// 重置页要显示的信息：给哪个账号设密码、链接什么时候到期。
    ///
    /// 邮箱做了打码——这一页可能被旁人瞄到，链接本身就是凭据，
    /// 不必再把完整地址摊在屏幕上。
    pub async fn reset_link_info(&self, token: &str) -> Result<ResetLinkInfo> {
        let record = self.find_reset_link(token).await?;
        Ok(ResetLinkInfo {
            email: mask_email(&record.email),
            expires_at: record.expires_at.to_rfc3339(),
        })
    }

    /// 用重置链接设新密码。令牌用一次就废。
    pub async fn reset_password_by_token(
        &self,
        token: &str,
        password_plain: &str,
        device: &DeviceInfo,
    ) -> Result<String> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        password::check_strength(password_plain)?;
        let record = self.find_reset_link(token).await?;
        let Some(user) = self.repo.find_user_by_email(&record.email).await? else {
            // 链接还在、账号没了：只可能是发链接之后管理员删了账号
            return Err(AppError::bad_request(
                "这个账号已经不存在，请重新发一封重置邮件",
            ));
        };
        self.repo.consume_verification(record.id, Utc::now()).await?;
        self.apply_new_password(&user, password_plain, device, "link")
            .await?;
        Ok(record.email)
    }

    /// 换密码的收尾：写哈希、清「必须改密码」、踢掉其它会话、留审计。
    ///
    /// 自助重置（验证码 / 邮件链接）与管理员设密码走两条入口，收尾必须是
    /// 同一套——两处各写一遍，迟早会出现「某种方式改完没踢会话」。
    async fn apply_new_password(
        &self,
        user: &User,
        password_plain: &str,
        device: &DeviceInfo,
        via: &str,
    ) -> Result<()> {
        // 重置邮件、重置链接、验证码三条路都收尾在这里：访客账号一个也不放行，
        // 否则「拿到一封信就能设个密码登进去」
        crate::guest::ensure_not_guest(user)?;
        let hash = password::hash_password(password_plain)?;
        self.repo.update_user_password(user.id, &hash).await?;
        // 密码是用户自己设的：管理员之前打的「必须改密码」到此为止
        self.repo.set_must_change_password(user.id, false).await?;
        let revoked = self.repo.revoke_all_sessions(user.id, Utc::now()).await?;
        self.audit_detail(
            actions::PASSWORD_RESET,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "revoked_sessions": revoked, "via": via }),
            device.ip.as_deref(),
        )
        .await;
        Ok(())
    }

    /// 公开给队列用：发件箱出队后重新投递一封邮件。
    pub async fn deliver_outbox_mail(
        &self,
        item: &pixora_repo::model::MailOutboxItem,
    ) -> Result<()> {
        let Some(mailer) = self.mailer() else {
            return Err(AppError::StorageUnavailable("SMTP 未配置".into()));
        };
        let template = item.template_name.as_str();
        let stored = self
            .repo
            .find_email_template(template)
            .await?
            .map(|row| (row.subject, row.body));
        let (subject, body) = pixora_mail::template::resolve(template, stored);

        let mut map = pixora_mail::MailVars::new();
        if let Some(object) = item.vars.as_object() {
            for (key, value) in object {
                map.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
            }
        }
        let subject = pixora_mail::render_subject(&subject, &map);
        let body = pixora_mail::render(&body, &map);
        mailer
            .send(&item.to_email, &subject, &body)
            .await
            // 用 Mail 变体而不是 Internal：它带着「还值不值得重试」，
            // 队列据此决定是退避重发还是就此收手（见 MailError::is_permanent）
            .map_err(AppError::Mail)
    }

    /// 没有配置 SMTP 时，把验证码写进日志并把码带出来（仅本地开发）。
    /// 验证码回显。只在显式开了 `PIXORA_DEV_RETURN_CODES=1` 时回显给接口。
    ///
    /// 原来只要「没有可用邮件通道」就回显，这有个真实的坑：管理员把 SMTP 配了一半
    /// （填了服务器和密码，漏了发信地址），邮件发不出去，接口却把验证码返回给任何人——
    /// 于是别人可以用受害者的邮箱把账号建出来。现在默认只写服务端日志。
    async fn debug_code(&self, code: &str) -> Option<String> {
        if self.mailer().is_some() {
            return None;
        }
        tracing::info!(code, "未配置 SMTP：验证码写在这里，本机开发直接用它");
        if self.config.dev_return_codes {
            tracing::warn!("PIXORA_DEV_RETURN_CODES 已开启：验证码会出现在接口响应里，请勿用于对外环境");
            return Some(code.to_string());
        }
        None
    }

    /// 同 IP 失败次数达到阈值就自动临时封禁。
    async fn register_ip_failure(&self, ip: Option<&str>, action: &str) {
        match self.ip_guard.record_failure(ip, action).await {
            Ok(Some(note)) => {
                self.audit_detail(
                    actions::TEMP_BAN,
                    None,
                    ip.unwrap_or("-"),
                    serde_json::json!({ "reason": note }),
                    ip,
                )
                .await;
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "风控判定失败"),
        }
    }
}

/// 从存下来的头像地址里取出 `(sha256, ext)`。
///
/// 只认站内直链（`/images/{sha}.{ext}`，一级目录名可以在后台改）：外链头像
/// 没有本地实体要收，返回 `None`。查询串与锚点忽略掉。
pub fn parse_direct_link(value: &str) -> Option<(String, String)> {
    let path = value.trim();
    if !path.starts_with('/') {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let file = path.rsplit('/').next()?;
    let (hash_part, ext) = file.rsplit_once('.')?;
    if hash_part.len() != 64 || !hash_part.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some((hash_part.to_ascii_lowercase(), ext.to_ascii_lowercase()))
}

/// 邮箱输入的基本校验与归一化。域名必须有点号，但不在这一步判断
/// 「域名存不存在」——那是 MX 校验的事。
pub fn normalize_email_input(email: &str) -> Result<String> {
    let trimmed = email.trim().to_lowercase();
    let (local, domain) = trimmed
        .split_once('@')
        .ok_or_else(|| AppError::bad_request("邮箱格式不正确"))?;
    if local.is_empty() || domain.is_empty() {
        return Err(AppError::bad_request("邮箱格式不正确"));
    }
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return Err(AppError::bad_request("邮箱域名不正确"));
    }
    if local.len() > 64 || domain.len() > 190 {
        return Err(AppError::bad_request("邮箱长度超出限制"));
    }
    let allowed = |ch: char| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '.' | '_'
                    | '%'
                    | '+'
                    | '-'
                    | '!'
                    | '#'
                    | '$'
                    | '&'
                    | '\''
                    | '*'
                    | '/'
                    | '='
                    | '?'
                    | '^'
                    | '`'
                    | '{'
                    | '|'
                    | '}'
                    | '~'
            )
    };
    if !local.chars().all(allowed) {
        return Err(AppError::bad_request("邮箱包含不支持的字符"));
    }
    Ok(trimmed)
}

/// 恢复码的归一化：去掉分隔符与大小写差异。
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证器丢了的人：邮箱验证码也能关掉二次验证（不然换机就等于锁死）。
    #[tokio::test]
    async fn email_code_can_disable_totp() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let user = crate::tests_support::make_user(&state, "totp@example.com", "3101").await;
        let device = DeviceInfo::default();

        state
            .repo
            .set_totp(user.id, Some("JBSWY3DPEHPK3PXP"), true)
            .await
            .unwrap();
        let fresh = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();

        // 没发过邮箱码时，乱填的码只会得到「验证器丢了可以改用邮箱验证码」这句提示
        let wrong = state.totp_disable(&fresh, "000000", &device).await.unwrap_err();
        assert!(wrong.message().contains("验证器不可用"), "{}", wrong.message());

        // 发一个邮箱码（这里只落库，不发信），用它就能关掉
        let code = state
            .issue_totp_disable_code(&fresh, None)
            .await
            .unwrap();
        state.totp_disable(&fresh, &code, &device).await.unwrap();
        let after = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(!after.totp_enabled, "邮箱验证码应当关掉二次验证");
        assert!(after.totp_secret.is_none(), "密钥也一并清掉");
    }

    /// 头像地址：站内直链折成相对路径（换域名不裂），外链原样留，空 = 清掉。
    #[tokio::test]
    async fn avatar_urls_are_normalized() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        state
            .settings
            .set(crate::settings::keys::SITE_BASE_URL, "https://img.example.com")
            .await
            .unwrap();

        // 本站直链折成相对路径
        assert_eq!(
            state
                .normalize_avatar("https://img.example.com/images/abc.png")
                .await
                .unwrap()
                .as_deref(),
            Some("/images/abc.png")
        );
        // 相对路径原样留
        assert_eq!(
            state.normalize_avatar("/images/abc.png").await.unwrap().as_deref(),
            Some("/images/abc.png")
        );
        // 外链原样留
        assert_eq!(
            state.normalize_avatar("https://cdn.example.org/me.png").await.unwrap().as_deref(),
            Some("https://cdn.example.org/me.png")
        );
        // 域名长得像本站但不是：不能被折成一段奇怪的路径
        assert_eq!(
            state
                .normalize_avatar("https://img.example.com.evil.net/x.png")
                .await
                .unwrap()
                .as_deref(),
            Some("https://img.example.com.evil.net/x.png")
        );
        // 清掉
        assert!(state.normalize_avatar("   ").await.unwrap().is_none());
        // 乱七八糟的不收
        assert!(state.normalize_avatar("不是地址").await.is_err());
        assert!(state.normalize_avatar(&"a".repeat(600)).await.is_err());
    }

    /// 头像地址里那几个零件：站内直链认得出，外链与乱七八糟的输入一律不认。
    #[test]
    fn direct_links_are_parsed_for_cleanup() {
        let sha = "05ed61440d8bc939cae590a525fcdcbd9c75addaabf7986a1219a3c55d3a98cd";
        assert_eq!(
            parse_direct_link(&format!("/images/{sha}.png")),
            Some((sha.to_string(), "png".to_string())),
            "站内直链要认出来（换头像时靠它找那张图）"
        );
        assert_eq!(
            parse_direct_link(&format!("/i/{sha}.webp?v=2")).map(|(hash, _)| hash).as_deref(),
            Some(sha),
            "查询串不算在文件名里"
        );
        assert!(parse_direct_link("https://cdn.example.org/me.png").is_none(), "外链没有本地实体");
        assert!(parse_direct_link("").is_none());
        assert!(parse_direct_link("/images/not-a-hash.png").is_none(), "哈希形状不对");
        assert!(parse_direct_link(&format!("/images/{sha}")).is_none(), "没有扩展名");
    }

    /// 重置页上显示的是打码邮箱：能认出是哪个账号，又不至于摊开完整地址。
    #[test]
    fn reset_link_masks_the_email() {
        assert_eq!(mask_email("user@example.com"), "u***@e***.com");
        assert_eq!(mask_email("a@b.co"), "a***@b***.co");
        // 形状不标准的输入也不能崩，凑合给个结果就行
        assert_eq!(mask_email("没有at符号"), "没***@****");
    }

    /// 自助换绑邮箱：要当前密码才发码，码只认新地址，改完账号历史里记得明明白白。
    #[tokio::test]
    async fn self_service_email_change_switches_the_address() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let device = DeviceInfo::default();

        // 口令得是真的：这一步就是要验它
        let user = state
            .repo
            .create_user(&pixora_repo::repo::users::NewUser {
                uid: "3201".to_string(),
                email: "old@example.com".to_string(),
                password_hash: pixora_crypto::password::hash_password("Old-Passw0rd-2026")
                    .expect("哈希"),
                role: pixora_repo::model::role::USER.to_string(),
                status: pixora_repo::model::status::ACTIVE.to_string(),
                quota_bytes: 1024 * 1024,
                email_verified: true,
            })
            .await
            .expect("建用户");

        // 密码不对：不往下走
        let wrong = state
            .send_email_change_code(&user, "new@example.com", "Not-The-Password", &device)
            .await
            .unwrap_err();
        assert!(wrong.message().contains("当前密码"), "{}", wrong.message());

        // 密码对了但没配 SMTP：如实说发不出去，而不是假装发了码
        let no_mail = state
            .send_email_change_code(&user, "new@example.com", "Old-Passw0rd-2026", &device)
            .await
            .unwrap_err();
        assert!(no_mail.message().contains("发信服务"), "{}", no_mail.message());

        // 码在这一步由邮件送达（测试里直接落库），提交后地址就换了
        let code = "482913";
        state
            .repo
            .create_email_verification(
                "new@example.com",
                &crypto_token::token_hash(code),
                purpose::CHANGE_EMAIL,
                Utc::now() + Duration::minutes(10),
                None,
            )
            .await
            .expect("落码");
        let delivery = state
            .change_own_email(&user, "new@example.com", code, &device)
            .await
            .expect("换绑");
        assert!(!delivery.1.delivered, "没有 SMTP 时不能报「通知已发出」");
        assert_eq!(delivery.0, "new@example.com");

        let after = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(after.email, "new@example.com");
        assert!(after.email_verified, "新地址是验过码的，标成已验证");

        // 同一个码再用一次：已经消费掉了
        assert!(
            state
                .change_own_email(&after, "third@example.com", code, &device)
                .await
                .is_err(),
            "一次性的码不能复用"
        );

        // 账号历史里留了一条「本人更换邮箱」，从哪到哪都写着
        let page = state
            .account_log(
                &crate::account_log::AccountLogFilter {
                    action: Some(actions::ACCOUNT_EMAIL_CHANGE.to_string()),
                    uid: None,
                    query: None,
                },
                pixora_repo::PageRequest {
                    page: 1,
                    per_page: 50,
                },
            )
            .await
            .expect("读账号历史");
        assert_eq!(page.items.len(), 1, "换绑只该记一条");
        let row = &page.items[0];
        assert_eq!(row.target_uid.as_deref(), Some("3201"));
        assert_eq!(row.actor_kind, "self", "本人操作要标成 self");
        assert!(row.actor_label.contains("本人"), "{}", row.actor_label);
        assert_eq!(row.detail["from"], "old@example.com");
        assert_eq!(row.detail["to"], "new@example.com");
    }

    #[tokio::test]
    async fn reset_link_is_single_use_and_expires() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let user = crate::tests_support::make_user(&state, "link@example.com", "3001").await;

        // 站点地址没配时发不出链接：邮件里就只剩验证码
        assert!(state.issue_reset_link(&user.email).await.unwrap().is_empty());

        state
            .settings
            .set(crate::settings::keys::SITE_BASE_URL, "https://img.example.com")
            .await
            .unwrap();
        let sentence = state.issue_reset_link(&user.email).await.unwrap();
        assert!(sentence.contains("https://img.example.com/#/reset-password?token=pxr_"));
        let token = sentence.split("token=").last().unwrap().trim().to_string();

        // 令牌换出来的信息：打码邮箱 + 到期时间
        let info = state.reset_link_info(&token).await.unwrap();
        assert_eq!(info.email, "l***@e***.com");

        // 设一次密码，令牌就废了
        let device = crate::auth::DeviceInfo::default();
        state
            .reset_password_by_token(&token, "LinkReset-2026x", &device)
            .await
            .unwrap();
        assert!(
            state.reset_password_by_token(&token, "Again-2026x", &device).await.is_err(),
            "同一个令牌不能用第二次"
        );
        let fresh = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(pixora_crypto::password::verify_password("LinkReset-2026x", &fresh.password_hash).unwrap());
    }

    #[test]
    fn email_validation_catches_obvious_mistakes() {
        assert_eq!(
            normalize_email_input("  ChenYu@Example.com ").unwrap(),
            "chenyu@example.com"
        );
        assert!(normalize_email_input("no-at-sign").is_err());
        assert!(normalize_email_input("a@localhost").is_err());
        assert!(normalize_email_input("@example.com").is_err());
        assert!(normalize_email_input("a@.com").is_err());
        assert!(normalize_email_input("a+b@example.com").is_ok());
    }

    #[test]
    fn device_label_prefers_specific_browser() {
        let edge = DeviceInfo {
            user_agent: Some(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120 Safari/537.36 Edg/120"
                    .into(),
            ),
            ip: Some("203.0.113.18".into()),
        };
        assert_eq!(edge.device_label().as_deref(), Some("Edge · Windows"));

        let iphone = DeviceInfo {
            user_agent: Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) AppleWebKit/605 Safari/604".into(),
            ),
            ip: None,
        };
        assert_eq!(iphone.device_label().as_deref(), Some("Safari · iOS"));

        let picgo = DeviceInfo {
            user_agent: Some("PicGo/2.3 Windows".into()),
            ip: None,
        };
        assert_eq!(picgo.device_label().as_deref(), Some("PicGo · Windows"));

        assert_eq!(DeviceInfo::default().device_label(), None);
    }

    #[test]
    fn recovery_code_normalization() {
        assert_eq!(normalize_recovery_code("1234-5678"), "12345678");
        assert_eq!(normalize_recovery_code("1234 5678"), "12345678");
        assert_eq!(normalize_recovery_code("ABCD-EF12"), "abcdef12");
    }
}
