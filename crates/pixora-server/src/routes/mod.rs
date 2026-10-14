//! 路由层：把 HTTP 请求翻译成领域调用。
//!
//! 这一层只做四件事，任何「业务判断」都不该出现在这里：
//!
//!   1. **取参数**：路径、查询串、JSON 体、multipart 字段。取不到就给 `BAD_REQUEST`，
//!      不写一堆 `#[derive(Deserialize)]` 的请求结构体——字段可选性用
//!      `serde_json::Value` 判「键在不在」比给每个结构体加 `Option` 更直观。
//!   2. **鉴权**：`require_user` / `require_admin` 把「可选鉴权 + CSRF + 角色」
//!      三步合一次，避免每个 handler 抄三遍（抄漏一处就是一个越权口子）。
//!   3. **调领域**：`pixora_core` 上的方法签名就是业务契约，handler 只做类型搬运。
//!   4. **包信封**：`crate::http::finish` / `finish_page` / `with_cookies`。
//!
//! 两条贯穿全层的规矩：
//!   - **错误一律是 `AppError`**。状态码与错误码的对应关系只在 `http::error_response`
//!     一处实现，这里绝不手拼 `StatusCode`（拼出来的和契约迟早会对不上）。
//!   - **来源 IP 只从 `request_context` 取**。它走 `IpGuard::client_ip` 的代理层数
//!     解析，直接读 `X-Forwarded-For` 等于把 IP 黑名单的钥匙交给客户端。

pub mod account;
pub mod admin;
pub mod auth;
pub mod catalog;
pub mod install;
pub mod public;
pub mod upload;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use pixora_core::audit::actions;
use pixora_core::images::Viewer;
use pixora_core::{AppError, AppState};
use pixora_repo::model::User;

use crate::http::{authenticate_optional, error_response, Authenticated};

/// 连接对端地址。
///
/// 契约里写的是 `Option<ConnectInfo<SocketAddr>>`，但 axum 0.8 的 `ConnectInfo`
/// 没有实现 `OptionalFromRequestParts`（孤儿规则也不允许我们在外面补），
/// 所以这里包一层：拿不到连接信息就是 `None`，路由用没用
/// `into_make_service_with_connect_info` 都不会让请求直接 500。
/// 真实 IP 仍然由 `request_context` 结合代理层数解析，这里只是「最后的兜底」。
pub struct PeerAddr(pub Option<SocketAddr>);

/// 作为处理器参数时需要这个实现：`OptionalFromRequestParts` 只服务
/// `Option<T>` 形式的抽取，单独实现它并不会让类型本身可被抽取。
impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

impl<S: Send + Sync> OptionalFromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(Some(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        )))
    }
}

impl PeerAddr {
    pub fn addr(&self) -> Option<SocketAddr> {
        self.0
    }
}

// ============================================================
// 鉴权
// ============================================================

/// 必须登录：可选鉴权（未登录 → 401）+ CSRF 校验。
///
/// 为什么用「可选鉴权」再判空，而不是直接 `authenticate`：`authenticate` 在没有
/// 任何凭据时也报 `Unauthorized`，两者结果一样但错误码不同——契约里
/// 「未登录」是 `UNAUTHORIZED`，而「凭据无效」是 `INVALID_CREDENTIALS` /
/// `TOKEN_REVOKED`。走 `authenticate_optional` 能把这两类如实区分开。
pub async fn require_user(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ip: Option<&str>,
    method: &Method,
) -> Result<Authenticated, Response> {
    let auth = match authenticate_optional(state, headers, ip).await {
        Ok(Some(auth)) => auth,
        Ok(None) => return Err(error_response(AppError::Unauthorized)),
        Err(err) => return Err(error_response(err)),
    };
    if let Err(err) = crate::http::verify_csrf(Some(&auth), headers, method) {
        return Err(error_response(err));
    }
    Ok(auth)
}

/// 管理员专用：在 `require_user` 之上再判角色。
///
/// 越权访问要留审计——设计稿把「管理员跨用户访问」写成合规要求，
/// 「有人试图访问后台」同样属于该留档的事件（谁、什么时候、想干什么）。
pub async fn require_admin(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ip: Option<&str>,
    method: &Method,
    path: &str,
) -> Result<Authenticated, Response> {
    let auth = require_user(state, headers, ip, method).await?;
    if !auth.is_admin() {
        let target = auth.user.uid.clone();
        state
            .audit(
                pixora_repo::repo::audit::AuditEntry::new(ADMIN_DENIED)
                    .actor(Some(auth.user.id))
                    .target(target)
                    .detail(serde_json::json!({ "path": path }))
                    .ip(ip.map(str::to_string)),
            )
            .await;
        return Err(error_response(AppError::Forbidden(
            "需要管理员权限".into(),
        )));
    }
    Ok(auth)
}

