//! HTTP 公共设施：响应信封、错误映射、请求上下文与鉴权。
//!
//! 三件事集中在这一层，任何 handler 都不必重复做：
//!
//!   1. **信封**：成功 `{data, error: null}`，失败 `{data: null, error}`。
//!      错误里的 `code` 是给程序看的契约（见 `docs/api-contract.md` 1.2）。
//!   2. **鉴权**：会话 Cookie 与 API Token 两条路都收敛到 `Authenticated`，
//!      账号状态判断只在 `pixora_core::auth::ensure_loginable` 里做一次。
//!   3. **CSRF**：双提交 Cookie。带 Token 的请求不校验——它不是浏览器发起的，
//!      没有 Cookie 也就没有可被利用的会话。

use std::net::SocketAddr;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use pixora_core::auth::DeviceInfo;
use pixora_core::{AppError, AppState};
use pixora_repo::model::User;
use serde::Serialize;

/// 会话 Cookie 名。
pub const SESSION_COOKIE: &str = "pixora_session";
/// CSRF Cookie 名（非 HttpOnly，前端要读它放进请求头）。
pub const CSRF_COOKIE: &str = "pixora_csrf";
/// CSRF 请求头名。
pub const CSRF_HEADER: &str = "x-csrf-token";

/// 响应信封。
#[derive(Debug, Serialize)]
pub struct Envelope<T> {
    pub data: Option<T>,
    pub error: Option<ErrorBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Meta {
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
    pub total_pages: i64,
}

/// 把领域结果包成 HTTP 响应。
pub struct AppResponse(Result<Response, AppError>);

impl AppResponse {
    pub fn ok<T: Serialize>(data: T) -> Response {
        envelope_response(StatusCode::OK, Some(data), None, None, None)
    }


    pub fn paged<T: Serialize>(page: pixora_repo::Page<T>) -> Response {
        let meta = Meta {
            page: page.page,
            per_page: page.per_page,
            total: page.total,
            total_pages: page.total_pages(),
        };
        envelope_response(StatusCode::OK, Some(page.items), None, Some(meta), None)
    }

}

impl IntoResponse for AppResponse {
    fn into_response(self) -> Response {
        match self.0 {
            Ok(response) => response,
            Err(err) => error_response(err),
        }
    }
}

/// 把领域错误翻成响应。错误码与 HTTP 状态码的对应关系只在这里。
pub fn error_response(err: AppError) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut extra = serde_json::Map::new();
    if let Some(value) = err.extra() {
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                extra.insert(key.clone(), value.clone());
            }
        }
    }

    let body = ErrorBody {
        code: err.code().to_string(),
        message: err.message(),
        detail: err.extra().and_then(|value| {
            value
                .get("detail")
                .and_then(|item| item.as_str())
                .map(str::to_string)
        }),
        extra,
    };

    // 服务端自己的问题要留日志；客户端错误（4xx）不值得刷屏
    if err.is_server_fault() {
        tracing::error!(code = err.code(), error = %err, "请求处理失败");
    } else {
        tracing::debug!(code = err.code(), message = %err.message(), "请求被拒绝");
    }

    let mut response = envelope_response(status, None::<()>, Some(body), None, None);
    if let Some(seconds) = err.retry_after() {
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

fn envelope_response<T: Serialize>(
    status: StatusCode,
    data: Option<T>,
    error: Option<ErrorBody>,
    meta: Option<Meta>,
    extra_headers: Option<HeaderMap>,
) -> Response {
    let envelope = Envelope { data, error, meta };
    let mut response = (status, Json(envelope)).into_response();
    if let Some(headers) = extra_headers {
        response.headers_mut().extend(headers);
    }
    response
}

/// 便捷：把领域结果转成响应。
pub fn finish<T: Serialize>(result: pixora_core::Result<T>) -> Response {
    match result {
        Ok(data) => AppResponse::ok(data),
        Err(err) => error_response(err),
    }
}

/// 便捷：分页结果转响应。
pub fn finish_page<T: Serialize>(result: pixora_core::Result<pixora_repo::Page<T>>) -> Response {
    match result {
        Ok(page) => AppResponse::paged(page),
        Err(err) => error_response(err),
    }
}

// ============================================================
// 请求上下文
// ============================================================

/// 一次请求的来源信息与身份。
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl RequestContext {
    pub fn device(&self) -> DeviceInfo {
        DeviceInfo {
            ip: self.ip.clone(),
            user_agent: self.user_agent.clone(),
        }
    }
}

/// 从请求头与连接信息里解析来源。
///
/// 真实 IP 的解析规则在 `IpGuard::client_ip` 里：只信最靠近服务端的那一层代理，
/// 否则客户端可以伪造 `X-Forwarded-For` 绕过 IP 黑名单。
pub async fn request_context(
    state: &AppState,
    headers: &HeaderMap,
    addr: Option<SocketAddr>,
) -> RequestContext {
    let policy = state
        .ip_guard
        .policy()
        .await
        .unwrap_or_else(|_| pixora_core::ipguard::IpPolicy::default());

    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    let real_ip = headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok());
    let peer = addr.map(|value| value.ip().to_string());

    RequestContext {
        ip: pixora_core::ipguard::IpGuard::client_ip(
            &policy,
            forwarded,
            real_ip,
            peer.as_deref(),
        ),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    }
}

