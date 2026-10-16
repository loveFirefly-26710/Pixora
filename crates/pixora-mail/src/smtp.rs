//! SMTP 投递。
//!
//! 默认预置阿里云企业邮箱参数（`smtp.qiye.aliyun.com:465`，SSL），
//! 也支持任意标准 SMTP 与 STARTTLS。密码从库里解密后传进来，
//! 这个结构体本身不碰加密。
//!
//! 送达率是自建 SMTP 的短板，所以除了「发得出去」还要有「连不上时能说清楚」：
//! [`Mailer::verify`] 直接建连并握手，把底层错误原样带出来，管理员在后台点
//! 「测试发信」就能看到到底是端口不通、证书不对还是账号密码错。

use lettre::message::{header, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MailError {
    #[error("邮件地址无效：{0}")]
    Address(String),

    #[error("邮件内容构建失败：{0}")]
    Build(String),

    #[error("SMTP 连接或投递失败：{0}")]
    Transport(String),

    /// 服务器明确拒收（SMTP 5xx）：地址不存在、被对方拉黑这类。重发多少次
    /// 结果都一样，所以不能跟连接抖动混在一起——重试只是把退信一封封堆进
    /// 发信账号。
    #[error("收件服务器拒收：{0}")]
    Rejected(String),

    /// 服务器拒了这组凭据。单独成一类：这种错九成是配置问题，
    /// 管理员照着清单能自己改好，不该让他去猜「连接失败」是什么意思。
    #[error("邮箱认证失败（{0}）：发信账号或密码不对。挨个核对：1) 发信账号要填完整邮箱地址；\
2) 密码填邮箱登录密码，若邮箱开了「客户端专用密码 / 授权码」则填那个；\
3) 确认该邮箱没有关闭 SMTP 发信权限（企业邮箱后台通常有这一项）")]
    Auth(String),

    #[error("SMTP 未配置：{0}")]
    NotConfigured(String),
}

impl MailError {
    /// 这封信还值不值得重试。
    ///
    /// 拒收与认证失败都是一次性的结论：地址不对，发一百次还是不对；
    /// 密码错了也不会自己变对。只有网络、超时、TLS 这类才算「待会儿再试」。
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::Auth(_))
    }

    /// 把 lettre 的错误归类。认证失败在 SMTP 里是 5xx 永久错误，
    /// 常见码：535（标准）、526（阿里云的叫法）、530/534/538。
    pub fn from_transport(err: &lettre::transport::smtp::Error) -> Self {
        let text = err.to_string();
        let lower = text.to_ascii_lowercase();
        let auth_code = ["535", "526", "530", "534", "538"]
            .iter()
            .any(|code| text.contains(code));
        if auth_code || lower.contains("authentication") {
            return Self::Auth(text);
        }
        // 其余 5xx：对方服务器把 MAIL FROM / RCPT TO / DATA 这几步直接拒了。
        // 4xx 是「待会儿再来」，才值得重试
        if err.is_permanent() {
            return Self::Rejected(text);
        }
        Self::Transport(text)
    }
}

pub type Result<T> = std::result::Result<T, MailError>;

/// 传输安全方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailSecurity {
    /// 直接 TLS（465 端口，阿里云企业邮箱默认）
    Ssl,
    /// STARTTLS 升级（587 端口）
    StartTls,
    /// 明文（仅用于内网自建邮件服务）
    None,
}

impl MailSecurity {
    pub fn parse(input: &str) -> Self {
        match input.trim().to_ascii_lowercase().as_str() {
            "starttls" | "tls" => Self::StartTls,
            "none" | "plain" | "insecure" => Self::None,
            _ => Self::Ssl,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssl => "ssl",
            Self::StartTls => "starttls",
            Self::None => "none",
        }
    }

    /// 没填端口时按安全方式给一个合理默认。
    pub fn default_port(self) -> u16 {
        match self {
            Self::Ssl => 465,
            Self::StartTls => 587,
            Self::None => 25,
        }
    }
}

/// SMTP 配置。字段与后台「邮件与注册验证」表单一一对应。
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub host: String,
    pub port: u16,
    pub security: MailSecurity,
    pub username: String,
    pub password: String,
    /// 发信人显示名与地址。发件地址就是注册时发验证码的那个账号，
    /// 不给申诉单独配一个邮箱——管理员会漏看。
    pub from_name: String,
    pub from_email: String,
    /// 超时（秒），网络差的环境可以调大
    pub timeout_seconds: u64,
}

impl MailConfig {
    pub fn new(host: &str, username: &str, password: &str, from_email: &str) -> Self {
        Self {
            host: host.to_string(),
            port: MailSecurity::Ssl.default_port(),
            security: MailSecurity::Ssl,
            username: username.to_string(),
            password: password.to_string(),
            from_name: "Pixora".to_string(),
            from_email: from_email.to_string(),
            timeout_seconds: 20,
        }
    }

    /// 阿里云企业邮箱的预置参数。
    pub fn aliyun(username: &str, password: &str) -> Self {
        let mut config = Self::new("smtp.qiye.aliyun.com", username, password, username);
        config.port = 465;
        config.security = MailSecurity::Ssl;
        config
    }

    /// 配置是否完整到可以尝试发信。
    pub fn is_usable(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.from_email.trim().is_empty()
    }

    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
        if !self.is_usable() {
            return Err(MailError::NotConfigured(
                "缺少 SMTP 服务器、账号或发件地址".into(),
            ));
        }