/// 越权访问后台的审计动作名。
///
/// `actions` 里没有这一项（它只列了业务动作），但它属于「必须留档」的事件，
/// 所以在路由层就近定义；字符串一旦写进库就不能再改，取个直白的名字。
pub const ADMIN_DENIED: &str = "admin_denied";

/// Token 权限范围检查。
///
/// 会话登录天然拥有全部权限（`require_scope` 里判），所以这里只对 API Token 生效。
pub fn ensure_scope(auth: &Authenticated, scope: &str) -> Result<(), Response> {
    auth.require_scope(scope).map_err(error_response)
}

/// 按访问者裁剪视角。
///
/// `viewing_other` 为真且访问者是管理员时才给 `AdminViewing`——它决定了
/// 「能不能看别人的空间」以及「跨用户访问要不要写审计」，判断只在这里做一次。
pub fn viewer_for(auth: Option<&Authenticated>, viewing_other: bool) -> Viewer {
    match auth {
        None => Viewer::Anonymous,
        Some(auth) if auth.user.is_admin() && viewing_other => {
            Viewer::AdminViewing(auth.user.clone())
        }
        Some(auth) => Viewer::User(auth.user.clone()),
    }
}

// ============================================================
// 请求体取值
// ============================================================

/// 必填字符串。空串与纯空白都算没给——前端传 `""` 多半是「输入框是空的」。
pub fn field_str(body: &serde_json::Value, key: &str) -> Result<String, Response> {
    body.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| bad_request(format!("缺少参数 {key}")))
}

/// 必填整数。
pub fn field_i64(body: &serde_json::Value, key: &str) -> Result<i64, Response> {
    field_opt_i64(body, key).ok_or_else(|| bad_request(format!("缺少参数 {key}")))
}

