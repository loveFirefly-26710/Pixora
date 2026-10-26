//! 后台用户管理与处置。
//!
//! 设计稿对处置这块的要求最细，逐条落在这里：
//!
//!   - **暂停与封禁都比「禁用」重**：网页登录与 API 调用同时失效，靠账号状态
//!     在鉴权中间件里统一拦（见 `auth::ensure_loginable`）。
//!   - **API Token 不删**：鉴权时按状态拦，解封即自动恢复，不用重新签发一遍。
//!   - **管理员账号不能被处置**：否则管理员能把自己锁在外面，且没人能来救。
//!   - **处置记录只增不改**，处置原因、解封要求、是否发信都留档。
//!   - **解封走邮件人工**：通知邮件用注册发信邮箱发出，正文里写清解封材料清单。
//!     暂停写「可申请解封」，封禁只写「请联系管理员」——不承诺解封，否则给出错误预期。

use chrono::{Duration, Utc};
use pixora_repo::model::{role, status, User};
use pixora_repo::repo::users::{NewUser, UserFilter};
use pixora_repo::{Page, PageRequest};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::AppState;

/// 处置动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Suspend,
    Ban,
    Restore,
}

impl Disposition {
    pub fn parse(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "suspend" => Ok(Self::Suspend),
            "ban" => Ok(Self::Ban),
            "restore" | "unsuspend" | "unban" => Ok(Self::Restore),
            other => Err(AppError::bad_request(format!("不支持的处置类型：{other}"))),
        }
    }

    fn target_status(self) -> &'static str {
        match self {
            Self::Suspend => status::SUSPENDED,
            Self::Ban => status::BANNED,
            Self::Restore => status::ACTIVE,
        }
    }

    fn action_name(self) -> &'static str {
        match self {
            Self::Suspend => actions::ACCOUNT_SUSPEND,
            Self::Ban => actions::ACCOUNT_BAN,
            Self::Restore => actions::ACCOUNT_RESTORE,
        }
    }

    fn user_action(self) -> &'static str {
        match self {
            Self::Suspend => "suspend",
            Self::Ban => "ban",
            Self::Restore => "unsuspend",
        }
    }
}

/// 处置请求体。
#[derive(Debug, Clone)]
pub struct DispositionInput {
    pub action: Disposition,
    pub reason: Option<String>,
    /// 生效时长（分钟）。`None` 表示永久，只能人工解封。
    pub duration_minutes: Option<i64>,
    /// 解封要求，写进通知邮件的「如何解封」部分
    pub requirements: Option<String>,
    pub appeal_email: Option<String>,
    pub notify: bool,
}

/// 处置结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DispositionOutcome {
    pub status: String,
    pub revoked_sessions: i64,
    pub mail_sent: bool,
    pub mail_error: Option<String>,
    /// 到期自动恢复的时刻（`None` = 永久，要人工解封）
    pub auto_restore_at: Option<String>,
}

/// 删除账号的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeleteUserOutcome {
    pub email: String,
    pub uid: String,
    /// 这个账号曾经引用过的实体数（原图 + 缩略图）
    pub blobs: usize,
    /// 当场删掉的物理文件数。共享实体与远端失败的会留给后台重试
    pub reclaimed_files: u64,
}

impl AppState {
    /// 安装时创建的那个管理员账号的 UID。
    ///
    /// 先看设置里记的值；早先装好的实例没有这个键，退回「最早创建的账号」——
    /// 账号按 id 递增创建，第一个就是当年安装时建的管理员。
    pub async fn install_admin_uid(&self) -> Result<Option<String>> {
        let recorded = self
            .settings
            .raw(crate::settings::keys::INSTALL_ADMIN_UID)
            .await?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        match recorded {
            Some(uid) => Ok(Some(uid)),
            None => Ok(self.repo.first_user_uid().await?),
        }
    }

    /// 别的管理员不能动安装账号。
    ///
    /// 安装账号是这套部署的主钥匙：能处置普通账号的管理员，不该顺手把它改名、
    /// 降权、切进去用或者删掉。它操作自己不受这条限制。
    pub async fn ensure_not_install_admin(&self, actor: &User, target: &User) -> Result<()> {
        if actor.id == target.id {
            return Ok(());
        }
        if let Some(uid) = self.install_admin_uid().await? {
            if target.uid == uid {
                return Err(AppError::forbidden(
                    "这是安装时创建的管理员账号，其他管理员不能改动它",
                ));
            }
        }
        Ok(())
    }

    /// 管理端用户列表。
    pub async fn admin_user_list(
        &self,
        query: Option<&str>,
        role_value: Option<&str>,
        status_value: Option<&str>,
        page: Option<i64>,
        per_page: Option<i64>,
    ) -> Result<Page<AdminUserRow>> {
        let filter = UserFilter {
            query: query.map(str::to_string),
            role: role_value.map(str::to_string),
            status: status_value.map(str::to_string),
        };
        let page = self
            .repo
            .list_users(&filter, PageRequest::new(page, per_page, 20, 100))
            .await?;
        Ok(page.map(|row| AdminUserRow {
            guest: crate::guest::is_guest_uid(&row.uid),
            id: row.id,
            uid: row.uid,
            email: row.email,
            email_verified: row.email_verified,
            role: row.role,
            status: row.status.clone(),
            status_label: pixora_repo::model::status::label(&row.status).to_string(),
            quota_bytes: row.quota_bytes,
            used_bytes: row.used_bytes,
            image_count: row.image_count,
            rate_custom: row.rate_custom,
            created_at: row.created_at.to_rfc3339(),
            last_login_at: row.last_login_at.map(|at| at.to_rfc3339()),
        }))
    }

