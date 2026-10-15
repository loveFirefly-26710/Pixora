//! 邮件模板的后台管理。
//!
//! 内置文案在 `pixora-mail` 里，管理员改过的存在 `email_templates` 表；
//! 发信时「有存的用存的，没有用内置」（见 `pixora_mail::template::resolve`）。
//! 这一层只做三件事：列出来、存进去、删掉回到内置。
//!
//! 存之前会校验占位符：拼错的变量（`{{emial}}`）在渲染时会被当成「用户没提供」
//! 而静默清掉，信里少一段话还不报错——那种错只有收到信的人才知道。

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::AppState;

/// 后台列表里的一项。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MailTemplateRow {
    pub name: String,
    pub label: String,
    /// 什么时候发这封
    pub when: String,
    /// 正文里能用的占位符
    pub vars: Vec<String>,
    pub subject: String,
    pub body: String,
    /// 管理员改过（库里有行）。false 表示用的是内置文案
    pub customized: bool,
    /// 内置文案，界面上一键「恢复默认」时不必再问服务端
    pub builtin_subject: String,
    pub builtin_body: String,
    pub updated_at: Option<String>,
}

impl AppState {
    /// 全部邮件模板：存的覆盖内置。
    pub async fn mail_template_list(&self) -> Result<Vec<MailTemplateRow>> {
        let stored = self.repo.list_email_templates().await?;
        let mut rows = Vec::with_capacity(pixora_mail::CATALOG.len());
        for spec in pixora_mail::CATALOG {
            let builtin = pixora_mail::template::builtin(spec.name)
                .ok_or_else(|| AppError::internal(format!("模板 {} 没有内置文案", spec.name)))?;
            let saved = stored.iter().find(|row| row.name == spec.name);
            rows.push(MailTemplateRow {
                name: spec.name.to_string(),
                label: spec.label.to_string(),
                when: spec.when.to_string(),
                vars: spec.vars.iter().map(|item| item.to_string()).collect(),
                subject: saved.map(|row| row.subject.clone()).unwrap_or_else(|| builtin.0.to_string()),
                body: saved.map(|row| row.body.clone()).unwrap_or_else(|| builtin.1.to_string()),
                customized: saved.is_some(),
                builtin_subject: builtin.0.to_string(),
                builtin_body: builtin.1.to_string(),
                updated_at: saved.map(|row| row.updated_at.to_rfc3339()),
            });
        }
        Ok(rows)
    }

    /// 保存一个模板。返回改过之后的那一行。
    pub async fn mail_template_save(
        &self,
        admin_id: i64,
        name: &str,
        subject: &str,
        body: &str,
        ip: Option<&str>,
    ) -> Result<MailTemplateRow> {
        let spec = pixora_mail::template::spec(name)
            .ok_or_else(|| AppError::not_found("邮件模板"))?;

        let subject = subject.trim();
        let body = body.trim();
        if subject.is_empty() {
            return Err(AppError::bad_request("主题不能为空"));
        }
        if body.is_empty() {
            return Err(AppError::bad_request("正文不能为空"));
        }
        if subject.len() > 200 {
            return Err(AppError::bad_request("主题太长了，控制在 200 字以内"));
        }
        if body.len() > 8000 {
            return Err(AppError::bad_request("正文太长了，控制在 8000 字以内"));
        }
        // 主题里换行会让邮件客户端显示得很怪，顺手折成一行
        let subject = subject.replace(['\r', '\n'], " ");
        let subject = subject.trim();

        // 占位符白名单：写错了要当场说，不能等到收信人发现少了一段
        for combined in [subject, body] {
            for key in pixora_mail::placeholders(combined) {
                if !spec.vars.contains(&key.as_str()) {
                    return Err(AppError::bad_request(format!(
                        "占位符 {{{{{key}}}}} 这个模板用不上；可用的有：{}",
                        spec.vars
                            .iter()
                            .map(|item| format!("{{{{{item}}}}}"))
                            .collect::<Vec<_>>()
                            .join("、")
                    )));
                }
            }
        }

        self.repo
            .upsert_email_template(spec.name, subject, body)
            .await?;
        self.audit_detail(
            actions::SETTINGS_CHANGE,
            Some(admin_id),
            spec.name,
            serde_json::json!({ "scope": "mail_template", "action": "save" }),
            ip,
        )
        .await;

        let rows = self.mail_template_list().await?;
        rows.into_iter()
            .find(|row| row.name == spec.name)
            .ok_or_else(|| AppError::internal("保存后读不回模板"))
    }

