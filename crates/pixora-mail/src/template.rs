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
    /// 管理员直接给用户设了新密码：把密码与「下次登录必须改」一起告知
    pub const PASSWORD_SET_BY_ADMIN: &str = "password_set_by_admin";
    /// 登录邮箱被管理员改了：新旧两个地址各收到一封
    pub const EMAIL_CHANGED: &str = "email_changed";
    pub const SUSPEND_NOTICE: &str = "suspend_notice";
    pub const BAN_NOTICE: &str = "ban_notice";
    pub const SUSPEND_LIFTED: &str = "suspend_lifted";
    /// 内容审核拦下图后的管理员上报
    pub const MODERATION_ALERT: &str = "moderation_alert";
}

/// 一个可编辑模板的说明。后台的「邮件模板」页按它列清单、给变量提示。
pub struct TemplateSpec {
    pub name: &'static str,
    /// 中文名，列表里给人看的
    pub label: &'static str,
    /// 什么时候发这封
    pub when: &'static str,
    /// 正文里能用的占位符（不含花括号）
    pub vars: &'static [&'static str],
}

/// 全部可编辑模板。顺序就是后台列表里的顺序：按「用户会收到它的频率」排。
pub const CATALOG: &[TemplateSpec] = &[
    TemplateSpec {
        name: names::VERIFY_CODE,
        label: "邮箱验证码",
        when: "注册、以及关闭二次验证时的邮箱验证码",
        vars: &["email", "code"],
    },
    TemplateSpec {
        name: names::RESET_PASSWORD,
        label: "重置密码",
        when: "用户自助找回密码，或管理员代发重置邮件",
        vars: &["email", "code", "link"],
    },
    TemplateSpec {
        name: names::PASSWORD_SET_BY_ADMIN,
        label: "管理员重置了密码",
        when: "管理员直接给用户设了新密码",
        vars: &["email", "uid", "password", "appeal_email"],
    },
    TemplateSpec {
        name: names::EMAIL_CHANGED,
        label: "邮箱变更通知",
        when: "管理员改了某个账号的登录邮箱（新旧地址各一封）",
        vars: &["uid", "old_email", "new_email", "time", "appeal_email"],
    },
    TemplateSpec {
        name: names::SUSPEND_NOTICE,
        label: "账号被暂停",
        when: "管理员暂停账号时",
        vars: &["email", "uid", "time", "reason", "requirements", "appeal_email"],
    },
    TemplateSpec {
        name: names::BAN_NOTICE,
        label: "账号被封禁",
        when: "管理员封禁账号时",
        vars: &["email", "uid", "time", "reason", "appeal_email"],
    },
    TemplateSpec {
        name: names::SUSPEND_LIFTED,
        label: "账号恢复正常",
        when: "管理员解封或临时暂停到期时",
        vars: &["email", "uid", "time"],
    },
    TemplateSpec {
        name: names::MODERATION_ALERT,
        label: "内容审核上报",
        when: "内容审核拦下一张图，通知管理员",
        vars: &["email", "uid", "label", "score", "threshold", "provider", "name", "time", "count", "ban_after"],
    },
];

/// 按名字找模板说明。
pub fn spec(name: &str) -> Option<&'static TemplateSpec> {
    CATALOG.iter().find(|item| item.name == name)
}

/// 正文里出现的占位符。用来挡住 `{{emial}}` 这类拼错的变量——
/// 拼错的占位符会被当成「用户没提供」而静默清掉，信里就少一段话。
pub fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let key = after[..end].trim().to_string();
        if !key.is_empty() && !found.contains(&key) {
            found.push(key);
        }
        rest = &after[end + 2..];
    }
    found
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