/// 选填字符串（不给、给 null、给空串都返回 `None`）。
pub fn field_opt_str(body: &serde_json::Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 选填整数。数字与数字字符串都认（multipart 里传过来的都是字符串）。
pub fn field_opt_i64(body: &serde_json::Value, key: &str) -> Option<i64> {
    match body.get(key)? {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 选填布尔。
pub fn field_opt_bool(body: &serde_json::Value, key: &str) -> Option<bool> {
    match body.get(key)? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::Number(number) => number.as_i64().map(|value| value != 0),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// 键在不在。
///
/// 迁移兼容靠它：`PATCH /images/{id}` 里 `album_id` 传 `null` 表示「移出相册」，
/// 键不存在表示「不改」——`field_opt_i64` 两种都返回 `None`，必须先用它区分。
pub fn field_present(body: &serde_json::Value, key: &str) -> bool {
    body.get(key).is_some()
}

/// 选填的字符串数组（元素去空白、丢空串）。
pub fn field_string_list(body: &serde_json::Value, key: &str) -> Option<Vec<String>> {
    let array = body.get(key)?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// 逗号分隔的字符串列表（multipart 的 `tags` 字段）。
pub fn split_csv(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// 构造一个 `BAD_REQUEST` 响应。
pub fn bad_request(message: impl Into<String>) -> Response {
    error_response(AppError::bad_request(message))
}

/// 构造一个 `NOT_FOUND` 响应。
pub fn not_found(what: impl Into<String>) -> Response {
    error_response(AppError::not_found(what))
}

/// 解析 `serde_json::Value` 成具体类型，失败给 `BAD_REQUEST`。
pub fn parse_body<T: serde::de::DeserializeOwned>(body: &serde_json::Value, what: &str) -> Result<T, Response> {
    serde_json::from_value(body.clone())
        .map_err(|err| bad_request(format!("{what}不合法：{err}")))
}

/// 从查询串取分页参数，交给领域层的 `PageRequest`。
pub fn page_request(
    query: &std::collections::HashMap<String, String>,
    default_per_page: i64,
    max_per_page: i64,
) -> pixora_repo::PageRequest {
    pixora_repo::PageRequest::new(
        crate::http::parse_i64(query, "page"),
        crate::http::parse_i64(query, "per_page"),
        default_per_page,
        max_per_page,
    )
}

// ============================================================
// 用户视图（契约 3.1 的 `user` 对象）
// ============================================================

/// 每个用户两个偏好开关的存储键。放在 `settings` 表里而不是加列：
/// 这两个开关只给界面用，改它们不该牵动用户表的读写路径。
/// 键名带 uid（对外标识）而不是自增 id，导入导出时更稳。
pub fn pref_key_login_alert(uid: &str) -> String {
    format!("user.{uid}.login_alert")
}

pub fn pref_key_new_device_guard(uid: &str) -> String {
    format!("user.{uid}.new_device_guard")
}

/// 读用户的界面偏好，缺省都是 `true`（默认打开提醒与异地登录保护）。
pub async fn user_prefs(state: &AppState, user: &User) -> (bool, bool) {
    let login_alert = state
        .settings
        .bool(&pref_key_login_alert(&user.uid), true)
        .await
        .unwrap_or(true);
    let new_device_guard = state
        .settings
        .bool(&pref_key_new_device_guard(&user.uid), true)
        .await
        .unwrap_or(true);
    (login_alert, new_device_guard)
}

/// 契约 3.1 的 `user` 对象：账号本体 + 统计 + 生效限额。
///
/// 统计与限额都从库里现取而不是让前端再发一次请求——它们是同一个页面的
/// 同一次渲染，分两次取会出现「配额条和数字对不上」这种说不清的问题。
pub async fn user_view(state: &Arc<AppState>, user: &User) -> pixora_core::Result<serde_json::Value> {
    let stats = state.repo.user_stats(user.id).await?;
    let limits = state.repo.effective_policy(user).await?;
    let (login_alert, new_device_guard) = user_prefs(state, user).await;

    Ok(serde_json::json!({
        "id": user.id,
        "uid": user.uid,
        "email": user.email,
        "role": user.role,
        "status": user.status,
        "email_verified": user.email_verified,
        "created_at": crate::http::rfc3339(user.created_at),
        "last_login_at": user.last_login_at.map(crate::http::rfc3339),
        "last_login_ip": user.last_login_ip,
        "totp_enabled": user.totp_enabled,
        "login_alert": login_alert,
        "new_device_guard": new_device_guard,
        "quota_bytes": user.quota_bytes,
        "used_bytes": user.used_bytes,
        "stats": {
            "album_count": stats.album_count,
            "image_count": stats.image_count,
            "public_count": stats.public_count,
            "tag_count": stats.tag_count,
        },
        "limits": {
            "api_per_minute": limits.api_per_minute,
            "upload_per_minute": limits.upload_per_minute,
            "upload_per_day": limits.upload_per_day,
            "upload_bytes_per_day": limits.upload_bytes_per_day,
            "max_file_size": limits.max_file_size,
            "max_batch_size": limits.max_batch_size,
            "unlimited": limits.unlimited,
        },
    }))
}

/// 契约 3.1 的完整响应体：`{ user, csrf_token }`。
///
/// `csrf_token` 在会话模式下就是 `pixora_csrf` Cookie 的值（前端存内存后刷新页面
/// 靠 Cookie 还原），所以这里把它从请求里读回来原样回显；Token 模式下没有
/// CSRF 这条线，给 `null`。
pub async fn me_payload(
    state: &Arc<AppState>,
    user: &User,
    csrf_token: Option<String>,
) -> pixora_core::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "user": user_view(state, user).await?,
        "csrf_token": csrf_token,
    }))
}

/// 审计里记「管理员看了谁」，与 `AppState::audit_cross_user` 同一动作名。
pub async fn audit_admin_view(state: &AppState, admin: &User, target_uid: &str, page: &str, ip: Option<&str>) {
    state
        .audit_detail(
            actions::ADMIN_VIEW_USER,
            Some(admin.id),
            target_uid.to_string(),
            serde_json::json!({ "page": page }),
            ip,
        )
        .await;
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use pixora_repo::db::DbDriver;
    use pixora_repo::model::status as user_status;
    use pixora_repo::repo::tokens::NewApiToken;
    use pixora_repo::NewUser;
    use tower::ServiceExt;

    /// 1x1 的合法 PNG（魔数与 IHDR 都真，足以过服务端的嗅探与尺寸解析）。
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// 真实 SQLite + 本地目录的实例：迁移、设置、存储装配都走真代码。
    /// 每个测试用独立的临时目录，测试之间不共享状态。
    async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let dsn = dir.path().join("pixora-routes.db");
        let config = pixora_core::config::Config {
            bind: "127.0.0.1:0".parse().expect("监听地址"),
            db_driver: DbDriver::Sqlite,
            db_dsn: dsn.to_string_lossy().to_string(),
            db_max_connections: 2,
            data_dir: dir.path().join("data"),
            master_key: None,
            key_source: pixora_core::config::KeySource::Missing,
            workers: 1,
            base_url: Some("https://img.example.com".into()),
            // 测试里没有代理，直接信对端地址
            trust_proxy: 0,
            web_dir: None,
            accel_prefix: None,
            secure_cookies: false,
        dev_return_codes: false,
            log_level: "error".into(),
        };
        let state = AppState::bootstrap(config).await.expect("装配 AppState");
        (state, dir)
    }

    /// 端到端用真实路由表：路径、方法与 handler 的接线也一并验证。
    fn app(state: Arc<AppState>) -> axum::Router {
        crate::app::build(state)
    }

    async fn add_user(state: &Arc<AppState>, email: &str, uid: &str, admin: bool) -> User {
        let user = state
            .repo
            .create_user(&NewUser {
                uid: uid.to_string(),
                email: email.to_string(),
                password_hash: "$argon2id$fake".to_string(),
                role: if admin { "admin" } else { "user" }.to_string(),
                status: user_status::ACTIVE.to_string(),
                quota_bytes: 10 * 1024 * 1024 * 1024,
                email_verified: true,
            })
            .await
            .expect("建用户");
        state
            .repo
            .insert_default_album(user.id, "默认相册")
            .await
            .expect("默认相册");
        user
    }

    /// 造一个可用的会话 Cookie。
    ///
    /// 服务端只存令牌的哈希，所以这里自己生成明文令牌再按同一算法存哈希，
    /// 走的是与登录完全相同的鉴权路径。
    async fn login_cookie(state: &Arc<AppState>, user: &User) -> String {
        let token = format!("pxs_test_{}", user.uid);
        state
            .repo
            .create_session(
                user.id,
                &pixora_crypto::token::token_hash(&token),
                Some("测试设备"),
                Some("203.0.113.18"),
                Some("Mozilla/5.0 Chrome/120"),
            )
            .await
            .expect("建会话");
        format!("pixora_session={token}; pixora_csrf=csrf-{}", user.uid)
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("响应体是 JSON")
    }

    fn get(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        builder.body(Body::empty()).expect("构造请求")
    }

    fn post_json(uri: &str, cookie: Option<&str>, csrf: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if let Some(csrf) = csrf {
            builder = builder.header("x-csrf-token", csrf);
        }
        builder
            .body(Body::from(body.to_string()))
            .expect("构造请求")
    }

    /// 契约 1.2：错误必须带 code，`data` 为 null。
    fn assert_error(body: &serde_json::Value, code: &str) {
        assert_eq!(body["error"]["code"], code, "错误码要与契约一致：{body}");
        assert!(body["data"].is_null(), "失败响应里 data 必须是 null：{body}");
    }

    #[tokio::test]
    async fn unauthenticated_me_is_rejected_with_unauthorized() {
        let (state, _dir) = test_state().await;

        let response = app(state)
            .oneshot(get("/api/v1/me", None))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = body_json(response).await;
        assert_error(&body, "UNAUTHORIZED");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("登录"));
    }

    #[tokio::test]
    async fn write_without_csrf_header_is_rejected() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "chenyu@example.com", "1001", false).await;
        let cookie = login_cookie(&state, &user).await;

        // 带会话但没有 CSRF 头：双提交校验失败
        let response = app(state.clone())
            .oneshot(post_json(
                "/api/v1/albums",
                Some(&cookie),
                None,
                serde_json::json!({ "name": "旅行" }),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_error(&body, "CSRF_FAILED");

        // 补上 CSRF 头就能过（证明上一步拦的是 CSRF，而不是别的）
        let response = app(state)
            .oneshot(post_json(
                "/api/v1/albums",
                Some(&cookie),
                Some("csrf-1001"),
                serde_json::json!({ "name": "旅行" }),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["data"]["name"], "旅行");
        assert_eq!(body["data"]["slug"], "旅行", "slug 省略时按名称生成");
    }

    #[tokio::test]
    async fn admin_routes_reject_normal_users() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "chenyu@example.com", "1001", false).await;
        let cookie = login_cookie(&state, &user).await;

        let response = app(state)
            .oneshot(get("/api/v1/admin/users", Some(&cookie)))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_error(&body, "FORBIDDEN");
    }

    #[tokio::test]
    async fn site_is_reachable_without_login() {
        let (state, _dir) = test_state().await;

        let response = app(state)
            .oneshot(get("/api/v1/site", None))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        let data = &body["data"];
        assert_eq!(data["name"], "Pixora");
        assert_eq!(data["base_url"], "https://img.example.com");
        assert_eq!(data["registration_open"], true);
        assert_eq!(data["installed"], false);
        assert_eq!(data["terms"]["required"], false, "未登录时不要求同意条款");
        assert!(data["formats"]
            .as_array()
            .expect("格式清单")
            .iter()
            .any(|item| item == "png"));
    }

    #[tokio::test]
    async fn list_endpoints_carry_pagination_meta() {
        let (state, _dir) = test_state().await;

        let response = app(state)
            .oneshot(get("/api/v1/images?page=2&per_page=10", None))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        assert_eq!(body["meta"]["page"], 2);
        assert_eq!(body["meta"]["per_page"], 10);
        assert_eq!(body["meta"]["total"], 0);
        assert_eq!(body["meta"]["total_pages"], 0);
        assert!(body["data"].is_array());
    }

    #[tokio::test]
    async fn missing_image_reports_not_found_code() {
        let (state, _dir) = test_state().await;

        let response = app(state)
            .oneshot(get("/api/v1/images/99999", None))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_error(&body, "NOT_FOUND");
    }

    #[tokio::test]
    async fn plaza_respects_the_public_switch() {
        let (state, _dir) = test_state().await;

        let open = app(state.clone())
            .oneshot(get("/api/v1/plaza", None))
            .await
            .expect("发请求");
        assert_eq!(open.status(), StatusCode::OK);

        // 关掉广场公开浏览：未登录要拿到 FORBIDDEN，而不是「空列表」
        state
            .settings
            .set_bool(pixora_core::settings::keys::PLAZA_PUBLIC, false)
            .await
            .expect("关闭广场");
        let closed = app(state)
            .oneshot(get("/api/v1/plaza", None))
            .await
            .expect("发请求");
        assert_eq!(closed.status(), StatusCode::FORBIDDEN);
        let body = body_json(closed).await;
        assert_error(&body, "FORBIDDEN");
    }

    #[tokio::test]
    async fn legacy_upload_accepts_api_key_and_old_field_name() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "picgo@example.com", "2001", false).await;

        // 老客户端只有 API Token，没有会话 Cookie
        let plain = "px_live_route_test_token";
        state
            .repo
            .create_api_token(&NewApiToken {
                user_id: user.id,
                name: "PicGo 桌面端".to_string(),
                token_hash: pixora_crypto::token::token_hash(plain),
                display: plain[..16].to_string(),
                scopes: vec!["upload".to_string()],
                expires_at: None,
            })
            .await
            .expect("签发令牌");

        let boundary = "----pixoraTestBoundary";
        let mut payload = Vec::new();
        // **文件在前、元数据在后**：curl 的 -F 就是按命令行顺序发的
        // （smoke 脚本与 PicGo 都是这个形状），必须能正确处理
        payload.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"1x1.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        payload.extend_from_slice(PNG_1X1);
        payload.extend_from_slice(b"\r\n");
        payload.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"tags\"\r\n\r\n风景,旅行\r\n"
            )
            .as_bytes(),
        );
        payload.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let request = |uri: &str, accept_json: bool| {
            let mut builder = Request::builder()
                .method("POST")
                .uri(uri)
                .header("x-api-key", plain)
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                );
            if accept_json {
                builder = builder.header("accept", "application/json");
            }
            builder.body(Body::from(payload.clone())).expect("构造请求")
        };

        // 旧结构：{success, message, url, storage_type, tags}
        let response = app(state.clone())
            .oneshot(request("/upload", false))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["storage_type"], "local");
        assert_eq!(body["tags"], "风景,旅行");
        let url = body["url"].as_str().expect("直链");
        assert!(url.starts_with("https://img.example.com/images/"), "{url}");
        assert!(url.ends_with(".png"), "{url}");
        assert!(
            body.get("data").is_none(),
            "旧结构不带信封：{body}"
        );

        // 同一个端点带 ?format=picgo 时回标准信封
        let response = app(state)
            .oneshot(request("/upload?format=picgo", false))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["data"]["dedup"], true, "同一份文件第二次应为去重：{body}");
        assert!(body["error"].is_null());
        assert!(body["data"]["url"].as_str().unwrap_or_default().contains("/images/"));
    }

    #[tokio::test]
    async fn upload_needs_a_token() {
        let (state, _dir) = test_state().await;

        let response = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/upload")
                    .header("content-type", "multipart/form-data; boundary=x")
                    .body(Body::from("--x--\r\n"))
                    .expect("构造请求"),
            )
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_error(&body, "UNAUTHORIZED");
    }
}