/// 已通过鉴权的身份。
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub user: User,
    /// 会话模式下是会话 id，Token 模式下是 None
    pub session_id: Option<i64>,
    /// 是否用 API Token 鉴权（决定要不要跳过 CSRF）
    pub via_token: bool,
    /// Token 的权限范围（会话模式下为空，表示全权）
    pub scopes: Vec<String>,
}

impl Authenticated {
    pub fn is_admin(&self) -> bool {
        self.user.is_admin()
    }

    /// 检查 Token 的权限范围。会话登录视为拥有全部权限。
    pub fn require_scope(&self, scope: &str) -> pixora_core::Result<()> {
        if self.session_id.is_some() || self.scopes.is_empty() {
            return Ok(());
        }
        if self
            .scopes
            .iter()
            .any(|item| item == scope || item == "admin")
        {
            return Ok(());
        }
        Err(AppError::forbidden(format!(
            "当前 Token 缺少 {scope} 权限"
        )))
    }
}

/// 从请求里取会话 Cookie。
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, SESSION_COOKIE)
}

/// 从请求里取 Bearer / X-API-Key。
pub fn api_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(text) = value.to_str() {
            if let Some(token) = text.strip_prefix("Bearer ").or_else(|| text.strip_prefix("bearer ")) {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 从 Cookie 头里取一个值。只处理 `name=value` 这一层，不做 URL 解码之外的花样。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 鉴权。两条路都走 `pixora_core::auth`，账号状态只在那里判一次。
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    ip: Option<&str>,
) -> pixora_core::Result<Authenticated> {
    // 优先看 Bearer：API 客户端不发送会话 Cookie，这个顺序对两边都自然
    if let Some(token) = api_token(headers) {
        let (user, record) = state.authenticate_token(&token).await?;
        return Ok(Authenticated {
            user,
            session_id: None,
            via_token: true,
            scopes: record.scopes,
        });
    }

    if let Some(token) = session_cookie(headers) {
        let (user, session) = state.authenticate_session(&token, ip).await?;
        return Ok(Authenticated {
            user,
            session_id: Some(session.id),
            via_token: false,
            scopes: Vec::new(),
        });
    }

    Err(AppError::Unauthorized)
}

/// 可选鉴权：未登录返回 None，凭据无效仍然报错（避免「Cookie 过期了却当成游客」）。
pub async fn authenticate_optional(
    state: &AppState,
    headers: &HeaderMap,
    ip: Option<&str>,
) -> pixora_core::Result<Option<Authenticated>> {
    if api_token(headers).is_none() && session_cookie(headers).is_none() {
        return Ok(None);
    }
    match authenticate(state, headers, ip).await {
        Ok(auth) => Ok(Some(auth)),
        // 被暂停/封禁要如实返回错误：前端要展示整页提示与申诉入口
        Err(AppError::Unauthorized) => Ok(None),
        Err(err) => Err(err),
    }
}

/// CSRF 校验：双提交 Cookie。
///
/// 只写操作需要过这一关。带 Bearer Token 的请求跳过——它不是浏览器发的，
/// 也不依赖 Cookie，本来就不存在被跨站利用的会话。
pub fn verify_csrf(
    auth: Option<&Authenticated>,
    headers: &HeaderMap,
    method: &axum::http::Method,
) -> pixora_core::Result<()> {
    let is_write = !matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if !is_write {
        return Ok(());
    }
    if auth.map(|item| item.via_token).unwrap_or(false) {
        return Ok(());
    }

    let cookie = cookie_value(headers, CSRF_COOKIE);
    let header = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    // 失败要留下可查的线索：这一关卡住了哪一半（Cookie 没带 / 请求头没带 /
    // 带了但不一样），从前端截图上看不出来，日志里得有
    let ok = match (cookie.as_deref(), header.as_deref()) {
        (Some(cookie), Some(header)) => pixora_crypto::token::constant_time_eq(cookie, header),
        _ => false,
    };
    if ok {
        return Ok(());
    }
    tracing::warn!(
        method = %method,
        has_cookie = cookie.is_some(),
        has_header = header.is_some(),
        same_length = matches!((cookie.as_deref(), header.as_deref()), (Some(c), Some(h)) if c.len() == h.len()),
        "CSRF 校验未通过：Cookie 与请求头对不上（前端会取新令牌重试一次）"
    );
    Err(AppError::Csrf)
}

/// 造登录成功后的两个 Cookie。
pub async fn session_cookies(
    state: &AppState,
    session_token: &str,
    csrf_token: &str,
    remember: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    // 记住我：30 天；否则会话 Cookie（关浏览器即失效）
    let max_age = if remember {
        "; Max-Age=2592000".to_string()
    } else {
        String::new()
    };
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };

    let session = format!(
        "{SESSION_COOKIE}={session_token}; Path=/; HttpOnly; SameSite=Lax{secure}{max_age}"
    );
    // CSRF Cookie 必须能被 JS 读到（双提交模式的前提），所以不设 HttpOnly
    let csrf = format!("{CSRF_COOKIE}={csrf_token}; Path=/; SameSite=Lax{secure}{max_age}");

    if let Ok(value) = HeaderValue::from_str(&session) {
        headers.append(header::SET_COOKIE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&csrf) {
        headers.append(header::SET_COOKIE, value);
    }
    headers
}

/// 清空两个 Cookie。
pub fn clear_cookies(state: &AppState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    for name in [SESSION_COOKIE, CSRF_COOKIE] {
        if let Ok(value) = HeaderValue::from_str(&format!(
            "{name}=; Path=/; Max-Age=0; SameSite=Lax{secure}"
        )) {
            headers.append(header::SET_COOKIE, value);
        }
    }
    headers
}

/// 给响应追加 Cookie。
pub fn with_cookies(mut response: Response, cookies: HeaderMap) -> Response {
    response.headers_mut().extend(cookies);
    response
}

/// RFC3339 时间串，统一用它给前端。
pub fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}