    /// 用户详情：账号 + 处置记录 + 生效限额。
    pub async fn admin_user_detail(&self, id: i64) -> Result<AdminUserDetail> {
        let summary = self
            .repo
            .find_user_summary(id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        let user = self
            .repo
            .find_user_by_id(id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        let actions = self.repo.list_user_actions(id, 50).await?;
        let identities = self.identities_of(&user).await.unwrap_or_default();
        let policy = self.repo.effective_policy(&user).await?;
        let custom = self.repo.user_policy(id).await?;
        // 上传策略按生效值展示：用户卡上填了 0（跟随）也不该让详情页显示 0，
        // 管理员要看到的是这个人当下真正被允许传多大的文件、什么格式
        let upload = self.effective_upload_limits(&user).await?;

        Ok(AdminUserDetail {
            user: AdminUserRow {
                id: summary.id,
                uid: summary.uid,
                email: summary.email,
                email_verified: summary.email_verified,
                role: summary.role,
                status: summary.status.clone(),
                status_label: pixora_repo::model::status::label(&summary.status).to_string(),
                quota_bytes: summary.quota_bytes,
                used_bytes: summary.used_bytes,
                image_count: summary.image_count,
                rate_custom: summary.rate_custom,
                created_at: summary.created_at.to_rfc3339(),
                last_login_at: summary.last_login_at.map(|at| at.to_rfc3339()),
                guest: crate::guest::is_guest(&user),
            },
            reason: user.status_reason.clone(),
            status_changed_at: user.status_changed_at.map(|at| at.to_rfc3339()),
            totp_enabled: user.totp_enabled,
            email_verified: user.email_verified,
            actions: actions
                .into_iter()
                .map(|item| UserActionRow {
                    id: item.id,
                    action: item.action.clone(),
                    action_label: item.action_label(),
                    reason: item.reason.clone(),
                    detail: item.detail.clone(),
                    mail_sent: item.mail_sent,
                    created_at: item.created_at.to_rfc3339(),
                })
                .collect(),
            policy: PolicyRow {
                unlimited: policy.unlimited,
                custom: custom.is_some(),
                api_per_minute: custom
                    .as_ref()
                    .map(|row| row.api_per_minute)
                    .unwrap_or(policy.api_per_minute),
                api_per_hour: policy.api_per_hour,
                api_read_per_minute: policy.api_read_per_minute,
                api_burst: policy.api_burst,
                upload_per_minute: custom
                    .as_ref()
                    .map(|row| row.upload_per_minute)
                    .unwrap_or(policy.upload_per_minute),
                upload_per_hour: policy.upload_per_hour,
                upload_per_day: custom
                    .as_ref()
                    .map(|row| row.upload_per_day)
                    .unwrap_or(policy.upload_per_day),
                upload_bytes_per_day: custom
                    .as_ref()
                    .map(|row| row.upload_bytes_per_day)
                    .unwrap_or(policy.upload_bytes_per_day),
                max_concurrent_uploads: upload.max_concurrent_uploads,
                max_batch_size: upload.max_batch,
                max_file_size: upload.max_file_bytes,
                max_pixels: upload.max_pixels,
                max_width: upload.max_width,
                max_height: upload.max_height,
                formats: upload.formats,
                download_per_minute: custom
                    .as_ref()
                    .map(|row| row.download_per_minute)
                    .unwrap_or(policy.download_per_minute),
                download_per_day: custom
                    .as_ref()
                    .map(|row| row.download_per_day)
                    .unwrap_or(policy.download_per_day),
                note: custom.as_ref().and_then(|row| row.note.clone()),
            },
            policy_override: custom.as_ref().map(|row| PolicyRow {
                unlimited: row.unlimited,
                custom: true,
                api_per_minute: row.api_per_minute,
                api_per_hour: row.api_per_hour,
                api_read_per_minute: row.api_read_per_minute,
                api_burst: row.api_burst,
                upload_per_minute: row.upload_per_minute,
                upload_per_hour: row.upload_per_hour,
                upload_per_day: row.upload_per_day,
                upload_bytes_per_day: row.upload_bytes_per_day,
                max_concurrent_uploads: row.max_concurrent_uploads,
                max_batch_size: row.max_batch_size,
                max_file_size: row.max_file_size,
                max_pixels: row.max_pixels,
                max_width: row.max_width,
                max_height: row.max_height,
                formats: row
                    .formats
                    .as_deref()
                    .map(crate::settings::parse_format_list)
                    .unwrap_or_default(),
                download_per_minute: row.download_per_minute,
                download_per_day: row.download_per_day,
                note: row.note.clone(),
            }),
            identities,
        })
    }

    /// 管理员改用户资料：邮箱、密码、角色。
    /// 管理员改用户资料：邮箱、密码、角色。
    ///
    /// 返回值里第二项是密码邮件（或重置邮件）的投递结果：只有管理员直接
    /// 设密码时才有值。界面要如实告诉管理员「邮件出去没有」，一句无条件的
    /// 「已重置」会让人以为对方已经拿到了密码。
    pub async fn admin_update_user(
        &self,
        admin: &User,
        user_id: i64,
        email: Option<&str>,
        password: Option<&str>,
        role_value: Option<&str>,
        ip: Option<&str>,
    ) -> Result<(AdminUserDetail, Option<crate::mail::MailDelivery>)> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;

        self.ensure_not_install_admin(admin, &target).await?;
        // 密码与角色决定「这个账号能不能被登进来、是不是管理员」，访客账号这两样
        // 都不动——配额、状态仍然可以调，那是管理员按住游客上传的手段（见 guest.rs）
        if password.is_some() || role_value.is_some() {
            crate::guest::ensure_not_guest(&target)?;
        }
        let target_is_guest = crate::guest::is_guest(&target);
        let mut detail_changes = serde_json::Map::new();
        let mut mail: Option<crate::mail::MailDelivery> = None;

        if let Some(email) = email {
            let email = crate::auth::normalize_email_input(email)?;
            if !email.eq_ignore_ascii_case(&target.email) {
                if self.repo.email_exists(&email).await? {
                    return Err(AppError::Conflict("该邮箱已被其他账号使用".into()));
                }
                // 访客账号的邮箱是个记号：它登不进来、也不收发信，管理员改它
                // 不需要验证、不查它收不收得到信，也不发变更通知（发不出去的东西
                // 不该走发信账号，见 guest.rs 与 antispam）。别的账号照旧
                if !target_is_guest {
                    self.ensure_deliverable(&email).await?;
                }
                // 管理员填的地址视为可信，直接标成已验证：填错一个字就把账号
                // 锁死过，那次之后这条规则就固定下来了
                self.repo.set_email_verified(user_id, &email, true).await?;
                detail_changes.insert("email".into(), serde_json::json!(email));
                self.repo
                    .insert_user_action(
                        user_id,
                        Some(admin.id),
                        "change_email",
                        Some("管理员修改邮箱"),
                        &serde_json::json!({ "from": target.email, "to": email }),
                        false,
                        Utc::now(),
                    )
                    .await?;
                // 账号历史里单独记一条「谁把哪个账号的邮箱改成了什么」：
                // 本人自助走的是同一个动作名，靠 via 区分；操作人邮箱留个快照，
                // 账号（或操作人）被删之后还读得出来是谁做的
                self.audit_detail(
                    actions::ACCOUNT_EMAIL_CHANGE,
                    Some(admin.id),
                    &target.uid,
                    serde_json::json!({
                        "from": target.email,
                        "to": email,
                        "via": "admin",
                        "actor_email": admin.email,
                        "guest": target_is_guest,
                    }),
                    ip,
                )
                .await;
                if !target_is_guest {
                    // 新旧两个地址各发一封：新地址的收件人确认「这是我」，
                    // 旧地址的收件人才能在「不是我安排的」时第一时间发现
                    mail = Some(self.notify_email_change(&target, &email).await?);
                }
            }
        }

        if let Some(password) = password {
            pixora_crypto::password::check_strength(password)?;
            let hash = self.hash_password(password).await?;
            self.repo.update_user_password(user_id, &hash).await?;
            // 重置密码后旧会话必须失效，否则账号可能仍被别人用着
            let revoked = self.repo.revoke_all_sessions(user_id, Utc::now()).await?;
            // 这个密码是经邮件送达的（可能落在聊天记录里），所以要求用户
            // 下次登录自己改一个：没改之前账号只放行 /me 与改密码本身
            self.repo.set_must_change_password(user_id, true).await?;
            detail_changes.insert("password_reset".into(), serde_json::json!(revoked));
            detail_changes.insert("must_change_password".into(), serde_json::json!(true));
            self.repo
                .insert_user_action(
                    user_id,
                    Some(admin.id),
                    "reset_password",
                    Some("管理员重置密码"),
                    &serde_json::json!({ "revoked_sessions": revoked, "must_change_password": true }),
                    false,
                    Utc::now(),
                )
                .await?;
            // 明文密码只在这一刻存在：发信告诉用户，免得管理员再口述一遍
            let delivery = self.send_password_set_mail(&target, password).await?;
            mail = Some(delivery);
        }

        if let Some(role_value) = role_value {
            if role_value != role::ADMIN && target.is_admin() {
                // 安装账号不能降权：降了之后别的管理员碰不了它，它自己也当不成
                // 管理员，这个身份就废在那里了
                if self.install_admin_uid().await?.as_deref() == Some(target.uid.as_str()) {
                    return Err(AppError::bad_request(
                        "安装时创建的管理员不能降为普通用户；要换主账号，先给另一个账号管理员权限",
                    ));
                }
                let admins = self.admin_count().await?;
                if admins <= 1 {
                    return Err(AppError::bad_request("至少需保留一个管理员"));
                }
            }
            self.repo.update_user_role(user_id, role_value).await?;
            detail_changes.insert("role".into(), serde_json::json!(role_value));
        }

        // 只改邮箱时 ACCOUNT_EMAIL_CHANGE 已经单独记过，不再重复一条；
        // 角色/密码这类资料修改记 ACCOUNT_UPDATE，归到「账户信息」而不是系统设置。
        if detail_changes.keys().any(|key| key != "email") {
            self.audit_detail(
                actions::ACCOUNT_UPDATE,
                Some(admin.id),
                &target.uid,
                serde_json::Value::Object(detail_changes),
                ip,
            )
            .await;
        }
        Ok((self.admin_user_detail(user_id).await?, mail))
    }

    /// 邮箱变更通知：新旧地址各发一封，把两次投递的结果合成一句给管理员看。
    ///
    /// 自助换绑与管理端改邮箱共用这一条：两个地址都该收到信——新地址确认
    /// 「这确实是我在改」，旧地址才能在「不是我干的」时第一时间发现。
    pub(crate) async fn notify_email_change(
        &self,
        target: &User,
        new_email: &str,
    ) -> Result<crate::mail::MailDelivery> {
        let mail_settings = self.settings.mail().await?;
        let appeal_email = if mail_settings.appeal_email.trim().is_empty() {
            mail_settings.from_email.clone()
        } else {
            mail_settings.appeal_email.clone()
        };
        let vars = serde_json::json!({
            "uid": target.uid,
            "old_email": target.email,
            "new_email": new_email,
            "time": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
            "appeal_email": appeal_email,
        });

        let mut failures: Vec<String> = Vec::new();
        for (label, to) in [("新邮箱", new_email), ("旧邮箱", target.email.as_str())] {
            match self
                .dispatch_mail(
                    to,
                    pixora_mail::template::names::EMAIL_CHANGED,
                    vars.clone(),
                )
                .await
            {
                Ok(delivery) if delivery.delivered => {}
                Ok(delivery) => failures.push(format!(
                    "{label}（{to}）：{}",
                    delivery.detail.unwrap_or_else(|| "投递失败".into())
                )),
                Err(err) => failures.push(format!("{label}（{to}）：{}", err.message())),
            }
        }
        Ok(crate::mail::MailDelivery {
            delivered: failures.is_empty(),
            detail: if failures.is_empty() {
                None
            } else {
                Some(failures.join("；"))
            },
        })
    }

    /// 把「管理员设的新密码」发给用户，并说明下次登录要自己改。
    async fn send_password_set_mail(
        &self,
        target: &User,
        password: &str,
    ) -> Result<crate::mail::MailDelivery> {
        let mail_settings = self.settings.mail().await?;
        let appeal_email = if mail_settings.appeal_email.trim().is_empty() {
            mail_settings.from_email.clone()
        } else {
            mail_settings.appeal_email.clone()
        };
        self.dispatch_mail(
            &target.email,
            pixora_mail::template::names::PASSWORD_SET_BY_ADMIN,
            serde_json::json!({
                "email": target.email,
                "uid": target.uid,
                "password": password,
                "appeal_email": appeal_email,
            }),
        )
        .await
    }

    /// 管理员替某个用户发一封重置邮件（里面是他自己点开就能设新密码的链接）。
    ///
    /// 与用户自助找回走同一个发码逻辑，只有两点不同：账号不存在时直接报错
    /// （管理员填的是具体某个人），以及把真实投递结果回给管理员——他点的
    /// 就是「帮我发封信」，界面上不能只回一句无条件的「已发送」。
    pub async fn admin_send_reset_mail(
        &self,
        admin: &User,
        user_id: i64,
        ip: Option<&str>,
    ) -> Result<crate::mail::MailDelivery> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        self.ensure_not_install_admin(admin, &target).await?;
        // 这封信的全部意义就是「让收件人设一个新密码」，而访客账号不接受登录，
        // 发出去只会是一封没人能用的信
        crate::guest::ensure_not_guest(&target)?;
        // 这个地址还收不收得到信：域名没了、MX 撤了的时候，管理员点这一下
        // 不该换来一封退信。发不出去的结果照旧如实带回界面（下面那几行）
        self.ensure_deliverable(&target.email).await?;

        let code = pixora_crypto::token::new_numeric_code(6)?;
        let code_hash = pixora_crypto::token::token_hash(&code);
        let expires_at = Utc::now() + Duration::minutes(crate::auth::code_ttl_minutes());
        self.repo
            .create_email_verification(
                &target.email,
                &code_hash,
                pixora_repo::repo::verifications::purpose::RESET,
                expires_at,
                ip,
            )
            .await?;

        let delivery = self.send_reset_mail(&target.email, &code).await?;

        self.repo
            .insert_user_action(
                user_id,
                Some(admin.id),
                "reset_password_mail",
                Some("管理员代为发送重置邮件"),
                &serde_json::json!({ "delivered": delivery.delivered, "detail": delivery.detail }),
                delivery.delivered,
                Utc::now(),
            )
            .await?;
        self.audit_detail(
            actions::PASSWORD_RESET,
            Some(admin.id),
            &target.uid,
            serde_json::json!({
                "via": "admin_mail",
                "delivered": delivery.delivered,
                "detail": delivery.detail,
            }),
            ip,
        )
        .await;
        Ok(delivery)
    }

