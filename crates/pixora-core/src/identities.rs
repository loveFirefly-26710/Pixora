//! 第三方登录与绑定：GitHub / Google / QQ / 微信 / 手机号验证码。
//!
//! 三件事分清楚：
//!
//!   1. **登录**：第三方说「这是我」，我们按 `(provider, subject)` 找账号。找得到就
//!      发会话；找不到就先建号——但那一步要先拿到邮箱（邮箱是账号本身，必需），
//!      所以没拿到邮箱时返回一张签名令牌，让前端补一个邮箱再回来。
//!   2. **绑定**：已登录的人把新方式挂到自己账号上。一个第三方身份只能挂一个账号
//!      （唯一索引兜底），挂到别人身上时明确报冲突。
//!   3. **解绑**：邮箱不可解绑（它不在身份表里）；其余方式解绑前要确认「还能进得来」——
//!      没有可用口令、也没有别的身份时拒绝，免得把人锁在门外。
//!
//! 每个提供方都要管理员在后台填客户端凭据（`login.{provider}.client_id` /
//! `client_secret`，密钥加密存储）并打开开关，没配好就只显示为「未启用」。
//!
//! 手机号走短信验证码，短信服务同样是管理员配置（阿里云短信）；没配好时发码接口
//! 直接说清楚缺哪一项，不假装发出去了。

use base64::Engine;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use pixora_repo::model::{role, status, User};
use pixora_repo::repo::identities::provider;
use pixora_repo::repo::users::NewUser;
use pixora_repo::repo::verifications::purpose;

use crate::auth::{DeviceInfo, IssuedSession};
use crate::error::{AppError, Result};
use crate::guest;
use crate::settings::keys;
use crate::AppState;

/// OAuth 的 `state` 与「补邮箱」令牌的有效期。十几分钟够走完一次授权，
/// 再长就只是把一张可以换账号的纸留在浏览器历史里。
const STATE_TTL_MINUTES: i64 = 15;
/// 单次 OAuth HTTP 请求（换令牌、读账号）的超时。
///
/// 服务器所在网络到 Google / GitHub 的链路可能不稳：设一个明确的上限，
/// 超时就带错误跳回登录页，而不是让浏览器对着 Cloudflare 等出 524。
const OAUTH_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 把 reqwest 的连接类错误翻成管理员能行动的一句话。
///
/// 原始错误里是 `error sending request for url (...)`，用户既看不懂，也不知道
/// 该改 DNS 还是加代理。这里只保留「超时 / 连不上 / 请求没发出」三类。
fn oauth_network_reason(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "连接超时；请确认服务器能出网，或系统代理已开启且可用"
    } else if err.is_connect() {
        "无法建立连接；可能是 DNS 解析失败、端口不可达，或系统代理未生效"
    } else if err.is_request() {
        "请求没有发出去；请检查代理地址与 TLS 配置"
    } else if err.is_decode() {
        "对方返回的数据读不出来"
    } else {
        "网络请求失败"
    }
}

fn oauth_network_error(provider: &str, err: reqwest::Error) -> AppError {
    tracing::warn!(provider, error = %err, "OAuth 出网请求失败");
    AppError::Config(format!(
        "暂时连不上 {provider}：{}。请检查服务器出网，或确认系统代理（Windows「Internet 选项」/ HTTPS_PROXY 环境变量）已生效；调整后重启服务再试",
        oauth_network_reason(&err)
    ))
}
/// 短信验证码有效期与重发间隔。
const PHONE_CODE_TTL_MINUTES: i64 = 10;
const PHONE_CODE_COOLDOWN_SECONDS: i64 = 60;

/// `state` 里背的信息：这次授权是登录还是绑定、完事之后回哪儿、防重放的随机数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthState {
    pub provider: String,
    /// `login` 或 `bind`
    pub mode: String,
    pub next: String,
    pub nonce: String,
    /// 发起绑定的人（`mode = bind` 时必须对上当前会话）
    pub user_id: Option<i64>,
    /// 是不是在页面里的 iframe 里走的（扫码登录）：回调时要把**顶层窗口**
    /// 带走，而不是让 iframe 自己跳到站内页面上
    #[serde(default)]
    pub embed: bool,
    pub expires_at: String,
}

/// 补邮箱令牌里背的信息：哪个第三方的哪个身份，还没落库。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignupTicket {
    pub provider: String,
    pub subject: String,
    pub display: Option<String>,
    pub email: Option<String>,
    pub expires_at: String,
}

/// 换回来的令牌，外加有些平台跟它一起给的 openid（微信直接给，QQ 要再问一次）。
pub struct TokenBundle {
    pub token: String,
    /// 微信与 QQ 的身份标识：两家都把它跟在授权结果里，取用户信息时要带上
    pub openid: Option<String>,
    pub union_id: Option<String>,
    /// Google 的 ID Token。令牌端点是拿着 client_secret 直接回给我们的，
    /// 里面的声明可以信任；用它读账号信息可以少一次对 userinfo 的请求，
    /// 在服务器到 Google 的链路不稳时少一处会超时的地方。
    pub id_token: Option<String>,
}

/// 提供方返回的账号信息。
#[derive(Debug, Clone, Default)]
pub struct IdentityProfile {
    pub subject: String,
    pub display: Option<String>,
    /// 对方给的邮箱。QQ 与微信不给，GitHub 只在邮箱公开/授权时才给
    pub email: Option<String>,
    /// 对方是否已经验证过这个邮箱。只有它为真才敢直接拿来建号
    pub email_verified: bool,
    pub avatar: Option<String>,
    /// 微信的 unionid：同一个开放平台账号下多应用识别同一个人用
    pub union_id: Option<String>,
}

/// 一次授权的结果：拿到会话，还是还要补个邮箱。
pub enum OAuthOutcome {
    LoggedIn(IssuedSession),
    /// 缺邮箱：把这张签名令牌交给前端，补完邮箱再回来建号
    NeedEmail(String),
}

/// 界面上要展示的一条登录方式。
#[derive(Debug, Clone, Serialize)]
pub struct IdentityView {
    pub provider: String,
    pub label: String,
    pub display: Option<String>,
    pub created_at: String,
}