        let builder = match self.security {
            MailSecurity::Ssl => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host)
                .map_err(|err| MailError::Transport(err.to_string()))?,
            MailSecurity::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host)
                .map_err(|err| MailError::Transport(err.to_string()))?,
            MailSecurity::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.host),
        };

        Ok(builder
            .port(self.port)
            .credentials(Credentials::new(
                self.username.clone(),
                self.password.clone(),
            ))
            .timeout(Some(std::time::Duration::from_secs(self.timeout_seconds)))
            .build())
    }
}

/// 邮件发送器。每次发送都新建连接（图床的邮件量很小，不值得维护连接池）。
pub struct Mailer {
    config: MailConfig,
}

impl Mailer {
    pub fn new(config: MailConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &MailConfig {
        &self.config
    }

    /// 发一封纯文本邮件。正文用等宽字体包一层 HTML，
    /// 让 Apple Mail 这类客户端不会把多行正文挤成一团。
    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        let message = self.build_message(to, subject, body)?;
        let transport = self.config.transport()?;
        transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|err| MailError::from_transport(&err))
    }

    fn build_message(&self, to: &str, subject: &str, body: &str) -> Result<Message> {
        let from = format!("{} <{}>", self.config.from_name, self.config.from_email)
            .parse()
            .map_err(|err: lettre::address::AddressError| MailError::Address(err.to_string()))?;
        let to: lettre::message::Mailbox = to
            .parse()
            .map_err(|err: lettre::address::AddressError| MailError::Address(err.to_string()))?;

        Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .header(header::ContentType::TEXT_PLAIN)
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(body.to_string()))
                    .singlepart(SinglePart::html(plain_text_to_html(body))),
            )
            .map_err(|err| MailError::Build(err.to_string()))
    }

    /// 连通性自检：建立连接并握手，不真的发信。
    pub async fn verify(&self) -> Result<String> {
        let transport = self.config.transport()?;
        transport
            .test_connection()
            .await
            .map(|_| {
                format!(
                    "连通 · {}:{}（{}）",
                    self.config.host,
                    self.config.port,
                    self.config.security.as_str()
                )
            })
            .map_err(|err| MailError::from_transport(&err))
    }

    /// 真发一封测试信，管理后台的「测试发信」按钮走这个。
    pub async fn send_test(&self, to: &str) -> Result<()> {
        let body = format!(
            "这是 Pixora 的测试邮件。\n\n\
             如果你收到了它，说明 SMTP 配置可用：\n\
             服务器 {}:{}\n\
             安全方式 {}\n\n\
             本邮件由系统自动发出，请勿直接回复。",
            self.config.host,
            self.config.port,
            self.config.security.as_str()
        );
        self.send(to, "【Pixora】测试发信", &body).await
    }
}

/// 把纯文本正文转成最简 HTML：换行保留，其余内容原样。
///
/// 不做 Markdown 渲染是刻意的——正文里可能出现用户填写的解封要求，
/// 任何富文本处理都是给自己引入 XSS 的机会。
fn plain_text_to_html(body: &str) -> String {
    let escaped = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<pre style=\"font-family:ui-monospace,Consolas,monospace;font-size:13px;line-height:1.7;white-space:pre-wrap\">{escaped}</pre>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_parsing_and_defaults() {
        assert_eq!(MailSecurity::parse("ssl"), MailSecurity::Ssl);
        assert_eq!(MailSecurity::parse("STARTTLS"), MailSecurity::StartTls);
        assert_eq!(MailSecurity::parse("none"), MailSecurity::None);
        assert_eq!(MailSecurity::Ssl.default_port(), 465);
        assert_eq!(MailSecurity::StartTls.default_port(), 587);
    }

    #[test]
    fn aliyun_preset_matches_design_doc() {
        let config = MailConfig::aliyun("noreply@example.com", "secret");
        assert_eq!(config.host, "smtp.qiye.aliyun.com");
        assert_eq!(config.port, 465);
        assert_eq!(config.security, MailSecurity::Ssl);
        assert!(config.is_usable());
    }

    #[test]
    fn incomplete_config_is_rejected() {
        let mut config = MailConfig::aliyun("noreply@example.com", "secret");
        config.host = String::new();
        assert!(!config.is_usable());
        assert!(matches!(
            config.transport(),
            Err(MailError::NotConfigured(_))
        ));
    }

    #[test]
    fn message_builds_with_plain_and_html_parts() {
        let mailer = Mailer::new(MailConfig::aliyun("noreply@example.com", "secret"));
        // 正文用 ASCII 便于断言字面量：中文会被 MIME 编码，字面量搜不到
        let message = mailer
            .build_message("user@example.com", "[Pixora] verify", "your code is 482913")
            .unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();
        assert!(raw.contains("text/plain"), "{raw}");
        assert!(raw.contains("text/html"), "{raw}");
        assert!(raw.contains("482913"), "{raw}");
        assert!(raw.contains("Subject: [Pixora] verify"), "{raw}");
    }

    #[test]
    fn html_part_escapes_user_content() {
        let html = plain_text_to_html("<script>alert(1)</script>\n第二行");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("第二行"));
    }

    #[test]
    fn invalid_address_is_reported() {
        let mailer = Mailer::new(MailConfig::aliyun("noreply@example.com", "secret"));
        assert!(matches!(
            mailer.build_message("not-an-email", "s", "b"),
            Err(MailError::Address(_))
        ));
    }
}