    /// 处置：暂停、封禁、恢复。
    pub async fn dispose_user(
        &self,
        admin: &User,
        user_id: i64,
        input: DispositionInput,
        ip: Option<&str>,
    ) -> Result<DispositionOutcome> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;

        // 管理员不能被处置：否则会把自己锁在外面，而且没人能来救
        if target.is_admin() {
            return Err(AppError::bad_request(
                "管理员账号不能被暂停或封禁，请先调整角色",
            ));
        }
        if target.id == admin.id {
            return Err(AppError::bad_request("不能对自己执行处置"));
        }
        self.ensure_not_install_admin(admin, &target).await?;

        let now = Utc::now();
        let new_status = input.action.target_status();
        let reason = input
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        if input.action != Disposition::Restore && reason.is_none() {
            return Err(AppError::bad_request("请填写处置原因：该原因会展示给用户"));
        }

        // 生效时长：填了就到点自动恢复，不填就是永久。
        // 走 `auto_unsuspend_at` 这一条路，与内容审核的临时封禁同一套机制——
        // 用户下次访问时发现已到期，就地放回 active，不需要额外的定时任务
        let expires_at = input
            .duration_minutes
            .filter(|minutes| *minutes > 0)
            .map(|minutes| now + Duration::minutes(minutes));