/// `/site` 下发的「本站支持哪些登录方式」。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LoginMethods {
    pub github: bool,
    pub google: bool,
    pub qq: bool,
    pub wechat: bool,
    pub phone: bool,
}

/// 一个提供方的运行期配置（凭据补齐之后）。
pub struct OAuthClient {
    pub provider: String,
    pub client_id: String,
    pub client_secret: String,
}

/// 单个 OAuth 端点的出网探测结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthEndpointHealth {
    pub ok: bool,
    /// 能收到 HTTP 响应就算通；这里只记录状态码，不把响应体带回给管理员
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// OAuth 出网自检结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthHealth {
    /// 是否检测到出网代理（环境变量或 Windows 系统代理）
    pub proxy_configured: bool,
    /// 代理来源，例如「Windows 系统代理」
    pub proxy_source: Option<String>,
    /// 代理主机端口（已去掉凭据）
    pub proxy_url: Option<String>,
    pub google: OAuthEndpointHealth,
    pub github: OAuthEndpointHealth,
}

impl AppState {
    // ============================================================
    // 配置
    // ============================================================

    /// 某个提供方在后台的键名。
    fn provider_key(provider: &str, field: &str) -> String {
        format!("login.{provider}.{field}")
    }

    /// 本站支持哪些登录方式（`/site` 用：只给开关状态，不给凭据）。
    pub async fn login_methods(&self) -> Result<LoginMethods> {
        let mut methods = LoginMethods::default();
        for name in provider::ALL {
            if name == provider::PHONE {
                methods.phone = self.phone_login_enabled().await?;
                continue;
            }
            let enabled = self
                .settings
                .bool(&Self::provider_key(name, "enabled"), false)
                .await?
                && self.get_oauth_client(name).await?.is_some();
            match name {
                provider::GITHUB => methods.github = enabled,
                provider::GOOGLE => methods.google = enabled,
                provider::QQ => methods.qq = enabled,
                provider::WECHAT => methods.wechat = enabled,
                _ => {}
            }
        }
        Ok(methods)
    }

    /// 有没有开手机号登录。开着的意思也只是「可以绑、可以用验证码登录」——
    /// 手机号不是必需的，第一入口仍然是邮箱。
    pub async fn phone_login_enabled(&self) -> Result<bool> {
        if !self.settings.bool(keys::LOGIN_PHONE_ENABLED, false).await? {
            return Ok(false);
        }
        // 开关开着但短信没配全，等于摆了个点不动的按钮：这里一并判掉
        Ok(self.sms_client().await?.is_some())
    }

