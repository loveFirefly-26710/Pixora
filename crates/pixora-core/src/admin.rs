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
    /// 生效时长（天）。`None` 表示永久，需要人工解封。
    pub duration_days: Option<i64>,
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
}

impl AppState {
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
            id: row.id,
            uid: row.uid,
            email: row.email,
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
                role: summary.role,
                status: summary.status.clone(),
                status_label: pixora_repo::model::status::label(&summary.status).to_string(),
                quota_bytes: summary.quota_bytes,
                used_bytes: summary.used_bytes,
                image_count: summary.image_count,
                rate_custom: summary.rate_custom,
                created_at: summary.created_at.to_rfc3339(),
                last_login_at: summary.last_login_at.map(|at| at.to_rfc3339()),
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
                formats: upload.formats,
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
                formats: row
                    .formats
                    .as_deref()
                    .map(crate::settings::parse_format_list)
                    .unwrap_or_default(),
                note: row.note.clone(),
            }),
        })
    }

    /// 管理员改用户资料：邮箱、密码、角色。
    pub async fn admin_update_user(
        &self,
        admin: &User,
        user_id: i64,
        email: Option<&str>,
        password: Option<&str>,
        role_value: Option<&str>,
        ip: Option<&str>,
    ) -> Result<AdminUserDetail> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;

        let mut detail_changes = serde_json::Map::new();

        if let Some(email) = email {
            let email = crate::auth::normalize_email_input(email)?;
            if !email.eq_ignore_ascii_case(&target.email) {
                if self.repo.email_exists(&email).await? {
                    return Err(AppError::Conflict("该邮箱已被其他账号使用".into()));
                }
                // 改邮箱会重置验证状态：管理员填错一个字就把账号锁死过，
                // 那次之后这条规则就固定下来了
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
            }
        }

        if let Some(password) = password {
            pixora_crypto::password::check_strength(password)?;
            let hash = pixora_crypto::password::hash_password(password)?;
            self.repo.update_user_password(user_id, &hash).await?;
            // 重置密码后旧会话必须失效，否则账号可能仍被别人用着
            let revoked = self.repo.revoke_all_sessions(user_id, Utc::now()).await?;
            detail_changes.insert("password_reset".into(), serde_json::json!(revoked));
            self.repo
                .insert_user_action(
                    user_id,
                    Some(admin.id),
                    "reset_password",
                    Some("管理员重置密码"),
                    &serde_json::json!({ "revoked_sessions": revoked }),
                    false,
                    Utc::now(),
                )
                .await?;
        }

        if let Some(role_value) = role_value {
            if role_value != role::ADMIN && target.is_admin() {
                let admins = self.admin_count().await?;
                if admins <= 1 {
                    return Err(AppError::bad_request("至少要保留一个管理员"));
                }
            }
            self.repo.update_user_role(user_id, role_value).await?;
            detail_changes.insert("role".into(), serde_json::json!(role_value));
        }

        if !detail_changes.is_empty() {
            self.audit_detail(
                actions::SETTINGS_CHANGE,
                Some(admin.id),
                &target.uid,
                serde_json::Value::Object(detail_changes),
                ip,
            )
            .await;
        }
        self.admin_user_detail(user_id).await
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

        let now = Utc::now();
        let new_status = input.action.target_status();
        let reason = input
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        if input.action != Disposition::Restore && reason.is_none() {
            return Err(AppError::bad_request("请填写处置原因，它会展示给用户"));
        }

        self.repo
            .update_user_status(user_id, new_status, reason.as_deref(), Some(admin.id), now)
            .await?;

        // 处置后立即踢下线。API Token 不删——鉴权时按状态拦，解封即恢复。
        let revoked = self.repo.revoke_all_sessions(user_id, now).await? as i64;

        // 生效时长：界面上写「永久 / 7 天 / 30 天」，这里只在记录里留痕，
        // 到期不自动恢复（自动恢复会让「暂停」看起来像临时限流，语义就乱了）
        let expires_at = input
            .duration_days
            .filter(|days| *days > 0)
            .map(|days| now + Duration::days(days));

        let detail = serde_json::json!({
            "from": target.status,
            "to": new_status,
            "duration_days": input.duration_days,
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

        // 通知邮件：发件地址取注册发信邮箱，不额外配一个申诉邮箱
        let mut mail_sent = false;
        let mut mail_error = None;
        if input.notify && input.action != Disposition::Restore {
            match self
                .send_disposition_mail(&target, &input, reason.as_deref())
                .await
            {
                Ok(()) => mail_sent = true,
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
        let password_hash = pixora_crypto::password::hash_password(password)?;
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

    /// 删除账号。内容随外键级联删除，物理文件由 GC 按引用计数回收。
    pub async fn admin_delete_user(
        &self,
        admin: &User,
        user_id: i64,
        ip: Option<&str>,
    ) -> Result<()> {
        let target = self
            .repo
            .find_user_by_id(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        if target.is_admin() {
            return Err(AppError::bad_request("不能直接删除管理员账号"));
        }
        if target.id == admin.id {
            return Err(AppError::bad_request("不能删除自己的账号"));
        }

        self.audit_detail(
            actions::ACCOUNT_DELETE,
            Some(admin.id),
            &target.uid,
            serde_json::json!({ "email": target.email, "used_bytes": target.used_bytes }),
            ip,
        )
        .await;
        self.repo.delete_user(user_id).await?;
        // 级联删掉的内容对应的实体可能直接归零：催一次回收
        self.kick_gc();
        Ok(())
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
        match policy {
            Some(policy) => {
                self.repo.upsert_user_policy(user_id, &policy).await?;
            }
            None => {
                self.repo.delete_user_policy(user_id).await?;
            }
        }
        self.audit_detail(
            actions::SETTINGS_CHANGE,
            Some(admin.id),
            &target.uid,
            serde_json::json!({ "scope": "rate_limit" }),
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
    ) -> Result<()> {
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

        let outbox_id = self
            .repo
            .enqueue_mail(&target.email, template, &vars)
            .await?;

        let Some(mailer) = self.mailer() else {
            return Err(AppError::internal(
                "SMTP 未配置，通知邮件只留在发件箱里等待重试",
            ));
        };
        let stored = self
            .repo
            .find_email_template(template)
            .await?
            .map(|row| (row.subject, row.body));
        let (subject, body) = pixora_mail::template::resolve(template, stored);

        let mut map = pixora_mail::MailVars::new();
        if let Some(object) = vars.as_object() {
            for (key, value) in object {
                map.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
            }
        }
        let subject = pixora_mail::render_subject(&subject, &map);
        let body = pixora_mail::render(&body, &map);

        match mailer.send(&target.email, &subject, &body).await {
            Ok(()) => {
                self.repo.mark_mail_sent(outbox_id, Utc::now()).await?;
                Ok(())
            }
            Err(err) => {
                let next = Utc::now() + Duration::minutes(5);
                self.repo
                    .mark_mail_failed(outbox_id, &err.to_string(), Some(next), false)
                    .await?;
                Err(AppError::internal(err.to_string()))
            }
        }
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
    pub role: String,
    pub status: String,
    pub status_label: String,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub image_count: i64,
    pub rate_custom: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
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
    pub formats: Vec<String>,
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
}