        self.repo
            .update_user_status_until(
                user_id,
                new_status,
                reason.as_deref(),
                Some(admin.id),
                now,
                expires_at,
            )
            .await?;

        // 处置后立即踢下线。API Token 不删——鉴权时按状态拦，解封即恢复。
        let revoked = self.repo.revoke_all_sessions(user_id, now).await? as i64;

        let detail = serde_json::json!({
            "from": target.status,
            "to": new_status,
            "duration_minutes": input.duration_minutes,
            "expires_at": expires_at.map(|at| at.to_rfc3339()),
            "requirements": input.requirements,
        });

        self.repo
            .insert_user_action(
                user_id,
                Some(admin.id),
                input.action.user_action(),
                reason.as_deref(),
                &detail,
                false,
                now,
            )
            .await?;

        self.audit_detail(
            input.action.action_name(),
            Some(admin.id),
            &target.uid,
            detail.clone(),
            ip,
        )
        .await;

        // 通知邮件：发件地址取注册发信邮箱，不额外配一个申诉邮箱。
        // 访客账号不发：它的邮箱是保留域（`visitor@pixora.invalid`），发出去只会
        // 退信，而它本身也登不进来——暂停它就是「按住游客上传」这一个作用
        let mut mail_sent = false;
        let mut mail_error = None;
        if input.notify && input.action != Disposition::Restore && !crate::guest::is_guest(&target)
        {
            match self
                .send_disposition_mail(&target, &input, reason.as_deref())
                .await
            {
                Ok(delivery) => {
                    mail_sent = delivery.delivered;
                    mail_error = delivery.detail;
                }
                Err(err) => {
                    mail_error = Some(err.message());
                    tracing::warn!(user = %target.email, error = %err, "处置通知邮件发送失败");
                }
            }
        }