    /// 取某个提供方的客户端凭据。没配好、没启用都返回 `None`。
    pub async fn get_oauth_client(&self, name: &str) -> Result<Option<OAuthClient>> {
        if !provider::is_known(name) || name == provider::PHONE {
            return Ok(None);
        }
        let client_id = self
            .settings
            .string(&Self::provider_key(name, "client_id"), "")
            .await?;
        let client_secret = self
            .settings
            .get_secret(&Self::provider_key(name, "client_secret"))
            .await?
            .unwrap_or_default();
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(OAuthClient {
            provider: name.to_string(),
            client_id: client_id.trim().to_string(),
            client_secret: client_secret.trim().to_string(),
        }))
    }

    /// 授权地址。让浏览器跳过去，用户在对方站点上同意之后再回到本站。
    pub fn authorize_url(&self, client: &OAuthClient, redirect_uri: &str, state: &str) -> String {
        let encoded = |value: &str| urlencode(value);
        match client.provider.as_str() {
            provider::GITHUB => format!(
                "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope={}&state={}",
                encoded(&client.client_id),
                encoded(redirect_uri),
                encoded("read:user user:email"),
                encoded(state),
            ),
            provider::GOOGLE => format!(
                "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&access_type=online&prompt=select_account",
                encoded(&client.client_id),
                encoded(redirect_uri),
                encoded("openid email profile"),
                encoded(state),
            ),
            provider::QQ => format!(
                "https://graph.qq.com/oauth2.0/authorize?response_type=code&client_id={}&redirect_uri={}&state={}&scope={}",
                encoded(&client.client_id),
                encoded(redirect_uri),
                encoded(state),
                encoded("get_user_info"),
            ),
            provider::WECHAT => format!(
                "https://open.weixin.qq.com/connect/qrconnect?appid={}&redirect_uri={}&response_type=code&scope={}&state={}#wechat_redirect",
                encoded(&client.client_id),
                encoded(redirect_uri),
                encoded("snsapi_login"),
                encoded(state),
            ),
            _ => String::new(),
        }
    }

    // ============================================================
    // state 的签发与校验
    // ============================================================

    /// 签一张 `state`。里面只有「哪个提供方、什么模式、回哪儿、什么时候过期」，
    /// 载荷是明文的，靠签名保证没被改过。
    pub fn sign_state(&self, state: &OAuthState) -> Result<String> {
        let raw = serde_json::to_string(state)
            .map_err(|err| AppError::internal(format!("state 序列化失败：{err}")))?;
        self.settings.sign_payload(&raw)
    }

    pub fn verify_state(&self, token: &str) -> Result<OAuthState> {
        let raw = self
            .settings
            .verify_payload(token)
            .map_err(|_| AppError::bad_request("登录状态校验失败，请回到登录页重试"))?;
        let state: OAuthState = serde_json::from_str(&raw)
            .map_err(|_| AppError::bad_request("登录状态已损坏，请回到登录页重试"))?;
        let expires = chrono::DateTime::parse_from_rfc3339(&state.expires_at)
            .map_err(|_| AppError::bad_request("登录状态没有有效期，请回到登录页重试"))?
            .with_timezone(&Utc);
        if expires < Utc::now() {
            return Err(AppError::bad_request("登录状态已过期，请重新发起登录"));
        }
        Ok(state)
    }

    /// 新的 `state`：带随机数、带过期时间，模式只有两种。
    pub fn new_state(
        &self,
        provider: &str,
        mode: &str,
        next: &str,
        user_id: Option<i64>,
        embed: bool,
    ) -> Result<OAuthState> {
        Ok(OAuthState {
            provider: provider.to_string(),
            mode: mode.to_string(),
            next: sanitize_next(next),
            nonce: pixora_crypto::token::new_session_token()?,
            user_id,
            embed,
            expires_at: (Utc::now() + Duration::minutes(STATE_TTL_MINUTES)).to_rfc3339(),
        })
    }

    // ============================================================
    // 授权码 → 用户信息
    // ============================================================

    /// OAuth 出网自检：只探测固定端点，不接受管理员传 URL，避免变成 SSRF 探针。
    ///
    /// 探测走的是与真实登录同一个 `AppState::http`，所以这里显示的就是
    /// 实际链路：系统代理生效时，探针也经同一代理出去。
    pub async fn oauth_health(&self) -> OAuthHealth {
        let (google, github) = tokio::join!(
            self.probe_oauth_endpoint("https://oauth2.googleapis.com/token"),
            self.probe_oauth_endpoint("https://github.com/login/oauth/access_token"),
        );
        OAuthHealth {
            proxy_configured: self.proxy.configured(),
            proxy_source: self.proxy.source.clone(),
            proxy_url: self.proxy.display.clone(),
            google,
            github,
        }
    }

    async fn probe_oauth_endpoint(&self, url: &str) -> OAuthEndpointHealth {
        match self
            .http
            .get(url)
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
        {
            Ok(response) => OAuthEndpointHealth {
                ok: true,
                status: Some(response.status().as_u16()),
                error: None,
            },
            Err(err) => {
                tracing::warn!(url, error = %err, "OAuth 出网探针失败");
                OAuthEndpointHealth {
                    ok: false,
                    status: None,
                    error: Some(oauth_network_reason(&err).to_string()),
                }
            }
        }
    }

    /// 用授权码换 access_token（QQ/微信顺带换出 openid）。
    pub async fn oauth_token_bundle(
        &self,
        client: &OAuthClient,
        code: &str,
        redirect_uri: &str,
    ) -> Result<TokenBundle> {
        match client.provider.as_str() {
            provider::GITHUB => {
                let body = self
                    .http
                    .post("https://github.com/login/oauth/access_token")
                    .header("accept", "application/json")
                    .form(&[
                        ("client_id", client.client_id.as_str()),
                        ("client_secret", client.client_secret.as_str()),
                        ("code", code),
                        ("redirect_uri", redirect_uri),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("GitHub", err))?;
                let json: serde_json::Value = body
                    .json()
                    .await
                    .map_err(|err| AppError::internal(format!("GitHub 返回的不是 JSON：{err}")))?;
                Ok(TokenBundle {
                    token: require_token(&json, "GitHub")?,
                    openid: None,
                    union_id: None,
                    id_token: None,
                })
            }
            provider::GOOGLE => {
                let response = self
                    .http
                    .post("https://oauth2.googleapis.com/token")
                    .form(&[
                        ("client_id", client.client_id.as_str()),
                        ("client_secret", client.client_secret.as_str()),
                        ("code", code),
                        ("grant_type", "authorization_code"),
                        ("redirect_uri", redirect_uri),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("Google", err))?;
                let json: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|err| AppError::internal(format!("Google 返回的不是 JSON：{err}")))?;
                Ok(TokenBundle {
                    token: require_token(&json, "Google")?,
                    openid: None,
                    union_id: None,
                    id_token: json
                        .get("id_token")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                })
            }
            provider::QQ => {
                let response = self
                    .http
                    .get("https://graph.qq.com/oauth2.0/token")
                    .query(&[
                        ("grant_type", "authorization_code"),
                        ("client_id", client.client_id.as_str()),
                        ("client_secret", client.client_secret.as_str()),
                        ("code", code),
                        ("redirect_uri", redirect_uri),
                        ("fmt", "json"),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("QQ", err))?;
                let text = response
                    .text()
                    .await
                    .map_err(|err| AppError::internal(format!("QQ 返回读不出来：{err}")))?;
                // QQ 的 fmt=json 有时仍然回 query string 形式，两种都认
                let token = parse_query_like(&text)
                    .get("access_token")
                    .cloned()
                    .ok_or_else(|| {
                        AppError::bad_request(format!("QQ 没有返回 access_token：{text}"))
                    })?;
                // openid 要单独问一次，顺手在这里拿：取用户信息时要用它
                let openid = self.qq_openid(&token).await?;
                Ok(TokenBundle {
                    token,
                    openid: Some(openid),
                    union_id: None,
                    id_token: None,
                })
            }
            provider::WECHAT => {
                let response = self
                    .http
                    .get("https://api.weixin.qq.com/sns/oauth2/access_token")
                    .query(&[
                        ("appid", client.client_id.as_str()),
                        ("secret", client.client_secret.as_str()),
                        ("code", code),
                        ("grant_type", "authorization_code"),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("微信", err))?;
                let json: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|err| AppError::internal(format!("微信返回的不是 JSON：{err}")))?;
                // 微信把 openid/unionid 跟在 token 响应里，一起收下
                Ok(TokenBundle {
                    token: require_token(&json, "微信")?,
                    openid: json
                        .get("openid")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    union_id: json
                        .get("unionid")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    id_token: None,
                })
            }
            other => Err(AppError::bad_request(format!("不支持的登录方式：{other}"))),
        }
    }

    /// 取用户信息。每个平台的字段名都不一样，收在这一处。
    pub async fn oauth_profile(
        &self,
        client: &OAuthClient,
        bundle: &TokenBundle,
    ) -> Result<IdentityProfile> {
        match client.provider.as_str() {
            provider::GITHUB => {
                let user: serde_json::Value = self
                    .http
                    .get("https://api.github.com/user")
                    .bearer_auth(&bundle.token)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("GitHub", err))?
                    .json()
                    .await
                    .map_err(|err| {
                        AppError::internal(format!("GitHub 账号信息不是 JSON：{err}"))
                    })?;
                let subject = user
                    .get("id")
                    .map(|value| value.to_string())
                    .ok_or_else(|| AppError::bad_request("GitHub 账号信息里没有 id"))?;
                let mut profile = IdentityProfile {
                    subject,
                    display: user
                        .get("login")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    avatar: user
                        .get("avatar_url")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    ..Default::default()
                };
                // 邮箱要单独问一次：`/user` 里的 email 只在用户公开邮箱时才有，
                // 而我们要的是「已验证的主邮箱」
                if let Ok(emails) = self
                    .http
                    .get("https://api.github.com/user/emails")
                    .bearer_auth(&bundle.token)
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await
                {
                    if let Ok(list) = emails.json::<serde_json::Value>().await {
                        if let Some(items) = list.as_array() {
                            let pick = items
                                .iter()
                                .find(|item| {
                                    item.get("primary").and_then(|v| v.as_bool()) == Some(true)
                                        && item.get("verified").and_then(|v| v.as_bool())
                                            == Some(true)
                                })
                                .or_else(|| {
                                    items.iter().find(|item| {
                                        item.get("verified").and_then(|v| v.as_bool()) == Some(true)
                                    })
                                });
                            if let Some(item) = pick {
                                profile.email = item
                                    .get("email")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_string);
                                profile.email_verified = profile.email.is_some();
                            }
                        }
                    }
                }
                Ok(profile)
            }
            provider::GOOGLE => {
                // 优先用令牌端点一起返回的 id_token：一次请求就够，
                // 少一次对 openidconnect.googleapis.com 的往返。
                if let Some(profile) = bundle
                    .id_token
                    .as_deref()
                    .and_then(|token| google_profile_from_id_token(&client.client_id, token))
                {
                    return Ok(profile);
                }
                let info: serde_json::Value = self
                    .http
                    .get("https://openidconnect.googleapis.com/v1/userinfo")
                    .bearer_auth(&bundle.token)
                    .timeout(OAUTH_HTTP_TIMEOUT)
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("Google", err))?
                    .json()
                    .await
                    .map_err(|err| {
                        AppError::internal(format!("Google 账号信息不是 JSON：{err}"))
                    })?;
                let subject = info
                    .get("sub")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| AppError::bad_request("Google 账号信息里没有 sub"))?;
                let email = info
                    .get("email")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let verified = info
                    .get("email_verified")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                Ok(IdentityProfile {
                    subject: subject.to_string(),
                    display: info
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    email_verified: verified && email.is_some(),
                    email,
                    avatar: info
                        .get("picture")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    union_id: None,
                })
            }
            provider::QQ => {
                let openid = bundle
                    .openid
                    .clone()
                    .ok_or_else(|| AppError::internal("QQ 授权没有拿到 openid"))?;
                let info: serde_json::Value = self
                    .http
                    .get("https://graph.qq.com/user/get_user_info")
                    .query(&[
                        ("access_token", bundle.token.as_str()),
                        ("oauth_consumer_key", client.client_id.as_str()),
                        ("openid", openid.as_str()),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("QQ", err))?
                    .json()
                    .await
                    .map_err(|err| AppError::internal(format!("QQ 账号信息不是 JSON：{err}")))?;
                if info
                    .get("ret")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
                    != 0
                {
                    return Err(AppError::bad_request(format!(
                        "QQ 拒绝了这次读取：{}",
                        error_hint(&info)
                    )));
                }
                Ok(IdentityProfile {
                    subject: openid,
                    display: info
                        .get("nickname")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    avatar: info
                        .get("figureurl_qq_2")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    ..Default::default()
                })
            }
            provider::WECHAT => {
                let openid = bundle
                    .openid
                    .clone()
                    .ok_or_else(|| AppError::internal("微信授权没有拿到 openid"))?;
                let info: serde_json::Value = self
                    .http
                    .get("https://api.weixin.qq.com/sns/userinfo")
                    .query(&[
                        ("access_token", bundle.token.as_str()),
                        ("openid", openid.as_str()),
                    ])
                    .send()
                    .await
                    .map_err(|err| oauth_network_error("微信", err))?
                    .json()
                    .await
                    .map_err(|err| AppError::internal(format!("微信账号信息不是 JSON：{err}")))?;
                Ok(IdentityProfile {
                    subject: openid,
                    display: info
                        .get("nickname")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    avatar: info
                        .get("headimgurl")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                    union_id: bundle.union_id.clone(),
                    ..Default::default()
                })
            }
            other => Err(AppError::bad_request(format!("不支持的登录方式：{other}"))),
        }
    }

    /// QQ 的 openid：回的是 `callback( {...} );` 这种 JSONP 形状。
    async fn qq_openid(&self, token: &str) -> Result<String> {
        let text = self
            .http
            .get("https://graph.qq.com/oauth2.0/me")
            .query(&[("access_token", token), ("fmt", "json")])
            .send()
            .await
            .map_err(|err| oauth_network_error("QQ", err))?
            .text()
            .await
            .map_err(|err| AppError::internal(format!("QQ openid 读不出来：{err}")))?;
        let json = extract_jsonp(&text);
        json.get("openid")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .ok_or_else(|| AppError::bad_request(format!("QQ 没有返回 openid：{text}")))
    }

    // ============================================================
    // 登录 / 绑定 / 解绑
    // ============================================================

    /// 第三方身份登录。`profile` 是刚取回来的账号信息。
    ///
    /// 三种走向：身份已绑定 → 直接登录；没绑定但有已验证邮箱 → 建号并绑定；
    /// 没绑定也没有邮箱 → 返回一张签名令牌，前端补完邮箱再回来。
    pub async fn oauth_login(
        &self,
        provider_name: &str,
        profile: IdentityProfile,
        device: &DeviceInfo,
        remember: bool,
    ) -> Result<OAuthOutcome> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;

        if let Some(identity) = self
            .repo
            .db()
            .find_identity(provider_name, &profile.subject)
            .await?
        {
            let Some(user) = self.repo.find_user_by_id(identity.user_id).await? else {
                return Err(AppError::internal("这个登录方式挂在一个已不存在的账号上"));
            };
            // 与密码登录走同一套状态判断：暂停、封禁、访客账号一律进不来
            guest::ensure_not_guest(&user)?;
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
            self.audit_detail(
                crate::audit::actions::LOGIN_SUCCESS,
                Some(user.id),
                &user.uid,
                serde_json::json!({ "via": provider_name }),
                device.ip.as_deref(),
            )
            .await;
            return Ok(OAuthOutcome::LoggedIn(issued));
        }

        // 没绑过：有已验证邮箱就直接建号，没有就让前端补一个
        let Some(email) = profile.email.as_deref().filter(|_| profile.email_verified) else {
            let ticket = SignupTicket {
                provider: provider_name.to_string(),
                subject: profile.subject.clone(),
                display: profile.display.clone(),
                email: None,
                expires_at: (Utc::now() + Duration::minutes(STATE_TTL_MINUTES)).to_rfc3339(),
            };
            let raw = serde_json::to_string(&ticket)
                .map_err(|err| AppError::internal(format!("补全令牌序列化失败：{err}")))?;
            return Ok(OAuthOutcome::NeedEmail(self.settings.sign_payload(&raw)?));
        };

        let user = self
            .create_identity_user(email, &provider_name.to_string(), &profile, device)
            .await?;
        let issued = self.issue_session(&user, device, remember).await?;
        Ok(OAuthOutcome::LoggedIn(issued))
    }

    /// 用第三方给的已验证邮箱建号，并把身份挂上去。
    async fn create_identity_user(
        &self,
        email: &str,
        provider_name: &str,
        profile: &IdentityProfile,
        device: &DeviceInfo,
    ) -> Result<User> {
        let email = crate::auth::normalize_email_input(email)?;
        // 注册白名单与反临时邮箱对这条路同样成立：站点只收哪些域名，
        // 不该因为从 GitHub 进来就放开
        self.ensure_register_domain_allowed(&email).await?;
        crate::antispam::check_email_allowed(self, &email).await?;
        if self.repo.email_exists(&email).await? {
            // 邮箱已经有了：那是同一个人的另一个入口，让他登录后去「登录方式」里绑
            return Err(AppError::Conflict(format!(
                "{email} 已经注册过了，请先用它登录，再到「安全 → 登录方式」里绑定 {label}",
                label = provider::label(provider_name)
            )));
        }

        let (quota_default, _) = self.settings.quota_values().await?;
        let user = self
            .create_user_with_uid(NewUser {
                uid: String::new(),
                email: email.clone(),
                // 第三方验证过的邮箱视为已验证；口令留成一个验不过去的值，
                // 用户想用密码登录时得先自己设一个（安全页有入口）
                password_hash: UNUSABLE_PASSWORD.to_string(),
                role: role::USER.to_string(),
                status: status::ACTIVE.to_string(),
                email_verified: true,
                quota_bytes: quota_default,
            })
            .await?;
        self.repo.insert_default_album(user.id, "默认相册").await?;
        self.bind_identity(&user, provider_name, profile, device.ip.as_deref())
            .await?;
        self.audit_detail(
            crate::audit::actions::REGISTER,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "email": email, "via": provider_name }),
            device.ip.as_deref(),
        )
        .await;
        Ok(user)
    }

    /// 绑定一个第三方身份。
    pub async fn bind_identity(
        &self,
        user: &User,
        provider_name: &str,
        profile: &IdentityProfile,
        ip: Option<&str>,
    ) -> Result<()> {
        if !provider::is_known(provider_name) {
            return Err(AppError::bad_request(format!(
                "不支持的登录方式：{provider_name}"
            )));
        }
        if let Some(existing) = self
            .repo
            .db()
            .find_identity(provider_name, &profile.subject)
            .await?
        {
            if existing.user_id == user.id {
                // 已经绑在同一个账号上：幂等通过，顺手把昵称刷新一下
                let meta = profile_meta(profile);
                self.repo
                    .db()
                    .update_identity_display(
                        existing.id,
                        profile.display.as_deref(),
                        meta.as_deref(),
                    )
                    .await?;
                return Ok(());
            }
            return Err(AppError::Conflict(format!(
                "这个 {label} 已经绑在别的账号上了，一个 {label} 只能绑一个账号",
                label = provider::label(provider_name)
            )));
        }
        // 同一个账号每类只留一条：换绑（解绑旧号再绑新号）走这条
        if let Some(existing) = self
            .repo
            .db()
            .find_user_identity(user.id, provider_name)
            .await?
        {
            self.repo
                .db()
                .delete_identity(user.id, provider_name)
                .await?;
            tracing::info!(
                user_id = user.id,
                provider = provider_name,
                old = %existing.subject,
                "同一账号重复绑定，已替换旧的一条"
            );
        }

        let meta = profile_meta(profile);
        self.repo
            .db()
            .insert_identity(
                user.id,
                provider_name,
                &profile.subject,
                profile.display.as_deref(),
                meta.as_deref(),
            )
            .await
            .map_err(|err| {
                if err.is_unique_violation() {
                    AppError::Conflict("这个第三方账号已经绑过了".into())
                } else {
                    err.into()
                }
            })?;
        self.audit_detail(
            crate::audit::actions::SETTINGS_CHANGE,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "identity": provider_name, "action": "bind" }),
            ip,
        )
        .await;
        Ok(())
    }

    /// 解绑。邮箱不在身份表里（它不可解绑），这里只管第三方方式与手机号。
    pub async fn unbind_identity(
        &self,
        user: &User,
        provider_name: &str,
        ip: Option<&str>,
    ) -> Result<()> {
        if provider_name == crate::auth::PASSWORD_IDENTITY {
            return Err(AppError::bad_request(
                "邮箱是账号本身，不能解绑；要换邮箱请到「账号信息」里改",
            ));
        }
        if !provider::is_known(provider_name) {
            return Err(AppError::bad_request(format!(
                "不支持的登录方式：{provider_name}"
            )));
        }
        let removed = self
            .repo
            .db()
            .delete_identity(user.id, provider_name)
            .await?;
        if removed == 0 {
            return Err(AppError::not_found(format!(
                "未绑定的 {label}",
                label = provider::label(provider_name)
            )));
        }
        // 解绑之后还得能进得来：没有可用口令、也没有别的第三方方式时，
        // 这个人就再也登不上了（邮箱验证码只在开了二次验证时才有意义）
        if !has_usable_password(user) && self.repo.db().count_identities(user.id).await? == 0 {
            // 把刚删的补回去：宁可让他先设密码，也不能留下一个进不去的账号
            let _ = self
                .repo
                .db()
                .insert_identity(user.id, provider_name, "-", None, None)
                .await;
            return Err(AppError::bad_request(
                "这是你唯一的登录方式，解绑后就进不来了；请先设置一个密码，或再绑一种方式",
            ));
        }
        self.audit_detail(
            crate::audit::actions::SETTINGS_CHANGE,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "identity": provider_name, "action": "unbind" }),
            ip,
        )
        .await;
        Ok(())
    }

    /// 某个账号挂着的登录方式（界面用）。
    pub async fn identities_of(&self, user: &User) -> Result<Vec<IdentityView>> {
        let list = self.repo.db().list_identities(user.id).await?;
        Ok(list
            .into_iter()
            .map(|item| IdentityView {
                label: provider::label(&item.provider).to_string(),
                display: item.display.clone(),
                created_at: item.created_at.to_rfc3339(),
                provider: item.provider,
            })
            .collect())
    }

    /// 用补全令牌建号（OAuth 没给邮箱时，前端让用户自己填一个）。
    pub async fn complete_oauth_signup(
        &self,
        ticket_token: &str,
        email: &str,
        code: &str,
        device: &DeviceInfo,
    ) -> Result<IssuedSession> {
        let raw = self
            .settings
            .verify_payload(ticket_token)
            .map_err(|_| AppError::bad_request("补全令牌无效，请重新发起登录"))?;
        let ticket: SignupTicket = serde_json::from_str(&raw)
            .map_err(|_| AppError::bad_request("补全令牌已损坏，请重新发起登录"))?;
        let expires = chrono::DateTime::parse_from_rfc3339(&ticket.expires_at)
            .map_err(|_| AppError::bad_request("补全令牌没有有效期"))?
            .with_timezone(&Utc);
        if expires < Utc::now() {
            return Err(AppError::bad_request("这次登录已经过期，请重新发起"));
        }

        // 邮箱走和注册一样的验证码：先发（`send_register_code`）再消费
        let email = crate::auth::normalize_email_input(email)?;
        self.consume_code(&email, purpose::REGISTER, code).await?;

        let profile = IdentityProfile {
            subject: ticket.subject.clone(),
            display: ticket.display.clone(),
            ..Default::default()
        };
        let user = self
            .create_identity_user(&email, &ticket.provider, &profile, device)
            .await?;
        // 建完号直接发会话：用户刚验证完邮箱，没有理由再让他填一遍密码
        self.issue_session(&user, device, false).await
    }

    // ============================================================
    // 手机号
    // ============================================================

    /// 发一条短信验证码。手机号进 `email_verifications` 的键位（那张表的键只是个
    /// 字符串，手机号一样能用），用途分开，免得分不清是哪条路上的码。
    pub async fn send_phone_code(
        &self,
        phone: &str,
        device: &DeviceInfo,
        purpose_name: &str,
    ) -> Result<()> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let phone = normalize_phone(phone)?;
        if !self.phone_login_enabled().await? {
            return Err(AppError::Config(
                "本站还没开手机号登录：管理员要在「登录方式」里配好短信服务".into(),
            ));
        }
        // 一分钟一条：短信是要花钱的，而且连着点会把人手机刷爆
        if let Some(latest) = self
            .repo
            .find_latest_verification(&phone, purpose_name)
            .await?
        {
            let waited = (Utc::now() - latest.created_at).num_seconds();
            if waited < PHONE_CODE_COOLDOWN_SECONDS {
                return Err(AppError::RateLimited {
                    retry_after: (PHONE_CODE_COOLDOWN_SECONDS - waited) as u64,
                    detail: "验证码刚发过，请稍等一会儿再要".into(),
                });
            }
        }

        let code = pixora_crypto::token::new_numeric_code(6)?;
        self.repo
            .create_email_verification(
                &phone,
                &pixora_crypto::token::token_hash(&code),
                purpose_name,
                Utc::now() + Duration::minutes(PHONE_CODE_TTL_MINUTES),
                device.ip.as_deref(),
            )
            .await?;
        self.send_sms(&phone, &code).await
    }

    /// 手机号 + 验证码登录。
    pub async fn phone_login(
        &self,
        phone: &str,
        code: &str,
        device: &DeviceInfo,
        remember: bool,
    ) -> Result<IssuedSession> {
        self.ensure_ip_allowed(device.ip.as_deref(), crate::ipguard::scope::LOGIN)
            .await?;
        let phone = normalize_phone(phone)?;
        // 先看短信配没配：没配的话「验证码无效」是句误导——用户根本收不到码
        if !self.phone_login_enabled().await? {
            return Err(AppError::Config(
                "本站没开手机号登录（或短信服务没配好），请用邮箱登录".into(),
            ));
        }
        self.consume_code(&phone, purpose::PHONE_LOGIN, code)
            .await?;

        let Some(identity) = self
            .repo
            .db()
            .find_identity(provider::PHONE, &phone)
            .await?
        else {
            return Err(AppError::bad_request(
                "这个手机号还没绑到任何账号上：先用邮箱注册登录，再到「安全 → 登录方式」里绑定",
            ));
        };
        let Some(user) = self.repo.find_user_by_id(identity.user_id).await? else {
            return Err(AppError::internal("这个手机号挂在一个已不存在的账号上"));
        };
        guest::ensure_not_guest(&user)?;
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
        self.audit_detail(
            crate::audit::actions::LOGIN_SUCCESS,
            Some(user.id),
            &user.uid,
            serde_json::json!({ "via": "phone" }),
            device.ip.as_deref(),
        )
        .await;
        Ok(issued)
    }

    /// 把手机号绑到当前账号上（先验码）。
    pub async fn bind_phone(
        &self,
        user: &User,
        phone: &str,
        code: &str,
        ip: Option<&str>,
    ) -> Result<()> {
        let phone = normalize_phone(phone)?;
        // 同上：配置问题排在验证码前面，报错才指向真正的原因
        if !self.phone_login_enabled().await? {
            return Err(AppError::Config(
                "本站没开手机号登录（或短信服务没配好），绑不了手机号".into(),
            ));
        }
        self.consume_code(&phone, purpose::PHONE_BIND, code).await?;
        let profile = IdentityProfile {
            subject: phone.clone(),
            display: Some(mask_phone(&phone)),
            ..Default::default()
        };
        self.bind_identity(user, provider::PHONE, &profile, ip)
            .await
    }

    // ============================================================
    // 短信发送
    // ============================================================

    /// 短信服务配置。缺一项就返回 `None`——宁可让管理员看到「未配置」，
    /// 也不要发出半条短信或者假装发出去了。
    async fn sms_client(&self) -> Result<Option<SmsClient>> {
        if !self.settings.bool(keys::SMS_ENABLED, false).await? {
            return Ok(None);
        }
        let access_key_id = self.settings.string(keys::SMS_ACCESS_KEY_ID, "").await?;
        let access_key_secret = self
            .settings
            .get_secret(keys::SMS_ACCESS_KEY_SECRET)
            .await?
            .unwrap_or_default();
        let sign_name = self.settings.string(keys::SMS_SIGN_NAME, "").await?;
        let template_code = self.settings.string(keys::SMS_TEMPLATE_CODE, "").await?;
        if access_key_id.trim().is_empty()
            || access_key_secret.trim().is_empty()
            || sign_name.trim().is_empty()
            || template_code.trim().is_empty()
        {
            return Ok(None);
        }
        Ok(Some(SmsClient {
            access_key_id: access_key_id.trim().to_string(),
            access_key_secret: access_key_secret.trim().to_string(),
            sign_name: sign_name.trim().to_string(),
            template_code: template_code.trim().to_string(),
        }))
    }

    /// 发一条验证码短信（阿里云短信）。
    pub async fn send_sms(&self, phone: &str, code: &str) -> Result<()> {
        let Some(client) = self.sms_client().await? else {
            return Err(AppError::Config(
                "短信服务没配好：需要在「登录方式」里填 AccessKey、签名与模板编号".into(),
            ));
        };
        client.send(self, phone, code).await
    }
}

