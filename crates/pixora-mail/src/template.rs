//! 邮件模板渲染与内置模板。
//!
//! 模板存数据库（`email_templates`），管理员改完即时生效；库里没有对应行时
//! 用这里的内置版本兜底，保证全新实例也能发出可读的邮件。
//!
//! 占位符是 `{{name}}`，只做纯文本替换——不引入模板引擎，因为它带来的是
//! 「可以写逻辑」的能力，而我们只需要填空。

use std::collections::HashMap;

/// 模板变量。键名与模板里的占位符一一对应。
pub type MailVars = HashMap<String, String>;

/// 模板名。与 `email_templates.name` 列一致。
pub mod names {
    pub const VERIFY_CODE: &str = "verify_code";
    pub const RESET_PASSWORD: &str = "reset_password";
    pub const SUSPEND_NOTICE: &str = "suspend_notice";
    pub const BAN_NOTICE: &str = "ban_notice";
    pub const SUSPEND_LIFTED: &str = "suspend_lifted";
}

/// 渲染正文。未提供的占位符替换成空串而不是保留 `{{x}}`，
/// 免得用户收到「你好 {{email}}」这种半成品。
pub fn render(template: &str, vars: &MailVars) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    // 清掉没被替换的占位符
    while let Some(start) = out.find("{{") {
        match out[start..].find("}}") {
            Some(end) => out.replace_range(start..start + end + 2, ""),
            None => break,
        }
    }
    out
}

/// 渲染主题行。主题里通常只有一个变量（验证码或账号）。
pub fn render_subject(template: &str, vars: &MailVars) -> String {
    render(template, vars).replace('\n', " ").trim().to_string()
}

/// 内置模板（主题, 正文）。库里没有同名行时使用。
pub fn builtin(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        names::VERIFY_CODE => Some((
            "【Pixora】邮箱验证码",
            "你好，\n\n\
             你正在验证邮箱 {{email}}。\n\n\
             验证码：{{code}}\n\n\
             验证码 10 分钟内有效，同一邮箱每小时最多 5 次。请勿把验证码告诉任何人。\n\n\
             如果这不是你本人的操作，忽略本邮件即可。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\
             Pixora 管理团队",
        )),
        names::RESET_PASSWORD => Some((
            "【Pixora】重置密码",
            "你好，\n\n\
             我们收到了重置 {{email}} 账号密码的请求。\n\n\
             验证码：{{code}}\n\n\
             验证码 10 分钟内有效。如果这不是你本人的操作，请立即修改密码并检查活跃会话。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\
             Pixora 管理团队",
        )),
        names::SUSPEND_NOTICE => Some((
            "【Pixora】你的账号已被暂停使用 - 解封指引",
            "你好，\n\n\
             你的 Pixora 账号（{{email}}，UID {{uid}}）已于 {{time}} 被暂停使用。\n\n\
             暂停原因：{{reason}}\n\n\
             暂停期间，你的账号无法通过网页登录，也无法调用 API 上传。你已上传的内容会被保留，\
             公开内容将暂时隐藏，解封后自动恢复。\n\n\
             如何解封\n\
             1. 准备材料：{{requirements}}\n\
             2. 将材料作为附件发送至 {{appeal_email}}\n\
             3. 邮件主题格式：解封申请 - {{email}}\n\n\
             我们会在 1-3 个工作日内审核并邮件回复结果。如果你认为这是误判，请在邮件中说明情况。\n\n\
             本邮件由系统自动发出，请勿直接回复本地址。申诉请发至 {{appeal_email}}。\n\n\
             Pixora 管理团队",
        )),
        // 封禁的文案不承诺解封，也不给材料清单——否则会给出错误预期
        names::BAN_NOTICE => Some((
            "【Pixora】你的账号已被封禁",
            "你好，\n\n\
             你的 Pixora 账号（{{email}}，UID {{uid}}）已于 {{time}} 被封禁。\n\n\
             封禁原因：{{reason}}\n\n\
             封禁期间，网页登录与 API 调用全部失效，账号下的公开内容已隐藏。\n\n\
             该处置需要管理员解封，请联系 {{appeal_email}}。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\n\
             Pixora 管理团队",
        )),
        names::SUSPEND_LIFTED => Some((
            "【Pixora】账号已恢复正常",
            "你好，\n\n\
             你的 Pixora 账号（{{email}}）已于 {{time}} 恢复正常，网页登录与 API 调用均已可用。\n\n\
             账号下已上传的内容已重新可见。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\n\
             Pixora 管理团队",
        )),
        _ => None,
    }
}