/// 拼链接用的最小百分号编码：只留 URL 里绝对安全的那几个字符。
///
/// 邮件里的链接会被各种客户端二次处理，`+`（在查询串里等于空格）和 `@`
/// 这类字符不编码就会把邮箱地址改坏。不引入依赖，够用就行。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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
             {{link}}\n\n\
             也可以在本站的「忘记密码」页输入这串验证码：{{code}}（10 分钟内有效）\n\n\
             链接是一次性的，用过即废。如果这不是你本人的操作，忽略本邮件即可，密码不会变。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\
             Pixora 管理团队",
        )),
        names::PASSWORD_SET_BY_ADMIN => Some((
            "【Pixora】管理员已为你重置密码",
            "你好，\n\n\
             管理员已为 {{email}}（UID {{uid}}）设置了一个新密码：\n\n\
             {{password}}\n\n\
             这个密码是经邮件送达的，之后请自己改一个只有你知道的：下次登录时系统会要求你改，\
             没改之前账号只能访问「修改密码」页。\n\n\
             如果这不是你要的，请联系 {{appeal_email}}。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\
             Pixora 管理团队",
        )),
        names::EMAIL_CHANGED => Some((
            "【Pixora】账号邮箱变更通知",
            "你好，

             账号（UID {{uid}}）的登录邮箱已由 {{old_email}} 改为 {{new_email}}。

             操作时间：{{time}}

             如果这是你要的，忽略本邮件即可。如果不是你安排的，请立刻联系 {{appeal_email}}——             邮箱是找回密码的唯一凭据，换了它就等于换了账号的钥匙。

             本邮件由系统自动发出，请勿直接回复。
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
        names::MODERATION_ALERT => Some((
            "【Pixora】内容审核拦下了一张图片",
            "管理员你好，\n\n\
             有一张上传被内容审核拦下了：\n\n\
             用户：{{email}}（UID {{uid}}）\n\
             判定：{{label}} · {{score}} 分（阈值 {{threshold}}，服务商 {{provider}}）\n\
             文件名：{{name}}\n\
             时间：{{time}}\n\n\
             该用户 24 小时内的违规次数：{{count}} 次。累计到 {{ban_after}} 次会自动临时暂停账号。\n\n\
             策略可在后台「系统设置 → 内容审核」调整；审计日志里有每次拦截的完整记录。\n\n\
             本邮件由系统自动发出，请勿直接回复。\n\
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

    /// 重置邮件里的链接：能直接点开，且邮箱与验证码都带在查询串上。
    #[test]
    fn reset_mail_carries_a_clickable_link() {
        let vars: MailVars = [
            ("email".to_string(), "user@example.com".to_string()),
            ("code".to_string(), "123456".to_string()),
            (
                "link".to_string(),
                "点开这个链接直接设置新密码：\nhttps://img.example.com/#/reset-password?email=user%40example.com&code=123456"
                    .to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let (_, body) = builtin(names::RESET_PASSWORD).expect("内置模板");
        let rendered = render(body, &vars);

        assert!(rendered.contains("https://img.example.com/#/reset-password?email=user%40example.com&code=123456"));
        assert!(rendered.contains("123456"), "验证码也留着，链接点不开时还能手输");
        assert!(!rendered.contains("{{"), "占位符要全部换掉：{rendered}");
    }

    /// 站点地址没配时不硬塞链接：那种情况下 `{{link}}` 是空串。
    #[test]
    fn reset_mail_without_link_still_reads_well() {
        let vars: MailVars = [
            ("email".to_string(), "user@example.com".to_string()),
            ("code".to_string(), "654321".to_string()),
            ("link".to_string(), String::new()),
        ]
        .into_iter()
        .collect();
        let (_, body) = builtin(names::RESET_PASSWORD).expect("内置模板");
        let rendered = render(body, &vars);

        assert!(rendered.contains("654321"));
        assert!(!rendered.contains("{{"));
        assert!(!rendered.contains("点开这个链接"));
    }

    #[test]
    fn url_encode_keeps_the_query_intact() {
        assert_eq!(url_encode("user@example.com"), "user%40example.com");
        assert_eq!(url_encode("a+b-c_d.e~f"), "a%2Bb-c_d.e~f");
    }

    /// 管理员设密码那封：密码要写在正文里，并说清下次登录得自己改。
    #[test]
    fn password_set_mail_states_the_password_and_the_forced_change() {
        let vars: MailVars = [
            ("email".to_string(), "user@example.com".to_string()),
            ("uid".to_string(), "1002".to_string()),
            ("password".to_string(), "AdminSet-2026x".to_string()),
            ("appeal_email".to_string(), "admin@example.com".to_string()),
        ]
        .into_iter()
        .collect();
        let (_, body) = builtin(names::PASSWORD_SET_BY_ADMIN).expect("内置模板");
        let rendered = render(body, &vars);

        assert!(rendered.contains("AdminSet-2026x"));
        assert!(rendered.contains("下次登录"));
    }
}