/// 第三方身份里的 `meta`：头像、unionid 这类原样留着，界面要显示时再解析。
fn profile_meta(profile: &IdentityProfile) -> Option<String> {
    let mut map = serde_json::Map::new();
    if let Some(avatar) = profile.avatar.as_deref() {
        map.insert("avatar".into(), serde_json::json!(avatar));
    }
    if let Some(union_id) = profile.union_id.as_deref() {
        map.insert("union_id".into(), serde_json::json!(union_id));
    }
    if map.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(map).to_string())
}

/// 没有可用口令的账号用的口令哈希：与访客账号同一套做法，验不过去。
const UNUSABLE_PASSWORD: &str = "!";

/// 这个账号有可用的口令吗（第三方建号的账号没有）。
fn has_usable_password(user: &User) -> bool {
    let hash = user.password_hash.trim();
    !hash.is_empty() && hash != UNUSABLE_PASSWORD
}

/// 手机号归一化：去掉空格、连字符与括号，只留 `+` 与数字，最后校验长度。
pub fn normalize_phone(raw: &str) -> Result<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '+')
        .collect();
    let digits = cleaned.trim_start_matches('+');
    if digits.len() < 6 || digits.len() > 20 {
        return Err(AppError::bad_request("手机号看起来不对：只认 6–20 位数字"));
    }
    Ok(format!("+{digits}"))
}