/// 从查询串里取可选整数。
pub fn parse_i64(params: &std::collections::HashMap<String, String>, key: &str) -> Option<i64> {
    params.get(key).and_then(|value| value.trim().parse::<i64>().ok())
}

/// 从查询串里取可选字符串（空串视为不存在）。
pub fn parse_str<'a>(
    params: &'a std::collections::HashMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    params
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(list: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in list {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn cookie_parsing_handles_multiple_values() {
        let headers = headers_with(&[("cookie", "a=1; pixora_session=pxs_abc; b=2")]);
        assert_eq!(session_cookie(&headers).as_deref(), Some("pxs_abc"));
        assert_eq!(cookie_value(&headers, "a").as_deref(), Some("1"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn api_token_from_bearer_or_header() {
        let bearer = headers_with(&[("authorization", "Bearer px_live_123")]);
        assert_eq!(api_token(&bearer).as_deref(), Some("px_live_123"));

        let key = headers_with(&[("x-api-key", "px_live_456")]);
        assert_eq!(api_token(&key).as_deref(), Some("px_live_456"));

        let empty = headers_with(&[("authorization", "Bearer ")]);
        assert_eq!(api_token(&empty), None);
    }

    #[test]
    fn csrf_requires_matching_double_submit() {
        let method = axum::http::Method::POST;

        let good = headers_with(&[
            ("cookie", "pixora_csrf=abc123"),
            ("x-csrf-token", "abc123"),
        ]);
        assert!(verify_csrf(None, &good, &method).is_ok());

        let mismatch = headers_with(&[
            ("cookie", "pixora_csrf=abc123"),
            ("x-csrf-token", "other"),
        ]);
        assert!(verify_csrf(None, &mismatch, &method).is_err());

        let missing = headers_with(&[("cookie", "pixora_csrf=abc123")]);
        assert!(verify_csrf(None, &missing, &method).is_err());

        // GET 不校验
        assert!(verify_csrf(None, &missing, &axum::http::Method::GET).is_ok());
    }

    #[test]
    fn token_requests_skip_csrf() {
        let auth = Authenticated {
            user: sample_user(),
            session_id: None,
            via_token: true,
            scopes: vec!["upload".into()],
        };
        let empty = HeaderMap::new();
        assert!(verify_csrf(Some(&auth), &empty, &axum::http::Method::POST).is_ok());
    }

    #[test]
    fn token_scopes_are_enforced() {
        let auth = Authenticated {
            user: sample_user(),
            session_id: None,
            via_token: true,
            scopes: vec!["upload".into(), "read".into()],
        };
        assert!(auth.require_scope("upload").is_ok());
        assert!(auth.require_scope("read").is_ok());
        assert!(auth.require_scope("write").is_err());

        // 会话登录不受范围限制
        let session_auth = Authenticated {
            user: sample_user(),
            session_id: Some(1),
            via_token: false,
            scopes: Vec::new(),
        };
        assert!(session_auth.require_scope("admin").is_ok());
    }

    #[test]
    fn error_responses_carry_codes() {
        let response = error_response(AppError::AccountSuspended {
            reason: Some("上传违规内容".into()),
            suspended_at: Some("2026-10-14T15:30:00Z".into()),
        });
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let limited = error_response(AppError::RateLimited {
            retry_after: 42,
            detail: String::new(),
        });
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            limited.headers().get(header::RETRY_AFTER).unwrap(),
            "42"
        );
    }

    fn sample_user() -> User {
        User {
            id: 1,
            uid: "1001".into(),
            email: "a@b.com".into(),
            email_verified: true,
            password_hash: String::new(),
            role: "user".into(),
            status: "active".into(),
            status_reason: None,
            status_changed_at: None,
            status_changed_by: None,
            totp_secret: None,
            totp_enabled: false,
            quota_bytes: 1024,
            used_bytes: 0,
            login_fail_count: 0,
            locked_until: None,
            created_at: Utc::now(),
            last_login_at: None,
            last_login_ip: None,
        }
    }
}