        Ok(DispositionOutcome {
            status: new_status.to_string(),
            revoked_sessions: revoked,
            mail_sent,
            mail_error,
            auto_restore_at: expires_at.map(|at| at.to_rfc3339()),
        })
    }

    /// 管理端直接建号。
    ///
    /// 与注册流程的区别只有一处：邮箱标记为已验证——管理员手输的地址就是
    /// 「我确认这个邮箱没问题」，再让对方走一遍验证邮件没有意义。
    /// 配额用当前默认值，建完可以在详情页单独调。
    pub async fn admin_create_user(
        &self,
        admin: &User,
        email_input: &str,
        password: &str,
        role_value: Option<&str>,
        ip: Option<&str>,
    ) -> Result<AdminUserDetail> {
        let email = crate::auth::normalize_email_input(email_input)?;
        pixora_crypto::password::check_strength(password)
            .map_err(|err| AppError::bad_request(err.to_string()))?;
        if self.repo.email_exists(&email).await? {
            return Err(AppError::Conflict("该邮箱已经注册过".into()));
        }

        // 角色只认 admin / user：放任别的字符串写进库，
        // 之后 is_admin() 之类的判断会给出「谁都不是」的结果
        let role_value = match role_value.map(str::trim).filter(|value| !value.is_empty()) {
            None => role::USER.to_string(),
            Some(value) if value == role::ADMIN || value == role::USER => value.to_string(),
            Some(value) => {
                return Err(AppError::bad_request(format!(
                    "角色只能是 admin 或 user（收到 {value}）"
                )))
            }
        };

        let (default_quota, _) = self.settings.quota_values().await?;
        let password_hash = self.hash_password(password).await?;
        let user = self
            .create_user_with_uid(NewUser {
                uid: String::new(),
                email: email.clone(),
                password_hash,
                role: role_value.clone(),
                status: status::ACTIVE.to_string(),
                quota_bytes: default_quota,
                email_verified: true,
            })
            .await?;

        // 默认相册：上传不指定相册时的落点，缺了它第一次上传就得报错
        self.repo.insert_default_album(user.id, "默认相册").await?;

        self.audit_detail(
            actions::REGISTER,
            Some(admin.id),
            &user.uid,
            serde_json::json!({ "email": email, "role": role_value, "via": "admin" }),
            ip,
        )
        .await;
        self.repo
            .insert_user_action(
                user.id,
                Some(admin.id),
                "create",
                Some("管理员创建账号"),
                &serde_json::json!({ "role": role_value }),
                false,
                Utc::now(),
            )
            .await?;

        self.admin_user_detail(user.id).await
    }

    /// 删除账号：连它名下的图片、相册、令牌、会话一起删，以及不再被别人引用的
    /// 物理文件。
    ///
    /// 这一步不可逆，回收站里的图也一并走。所以要求调用方把账号邮箱原样写进
    /// `confirm`——界面上让管理员照着敲一遍，手滑点不出第二下。
    ///
    /// 访客账号不在可删之列：它是游客上传的归属地，删掉就等于把全站游客的图
    /// 一次性清除（而且下一次游客上传又会把它建回来）。想停止游客上传，
    /// 关掉「允许游客上传」或者暂停这个账号都行，账留着
    pub async fn admin_delete_user(
        &self,
        admin: &User,
        user_id: i64,
        confirm: Option<&str>,
        ip: Option<&str>,
    ) -> Result<DeleteUserOutcome> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        if target.id == admin.id {
            return Err(AppError::bad_request("不能删除自己的账号"));
        }
        if crate::guest::is_guest(&target) {
            return Err(AppError::forbidden(
                "访客账号是系统账号，不能删除；要停止游客上传，关掉「允许游客上传」或暂停这个账号",
            ));
        }
        self.ensure_not_install_admin(admin, &target).await?;

        let typed = confirm.unwrap_or("").trim();
        if !typed.eq_ignore_ascii_case(&target.email) {
            return Err(AppError::bad_request(format!(
                "删除账号会连同其图片一并清除，且无法恢复。请将账号邮箱 {} 原样填入 confirm 后提交",
                target.email
            )));
        }

        // 先删账号（连带把引用计数还回去），再回收物理文件：
        // 顺序反过来的话，共享实体上还挂着这个账号的引用，收不掉
        let blobs = self.repo.delete_user(user_id).await?;
        let reclaimed_files = self.reclaim_purged(blobs.clone()).await;

        self.audit_detail(
            actions::ACCOUNT_DELETE,
            Some(admin.id),
            &target.uid,
            serde_json::json!({
                "email": target.email,
                "used_bytes": target.used_bytes,
                "blobs": blobs.len(),
                "reclaimed_files": reclaimed_files,
            }),
            ip,
        )
        .await;
        // 共享实体（别的账号也引用了同一份文件）与远端删除失败的留给后台重试
        self.kick_gc();
        tracing::info!(
            uid = %target.uid,
            blobs = blobs.len(),
            files = reclaimed_files,
            "账号已删除，物理文件已按引用计数回收"
        );

        Ok(DeleteUserOutcome {
            email: target.email,
            uid: target.uid,
            blobs: blobs.len(),
            reclaimed_files,
        })
    }

    /// 给某个用户设频率限制覆盖（`None` 表示恢复继承默认）。
    pub async fn admin_set_user_policy(
        &self,
        admin: &User,
        user_id: i64,
        policy: Option<pixora_repo::model::RateLimitPolicy>,
        ip: Option<&str>,
    ) -> Result<()> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        self.ensure_not_install_admin(admin, &target).await?;
        let custom = policy.is_some();
        match policy {
            Some(policy) => {
                self.repo.upsert_user_policy(user_id, &policy).await?;
            }
            None => {
                self.repo.delete_user_policy(user_id).await?;
            }
        }
        self.audit_detail(
            actions::ACCOUNT_UPDATE,
            Some(admin.id),
            &target.uid,
            serde_json::json!({ "scope": "rate_limit", "custom": custom }),
            ip,
        )
        .await;
        Ok(())
    }

    /// 处置通知邮件。暂停与封禁的文案必须分开：
    /// 暂停写「可申请解封」并给材料清单，封禁只写「请联系管理员」。
    async fn send_disposition_mail(
        &self,
        target: &User,
        input: &DispositionInput,
        reason: Option<&str>,
    ) -> Result<crate::mail::MailDelivery> {
        let mail_settings = self.settings.mail().await?;
        let appeal_email = input
            .appeal_email
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                if mail_settings.appeal_email.trim().is_empty() {
                    mail_settings.from_email.clone()
                } else {
                    mail_settings.appeal_email.clone()
                }
            });

        let template = match input.action {
            Disposition::Ban => pixora_mail::template::names::BAN_NOTICE,
            Disposition::Restore => pixora_mail::template::names::SUSPEND_LIFTED,
            Disposition::Suspend => pixora_mail::template::names::SUSPEND_NOTICE,
        };

        let default_requirements = "本人手持身份证的清晰照片；手写承诺书，写明账号邮箱与「承诺遵守服务条款、不再上传违规内容」";
        let vars = serde_json::json!({
            "email": target.email,
            "uid": target.uid,
            "reason": reason.unwrap_or("违反服务条款"),
            "time": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
            "appeal_email": appeal_email,
            "requirements": input
                .requirements
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| default_requirements.to_string()),
        });

        // 渲染、投递、写发件箱都在 dispatch_mail 里，这里不再自己走一遍
        self.dispatch_mail(&target.email, template, vars).await
    }

    async fn admin_count(&self) -> Result<i64> {
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
        Ok(page.total)
    }
}