/// 界面展示用的掩码手机号：`+86138****8000`。
fn mask_phone(phone: &str) -> String {
    let digits: Vec<char> = phone.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.len() < 7 {
        return phone.to_string();
    }
    let head: String = digits.iter().take(digits.len() - 8).collect();
    let tail: String = digits.iter().skip(digits.len() - 4).collect();
    format!("+{head}****{tail}")
}

/// 跳转目标只允许站内相对路径：`state` 里带的是可控输入，`//evil.com` 这种
/// 会被浏览器当成协议相对地址，变成把用户送去别处的开放重定向。
///
/// 反斜杠也要拒：浏览器在水合 URL 时常把 `\` 当 `/`，`/\evil.com` 会变成
/// `//evil.com`；控制字符同理，不能让它混进 Location。
fn sanitize_next(next: &str) -> String {
    let trimmed = next.trim();
    if trimmed.starts_with('/')
        && !trimmed.starts_with("//")
        && !trimmed.contains('\\')
        && !trimmed.chars().any(char::is_control)
    {
        return trimmed.to_string();
    }
    "/".to_string()
}

/// `application/x-www-form-urlencoded` 形状的响应（QQ 的 token 接口就是这样）。
fn parse_query_like(text: &str) -> std::collections::HashMap<String, String> {
    let trimmed = text.trim();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(map) = json.as_object() {
            return map
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect();
        }
    }
    // `a=1&b=2` 这种：自家解析，够用且不用为它引一个 URL 库
    trimmed
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 把 QQ 的 `callback( {...} );` 拆成 JSON。
fn extract_jsonp(text: &str) -> serde_json::Value {
    let start = text.find('{');
    let end = text.rfind('}');
    match (start, end) {
        (Some(start), Some(end)) if end > start => {
            serde_json::from_str(&text[start..=end]).unwrap_or(serde_json::Value::Null)
        }
        _ => serde_json::Value::Null,
    }
}