    /// 恢复内置文案。
    pub async fn mail_template_reset(
        &self,
        admin_id: i64,
        name: &str,
        ip: Option<&str>,
    ) -> Result<MailTemplateRow> {
        let spec =
            pixora_mail::template::spec(name).ok_or_else(|| AppError::not_found("邮件模板"))?;
        self.repo.delete_email_template(spec.name).await?;
        self.audit_detail(
            actions::SETTINGS_CHANGE,
            Some(admin_id),
            spec.name,
            serde_json::json!({ "scope": "mail_template", "action": "reset" }),
            ip,
        )
        .await;

        let rows = self.mail_template_list().await?;
        rows.into_iter()
            .find(|row| row.name == spec.name)
            .ok_or_else(|| AppError::internal("恢复后读不回模板"))
    }
}

#[cfg(test)]
mod tests {
    /// 列出来的是「内置 + 改过的」，改完立刻生效，恢复默认回到内置。
    #[tokio::test]
    async fn templates_can_be_edited_and_reset() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;

        let rows = state.mail_template_list().await.unwrap();
        assert_eq!(rows.len(), pixora_mail::CATALOG.len(), "每个内置模板都该列出来");
        let verify = rows.iter().find(|row| row.name == "verify_code").unwrap();
        assert!(!verify.customized, "一开始都是内置");
        assert!(verify.body.contains("{{code}}"), "内置文案带着占位符");
        assert_eq!(verify.vars, ["email", "code"]);

        // 改一版
        let saved = state
            .mail_template_save(
                1,
                "verify_code",
                "【Pixora】登录码",
                "你的验证码是 {{code}}，10 分钟内有效。",
                None,
            )
            .await
            .unwrap();
        assert!(saved.customized);
        assert_eq!(saved.subject, "【Pixora】登录码");
        assert!(saved.updated_at.is_some());

        // 真的生效：渲染时用新的那份
        let stored = state
            .repo
            .find_email_template("verify_code")
            .await
            .unwrap()
            .map(|row| (row.subject, row.body));
        let (subject, body) = pixora_mail::template::resolve("verify_code", stored);
        assert_eq!(subject, "【Pixora】登录码");
        assert!(body.contains("你的验证码是"));

        // 恢复默认：库里那行删掉，回到内置
        let reset = state
            .mail_template_reset(1, "verify_code", None)
            .await
            .unwrap();
        assert!(!reset.customized);
        assert!(state
            .repo
            .find_email_template("verify_code")
            .await
            .unwrap()
            .is_none());
    }

    /// 拼错的占位符要当场拒掉：渲染时它会被静默清掉，收信人才发现少一段话。
    #[tokio::test]
    async fn unknown_placeholders_are_rejected() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = crate::tests_support::test_state(&dir).await;
        let admin = crate::tests_support::make_user(&state, "admin@example.com", "1000").await;

        let err = state
            .mail_template_save(
                admin.id,
                "verify_code",
                "【Pixora】验证码",
                "你的验证码是 {{emial}}",
                None,
            )
            .await
            .unwrap_err();
        assert!(err.message().contains("emial"), "{}", err.message());
        assert!(err.message().contains("{{email}}"), "要告诉它哪些能用");

        // 主题与正文都查
        assert!(state
            .mail_template_save(admin.id, "verify_code", "{{nope}}", "正文", None)
            .await
            .is_err());
        // 别的模板的变量也算「用不上」
        assert!(state
            .mail_template_save(admin.id, "verify_code", "主题", "{{link}}", None)
            .await
            .is_err());
        // 空主题/空正文不收
        assert!(state
            .mail_template_save(admin.id, "verify_code", "   ", "正文", None)
            .await
            .is_err());
        // 不认识的模板名
        assert!(state
            .mail_template_save(admin.id, "no_such_template", "主题", "正文", None)
            .await
            .is_err());
    }
}
