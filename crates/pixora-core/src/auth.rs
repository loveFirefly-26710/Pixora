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

use chrono::Utc;
use pixora_crypto::password;
use pixora_crypto::token as crypto_token;
use pixora_repo::model::{now, role, status, User};
use pixora_repo::repo::users::NewUser;
use pixora_repo::repo::verifications::purpose;

use crate::antispam;
use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::AppState;

/// 连续登录失败多少次锁定。
const LOGIN_FAIL_LIMIT: i32 = 5;
/// 锁定多久（分钟）。
const LOGIN_LOCK_MINUTES: i64 = 15;
/// 验证码有效期（分钟）。
const CODE_TTL_MINUTES: i64 = 10;
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
        antispam::check_email_allowed(self, &email).await?;

        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已经注册过".into()));
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

        self.dispatch_mail(
            &email,
            pixora_mail::template::names::VERIFY_CODE,
            serde_json::json!({ "email": email, "code": code }),
        )
        .await?;

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

        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已经注册过".into()));
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

        self.dispatch_mail(
            &email,
            pixora_mail::template::names::RESET_PASSWORD,
            serde_json::json!({ "email": email, "code": code }),
        )
        .await?;

        Ok(CodeSent {
            expires_in: CODE_TTL_MINUTES * 60,
            resend_after: RESEND_COOLDOWN_SECONDS,
            debug_code: self.debug_code(&code).await,
        })
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

        let hash = password::hash_password(password_plain)?;
        self.repo.update_user_password(user.id, &hash).await?;
        let revoked = self.repo.revoke_all_sessions(user.id, Utc::now()).await?;

        self.audit_detail(
            actions::PASSWORD_RESET,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "revoked_sessions": revoked }),
            device.ip.as_deref(),
        )
        .await;
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
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let email = normalize_email_input(email)?;

        let Some(user) = self.repo.find_user_by_email(&email).await? else {
            // 账号不存在与密码错误给同一个错误：不给撞库者任何线索
            self.register_ip_failure(device.ip.as_deref(), actions::LOGIN_FAIL)
                .await;
            return Err(AppError::InvalidCredentials);
        };

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
    async fn issue_session(
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
                "当前会话已失效，请重新登录后再切换用户",
            ));
        };
        if session.user_id != admin.id {
            return Err(AppError::forbidden("会话与当前管理员对不上"));
        }

        let Some(target) = self.repo.find_user_by_id(target_user_id).await? else {
            return Err(AppError::not_found("用户"));
        };
        if target.id == admin.id {
            return Err(AppError::bad_request("目标就是你自己，不用切换"));
        }
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

    pub async fn totp_enable(&self, user: &User, code: &str, device: &DeviceInfo) -> Result<()> {
        let secret = user
            .totp_secret
            .clone()
            .ok_or_else(|| AppError::bad_request("请先生成密钥再提交动态码"))?;
        if !pixora_crypto::totp::verify(&secret, code, 1)? {
            return Err(AppError::TotpInvalid("动态验证码不正确".into()));
        }
        self.repo.set_totp(user.id, Some(&secret), true).await?;
        self.audit_action(actions::SETTINGS_CHANGE, Some(user.id), device.ip.as_deref())
            .await;
        Ok(())
    }

    pub async fn totp_disable(&self, user: &User, code: &str, device: &DeviceInfo) -> Result<()> {
        let secret = user
            .totp_secret
            .clone()
            .ok_or_else(|| AppError::bad_request("账号尚未开启二次验证"))?;
        if !pixora_crypto::totp::verify(&secret, code, 1)? {
            return Err(AppError::TotpInvalid("动态验证码不正确".into()));
        }
        self.repo.set_totp(user.id, None, false).await?;
        // 关掉二次验证时清掉恢复码，避免留下「已经作废但看起来还有效」的凭据
        self.repo.replace_recovery_codes(user.id, &[]).await?;
        self.audit_action(actions::SETTINGS_CHANGE, Some(user.id), device.ip.as_deref())
            .await;
        Ok(())
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
    async fn consume_code(&self, email: &str, purpose_value: &str, code: &str) -> Result<()> {
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
    /// 公开（`pub`）是因为内容审核的上报与自动暂停通知也要走同一条路径——
    /// 三处各写一遍渲染与重试，迟早会漂移。
    pub async fn dispatch_mail(
        &self,
        to: &str,
        template: &str,
        vars: serde_json::Value,
    ) -> Result<()> {
        let outbox_id = self.repo.enqueue_mail(to, template, &vars).await?;

        let Some(mailer) = self.mailer() else {
            tracing::warn!(to, template, "SMTP 未配置，验证码只留在发件箱里");
            return Ok(());
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
            }
            Err(err) => {
                // 失败留在发件箱里排队重试，不让注册流程因为 SMTP 抖动而失败
                tracing::warn!(to, template, error = %err, "邮件投递失败，已进入重试队列");
                let next = Utc::now() + chrono::Duration::minutes(2);
                self.repo
                    .mark_mail_failed(outbox_id, &err.to_string(), Some(next), false)
                    .await?;
            }
        }
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
            .map_err(|err| AppError::internal(err.to_string()))
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