/// 从 token 响应里取 `access_token`，取不到就把对方的原话带上。
fn require_token(json: &serde_json::Value, label: &str) -> Result<String> {
    json.get("access_token")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            AppError::bad_request(format!(
                "{label} 没有返回 access_token：{}",
                error_hint(json)
            ))
        })
}

/// 对方的错误响应里常见 `error` / `error_description`，带上能让管理员看懂。
fn error_hint(json: &serde_json::Value) -> String {
    for key in ["error_description", "error", "errmsg", "message"] {
        if let Some(value) = json.get(key).and_then(|value| value.as_str()) {
            return value.to_string();
        }
    }
    json.to_string()
}

/// 最小百分号编码：只留 unreserved 字符，其余按 UTF-8 转义。
fn urlencode(value: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// 阿里云短信客户端（dysmsapi，RPC 风格 + HMAC-SHA1 签名）。
struct SmsClient {
    access_key_id: String,
    access_key_secret: String,
    sign_name: String,
    template_code: String,
}

impl SmsClient {
    async fn send(&self, state: &AppState, phone: &str, code: &str) -> Result<()> {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;

        let params = [
            ("AccessKeyId", self.access_key_id.clone()),
            ("Action", "SendSms".to_string()),
            ("Format", "JSON".to_string()),
            ("PhoneNumbers", phone.trim_start_matches('+').to_string()),
            ("RegionId", "cn-hangzhou".to_string()),
            ("SignName", self.sign_name.clone()),
            ("SignatureMethod", "HMAC-SHA1".to_string()),
            ("SignatureNonce", pixora_crypto::token::new_session_token()?),
            ("SignatureVersion", "1.0".to_string()),
            ("TemplateCode", self.template_code.clone()),
            (
                "TemplateParam",
                serde_json::json!({ "code": code }).to_string(),
            ),
            (
                "Timestamp",
                Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ),
            ("Version", "2017-05-25".to_string()),
        ];

        // 签名：参数按名字排序 → 拼成 query string → HMAC-SHA1(secret + "&") →
        // base64。这是阿里云 RPC 接口的老规矩，改一个字节都会被判签名不过
        let mut sorted: Vec<(String, String)> = params
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical = sorted
            .iter()
            .map(|(key, value)| format!("{}={}", urlencode(key), urlencode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let to_sign = format!("GET&{}&{}", urlencode("/"), urlencode(&canonical));
        let mut mac =
            Hmac::<Sha1>::new_from_slice(format!("{}&", self.access_key_secret).as_bytes())
                .map_err(|err| AppError::internal(format!("短信签名初始化失败：{err}")))?;
        mac.update(to_sign.as_bytes());
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        let query = format!("{canonical}&Signature={}", urlencode(&signature));
        let response = state
            .http
            .get(format!("https://dysmsapi.aliyuncs.com/?{query}"))
            .send()
            .await
            .map_err(|err| AppError::Config(format!("调用短信服务失败：{err}")))?;
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|err| AppError::Config(format!("短信服务返回的不是 JSON：{err}")))?;
        let ok = body
            .get("Code")
            .and_then(|value| value.as_str())
            .map(|code| code.eq_ignore_ascii_case("OK"))
            .unwrap_or(false);
        if !ok {
            let code = body
                .get("Code")
                .and_then(|value| value.as_str())
                .unwrap_or("未知错误");
            let message = body
                .get("Message")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            return Err(AppError::Config(format!(
                "短信服务拒绝发送（{code}）：{message}"
            )));
        }
        Ok(())
    }
}

/// 从 Google 的 ID Token 里读账号信息。
///
/// 令牌是 Google 的令牌端点直接在 TLS 连接里、用我们的 client_secret 换回来的，
/// 所以不再验签；只核对 `aud` 是本应用，避免拿别的客户端的令牌混进来。
/// 解不出来或字段不全时返回 `None`，调用方回落到 userinfo 接口。
fn google_profile_from_id_token(client_id: &str, id_token: &str) -> Option<IdentityProfile> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let info: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if let Some(aud) = info.get("aud").and_then(|value| value.as_str()) {
        if !aud.is_empty() && aud != client_id {
            return None;
        }
    }
    let subject = info.get("sub")?.as_str()?.to_string();
    let email = info
        .get("email")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let verified = match info.get("email_verified") {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(value)) => value.eq_ignore_ascii_case("true"),
        _ => false,
    };
    Some(IdentityProfile {
        subject,
        display: info
            .get("name")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        email_verified: verified && email.is_some(),
        email,
        avatar: info
            .get("picture")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        union_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_id_token_avoids_the_userinfo_request() {
        let payload = serde_json::json!({
            "aud": "client-1",
            "sub": "google-subject",
            "name": "萤火",
            "email": "firefly@example.com",
            "email_verified": true,
            "picture": "https://example.com/avatar.png"
        });
        let token = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        );
        let profile = google_profile_from_id_token("client-1", &token).expect("能读出账号信息");
        assert_eq!(profile.subject, "google-subject");
        assert_eq!(profile.display.as_deref(), Some("萤火"));
        assert_eq!(profile.email.as_deref(), Some("firefly@example.com"));
        assert!(profile.email_verified);
        assert_eq!(
            profile.avatar.as_deref(),
            Some("https://example.com/avatar.png")
        );
        // 别的客户端的令牌不能拿来当我们的账号
        assert!(google_profile_from_id_token("client-2", &token).is_none());
    }

    #[test]
    fn next_keeps_site_relative_paths() {
        assert_eq!(sanitize_next("/albums?tab=1"), "/albums?tab=1");
        assert_eq!(sanitize_next("  /upload "), "/upload");
        // 站内路径里的 `#` 也照原样带回
        assert_eq!(sanitize_next("/#/login"), "/#/login");
    }

    #[test]
    fn next_rejects_open_redirects() {
        // 协议相对地址会被浏览器当成别家的域名
        assert_eq!(sanitize_next("//evil.example.com"), "/");
        // 绝对地址一律退回首页
        assert_eq!(sanitize_next("https://evil.example.com/x"), "/");
        // 前缀像但域名不同
        assert_eq!(sanitize_next("https://pixora.example.com.evil.net/"), "/");
        // 把域名塞进 userinfo 的写法
        assert_eq!(sanitize_next("https://pixora.example.com@evil.net/"), "/");
        assert_eq!(sanitize_next("javascript:alert(1)"), "/");
        // 反斜杠会被浏览器当成分隔符，`/\evil.com` 等价于 `//evil.com`
        assert_eq!(sanitize_next("/\\evil.example.com"), "/");
        assert_eq!(
            sanitize_next("/ok\nLocation: https://evil.example.com"),
            "/"
        );
        assert_eq!(sanitize_next(""), "/");
    }
}