/// 取模板内容：库里优先，没有就用内置。
pub fn resolve(
    name: &str,
    stored: Option<(String, String)>,
) -> (String, String) {
    match stored {
        Some((subject, body)) if !body.trim().is_empty() => (subject, body),
        _ => match builtin(name) {
            Some((subject, body)) => (subject.to_string(), body.to_string()),
            None => (
                "【Pixora】通知".to_string(),
                "{{content}}".to_string(),
            ),
        },
    }
}

/// 常见变量的便捷构造。
pub fn vars() -> MailVars {
    MailVars::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vars() -> MailVars {
        let mut vars = MailVars::new();
        vars.insert("email".into(), "spam@tempmail.xyz".into());
        vars.insert("uid".into(), "4120".into());
        vars.insert("reason".into(), "上传违规内容，违反服务条款第 3 条".into());
        vars.insert("time".into(), "2026-10-14 15:30".into());
        vars.insert("appeal_email".into(), "admin@example.com".into());
        vars.insert("requirements".into(), "本人手持身份证的清晰照片".into());
        vars
    }

    #[test]
    fn placeholders_are_substituted() {
        let rendered = render("账号 {{email}}（UID {{uid}}）", &sample_vars());
        assert_eq!(rendered, "账号 spam@tempmail.xyz（UID 4120）");
    }

    #[test]
    fn unknown_placeholders_are_removed() {
        let rendered = render("你好 {{email}}，{{unknown}}再见", &sample_vars());
        assert_eq!(rendered, "你好 spam@tempmail.xyz，再见");
    }

    #[test]
    fn suspend_notice_matches_prototype_text() {
        let (subject, body) = builtin(names::SUSPEND_NOTICE).unwrap();
        assert!(subject.contains("解封指引"));
        let rendered = render(body, &sample_vars());
        assert!(rendered.contains("spam@tempmail.xyz"));
        assert!(rendered.contains("解封申请 - spam@tempmail.xyz"));
        assert!(rendered.contains("1-3 个工作日"));
        assert!(rendered.contains("你已上传的内容会被保留"));
        // 暂停必须给材料清单
        assert!(rendered.contains("本人手持身份证"));
    }

    #[test]
    fn ban_notice_does_not_promise_unban() {
        let (_, body) = builtin(names::BAN_NOTICE).unwrap();
        let rendered = render(body, &sample_vars());
        assert!(rendered.contains("联系"));
        assert!(!rendered.contains("如何解封"), "封禁邮件不该出现解封指引");
        assert!(!rendered.contains("解封申请 -"), "封禁邮件不该给申诉主题模板");
    }

    #[test]
    fn stored_template_wins_over_builtin() {
        let stored = Some(("自定义主题".to_string(), "自定义正文 {{email}}".to_string()));
        let (subject, body) = resolve(names::VERIFY_CODE, stored);
        assert_eq!(subject, "自定义主题");
        assert_eq!(body, "自定义正文 {{email}}");

        let (subject, _) = resolve(names::VERIFY_CODE, Some((String::new(), String::new())));
        assert_eq!(subject, "【Pixora】邮箱验证码", "空模板回落到内置");
    }

    #[test]
    fn subject_is_single_line() {
        let mut vars = MailVars::new();
        vars.insert("code".into(), "482913".into());
        let subject = render_subject("验证码：{{code}}\n换行会被去掉", &vars);
        assert_eq!(subject, "验证码：482913 换行会被去掉");
    }
}