/// 管理端用户行（契约 8.1 的 `UserRow`）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminUserRow {
    pub id: i64,
    pub uid: String,
    pub email: String,
    /// 邮箱验证状态：详情页那张「已验证 / 未验证」的徽章读它
    pub email_verified: bool,
    pub role: String,
    pub status: String,
    pub status_label: String,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub image_count: i64,
    pub rate_custom: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
    /// 是不是那个访客账号（见 `guest.rs`）：它不接受登录，所以详情页把
    /// 改邮箱、改角色、改密码这几处直接灰掉，而不是让管理员点了吃一句报错
    pub guest: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UserActionRow {
    pub id: i64,
    pub action: String,
    pub action_label: String,
    pub reason: Option<String>,
    pub detail: serde_json::Value,
    pub mail_sent: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PolicyRow {
    pub unlimited: bool,
    pub custom: bool,
    pub api_per_minute: i64,
    pub api_per_hour: i64,
    pub api_read_per_minute: i64,
    pub api_burst: i64,
    pub upload_per_minute: i64,
    pub upload_per_hour: i64,
    pub upload_per_day: i64,
    pub upload_bytes_per_day: i64,
    /// 下面五项是「生效的上传策略」：单用户覆盖 → 全局默认 → 系统设置，
    /// 所以它们可能不等于系统设置里的全局值
    pub max_concurrent_uploads: i64,
    pub max_batch_size: i64,
    pub max_file_size: i64,
    pub max_pixels: i64,
    pub max_width: i64,
    pub max_height: i64,
    pub formats: Vec<String>,
    pub download_per_minute: i64,
    pub download_per_day: i64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminUserDetail {
    pub user: AdminUserRow,
    pub reason: Option<String>,
    pub status_changed_at: Option<String>,
    pub totp_enabled: bool,
    pub email_verified: bool,
    pub actions: Vec<UserActionRow>,
    /// 生效值（三层解析后的结果），只用于展示
    pub policy: PolicyRow,
    /// 该用户自己的覆盖行（未解析的原始值，`0 / 空` 表示跟随）。
    /// 编辑界面回传时要带这一份而不是上面的生效值：生效值是解析结果，
    /// 写回去会把「跟随」固化成「等于当前全局值」的覆盖
    pub policy_override: Option<PolicyRow>,
    /// 这个账号挂着的第三方登录方式（邮箱不在这里，它是账号本身）
    pub identities: Vec<crate::identities::IdentityView>,
}

/// 管理端的会话记录行（cookie 记录）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminSessionRow {
    pub id: i64,
    pub user_id: i64,
    pub uid: String,
    pub email: String,
    pub device: Option<String>,
    pub label: Option<String>,
    pub ip: Option<String>,
    pub created_at: String,
    pub last_seen_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    /// `active` / `saved` / `expired` / `revoked`
    pub state: &'static str,
    pub remember: bool,
    /// 是不是管理员自己当前正在用的会话；后台不允许删它
    pub current: bool,
}

impl AppState {
    /// 管理端会话记录：所有账号的 cookie / 会话记录，可按邮箱、UID、IP、设备搜索。
    pub async fn admin_session_list(
        &self,
        query: Option<&str>,
        page: Option<i64>,
        per_page: Option<i64>,
        current_session_id: Option<i64>,
    ) -> Result<Page<AdminSessionRow>> {
        let request = PageRequest::new(page, per_page, 20, 100);
        let policy = self.session_policy().await;
        let now = Utc::now();
        let page = self.repo.admin_list_sessions(query, request).await?;
        Ok(page.map(|row| {
            let state = row.state(policy, now);
            let remember = row.expires_at.is_some();
            let current = current_session_id == Some(row.id);
            AdminSessionRow {
                id: row.id,
                user_id: row.user_id,
                uid: row.uid,
                email: row.email,
                device: row.device,
                label: row.label,
                ip: row.ip,
                created_at: row.created_at.to_rfc3339(),
                last_seen_at: row.last_seen_at.to_rfc3339(),
                expires_at: row.expires_at.map(|at| at.to_rfc3339()),
                revoked_at: row.revoked_at.map(|at| at.to_rfc3339()),
                state,
                remember,
                current,
            }
        }))
    }

    /// 删除一条会话记录。管理员可以删任何人的，包括正在使用的那条
    /// （对方的浏览器下一次请求就会回到登录页）。
    pub async fn admin_delete_session(&self, session_id: i64) -> Result<u64> {
        Ok(self.repo.delete_session(session_id, None).await?)
    }

    /// 清理全站已经结束的会话记录（已吊销 / 已过期）。
    pub async fn admin_clear_session_history(&self) -> Result<u64> {
        let policy = self.session_policy().await;
        let now = Utc::now();
        let idle_before = (policy.idle_minutes > 0)
            .then(|| now - chrono::Duration::minutes(policy.idle_minutes));
        let created_before = (policy.absolute_days > 0)
            .then(|| now - chrono::Duration::days(policy.absolute_days));
        Ok(self
            .repo
            .delete_session_history(None, now, idle_before, created_before)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_parsing_and_targets() {
        assert_eq!(Disposition::parse("suspend").unwrap(), Disposition::Suspend);
        assert_eq!(Disposition::parse("BAN").unwrap(), Disposition::Ban);
        assert_eq!(Disposition::parse("unban").unwrap(), Disposition::Restore);
        assert!(Disposition::parse("nuke").is_err());

        assert_eq!(Disposition::Suspend.target_status(), "suspended");
        assert_eq!(Disposition::Ban.target_status(), "banned");
        assert_eq!(Disposition::Restore.target_status(), "active");
        assert_eq!(Disposition::Suspend.user_action(), "suspend");
    }

    /// 建一个测试用的管理员。
    async fn make_admin(state: &AppState, email: &str, uid: &str) -> User {
        state
            .repo
            .create_user(&NewUser {
                uid: uid.to_string(),
                email: email.to_string(),
                password_hash: "$argon2id$fake".to_string(),
                role: role::ADMIN.to_string(),
                status: status::ACTIVE.to_string(),
                quota_bytes: 1024 * 1024,
                email_verified: true,
            })
            .await
            .expect("建管理员")
    }

    fn dispose(action: Disposition, minutes: Option<i64>) -> DispositionInput {
        DispositionInput {
            action,
            reason: Some("测试用".to_string()),
            duration_minutes: minutes,
            requirements: None,
            appeal_email: None,
            // 不发邮件：测试环境没有 SMTP，发信只会留一条失败日志
            notify: false,
        }
    }

    /// 处置时长：填了就到点自动恢复，不填就是永久。
    #[tokio::test]
    async fn disposition_duration_sets_auto_restore() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let admin = make_admin(&state, "admin@example.com", "1000").await;
        let user = crate::tests_support::make_user(&state, "user@example.com", "1001").await;

        // 不填时长：永久，没有到期时间
        let outcome = state
            .dispose_user(&admin, user.id, dispose(Disposition::Suspend, None), None)
            .await
            .expect("暂停");
        assert_eq!(outcome.status, status::SUSPENDED);
        assert!(outcome.auto_restore_at.is_none());
        let reloaded = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(
            reloaded.auto_unsuspend_at.is_none(),
            "永久处置不该有到期时间"
        );
        assert!(
            state.lift_expired_suspension(&reloaded).await.is_none(),
            "永久处置只能人工解封"
        );

        // 填了时长：留一个到期时间，到期由鉴权入口放行
        let outcome = state
            .dispose_user(
                &admin,
                user.id,
                dispose(Disposition::Ban, Some(12 * 60)),
                None,
            )
            .await
            .expect("封禁 12 小时");
        assert_eq!(outcome.status, status::BANNED);
        assert!(outcome.auto_restore_at.is_some(), "填了时长就要给到期时间");
        let reloaded = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(reloaded.auto_unsuspend_at.is_some());

        // 把到期时间拨到过去，模拟「时间到了」：这一次访问就该放行
        state
            .repo
            .update_user_status_until(
                user.id,
                status::BANNED,
                reloaded.status_reason.as_deref(),
                None,
                Utc::now() - Duration::hours(13),
                Some(Utc::now() - Duration::minutes(1)),
            )
            .await
            .expect("拨表");
        let expired = state.repo.find_user_by_id(user.id).await.unwrap().unwrap();
        let lifted = state
            .lift_expired_suspension(&expired)
            .await
            .expect("到期应当自动恢复");
        assert_eq!(lifted.status, status::ACTIVE);
    }

    /// 安装时创建的管理员：别的管理员不能改、不能处置、不能切、不能删。
    #[tokio::test]
    async fn install_admin_is_off_limits_for_other_admins() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let owner = make_admin(&state, "owner@example.com", "1000").await;
        let other = make_admin(&state, "other@example.com", "1001").await;
        let plain = crate::tests_support::make_user(&state, "user@example.com", "1002").await;

        state
            .settings
            .set(crate::settings::keys::INSTALL_ADMIN_UID, &owner.uid)
            .await
            .expect("记下安装账号");
        assert_eq!(
            state.install_admin_uid().await.unwrap().as_deref(),
            Some(owner.uid.as_str())
        );

        // 别的管理员：改邮箱、改角色、切进去、删除，四条路都堵上
        assert!(state
            .admin_update_user(
                &other,
                owner.id,
                Some("hijack@example.com"),
                None,
                None,
                None
            )
            .await
            .is_err());
        assert!(state
            .admin_update_user(&other, owner.id, None, None, Some(role::USER), None)
            .await
            .is_err());
        assert!(state
            .ensure_not_install_admin(&other, &owner)
            .await
            .is_err());
        assert!(state
            .admin_delete_user(&other, owner.id, Some(&owner.email), None)
            .await
            .is_err());
        // 处置本来就被「管理员不可处置」挡着，这里确认没有绕过
        assert!(state
            .dispose_user(&other, owner.id, dispose(Disposition::Suspend, None), None)
            .await
            .is_err());

        // 它自己可以改自己
        assert!(state
            .admin_update_user(
                &owner,
                owner.id,
                Some("owner2@example.com"),
                None,
                None,
                None
            )
            .await
            .is_ok());
        // 但它不能把自己降成普通用户，那样这个身份就废了
        assert!(state
            .admin_update_user(&owner, owner.id, None, None, Some(role::USER), None)
            .await
            .is_err());

        // 普通用户不受这条规则影响
        assert!(state
            .admin_delete_user(&other, plain.id, Some(&plain.email), None)
            .await
            .is_ok());
    }

    /// 删账号要先对上邮箱，防止手滑。
    #[tokio::test]
    async fn deleting_a_user_requires_matching_email() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let admin = make_admin(&state, "admin@example.com", "1000").await;
        let user = crate::tests_support::make_user(&state, "user@example.com", "1001").await;

        assert!(state
            .admin_delete_user(&admin, user.id, None, None)
            .await
            .is_err());
        assert!(state
            .admin_delete_user(&admin, user.id, Some("别的邮箱@example.com"), None)
            .await
            .is_err());
        assert!(state.repo.find_user_by_id(user.id).await.unwrap().is_some());

        let outcome = state
            .admin_delete_user(&admin, user.id, Some("User@Example.com"), None)
            .await
            .expect("对上邮箱就能删（大小写不敏感）");
        assert_eq!(outcome.uid, user.uid);
        assert!(state.repo.find_user_by_id(user.id).await.unwrap().is_none());

        // 自己与不存在的账号都删不了
        assert!(state
            .admin_delete_user(&admin, admin.id, Some(&admin.email), None)
            .await
            .is_err());
        assert!(state
            .admin_delete_user(&admin, 999_999, Some("x@example.com"), None)
            .await
            .is_err());
    }
}
