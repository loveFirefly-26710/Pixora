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
pub mod oauth;
pub mod public;
pub mod recycle;
pub mod upload;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, OptionalFromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use pixora_core::audit::actions;
use pixora_core::images::Viewer;
use pixora_core::{AppError, AppState};
use pixora_repo::model::User;

use crate::http::{authenticate, error_response, Authenticated};

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

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
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

/// 全站 IP 规则。
///
/// 登录、注册、上传各自还会再按场景检查一次；这一层负责把
/// `scope = all` 的规则真正落到**所有**请求上。否则一条「全站封禁」只能
/// 拦住登录和上传，被拦的人照样能浏览广场、拉直链。
///
/// 规则表与设置都有短缓存，命中计数也有每分钟一次的节流；这层不会把
/// 每个静态资源请求变成一次数据库查询。
pub async fn enforce_ip(
    State(state): State<Arc<AppState>>,
    peer: PeerAddr,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let rectx = crate::http::request_context(&state, request.headers(), peer.addr()).await;
    if let Err(err) = state
        .ip_guard
        .ensure_allowed(rectx.ip.as_deref(), pixora_core::ipguard::scope::ALL)
        .await
    {
        return crate::http::error_response(err);
    }
    next.run(request).await
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
    let auth = require_user_allow_pending(state, headers, ip, method).await?;
    // 管理员直接设过密码的账号，在改掉之前只放行 /me 与改密码本身（那两个
    // 入口走 require_user_allow_pending）。拦在这里而不是逐个端点加判断：
    // 默认拒绝，漏掉一处也不会留下口子
    if auth.user.must_change_password {
        return Err(error_response(AppError::forbidden(
            "管理员已重置你的密码，请先设置新密码后再继续使用",
        )));
    }
    Ok(auth)
}

/// 与 `require_user` 相同，但不拦「必须改密码」的账号。
///
/// 只给 /me（前端要知道为什么被拦）与 /me/password（改密码本身）用。
pub async fn require_user_allow_pending(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ip: Option<&str>,
    method: &Method,
) -> Result<Authenticated, Response> {
    // 这里直接走 `authenticate`，不走 `authenticate_optional`：后者会给
    // 读接口记一次 API 额度，若在这里再用一次，同一个 GET 会被扣两次。
    let auth = match authenticate(state, headers, ip).await {
        Ok(auth) => auth,
        Err(err) => return Err(error_response(err)),
    };
    let is_read = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if let Err(err) = state
        .limits
        .enforce_api(&auth.user, is_read, auth.via_token || !is_read)
        .await
    {
        return Err(error_response(err));
    }
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
    // 两种拒绝：那个不接受登录的访客账号（见 core/guest.rs），以及角色不是管理员
    // 的普通用户。访客排在前面判，是为了让「后台只认人的账号」这件事不依赖于
    // 「访客账号恰好没有管理员角色」——给它改了角色也进不来
    let denial = if pixora_core::guest::is_guest(&auth.user) {
        Some("访客账号不能进入管理后台")
    } else if !auth.is_admin() {
        Some("需要管理员权限")
    } else {
        None
    };
    if let Some(reason) = denial {
        let target = auth.user.uid.clone();
        state
            .audit(
                pixora_repo::repo::audit::AuditEntry::new(ADMIN_DENIED)
                    .actor(Some(auth.user.id))
                    .target(target)
                    .detail(serde_json::json!({ "path": path, "reason": reason }))
                    .ip(ip.map(str::to_string)),
            )
            .await;
        return Err(error_response(AppError::Forbidden(reason.to_string())));
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
pub fn parse_body<T: serde::de::DeserializeOwned>(
    body: &serde_json::Value,
    what: &str,
) -> Result<T, Response> {
    serde_json::from_value(body.clone()).map_err(|err| bad_request(format!("{what}不合法：{err}")))
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
pub async fn user_view(
    state: &Arc<AppState>,
    user: &User,
) -> pixora_core::Result<serde_json::Value> {
    let stats = state.repo.user_stats(user.id).await?;
    let limits = state.repo.effective_policy(user).await?;
    // 上传策略（单文件上限、单批张数、像素上限、格式白名单、并发数）按生效值给：
    // 客户端的上传队列据此限流，不能再拿 /site 里的全局值当自己的额度
    let upload = state.effective_upload_limits(user).await?;
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
        // 头像：站内相对路径或外链，界面直接塞进 <img src>
        "avatar": user.avatar,
        // 管理员设过密码、用户还没自己改：前端据此把人送到改密码页
        "must_change_password": user.must_change_password,
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
            // 下面这些是「生效的上传策略」，可能被单用户覆盖过，
            // 所以不再等于 /site 里的全局值
            "max_file_size": upload.max_file_bytes,
            "max_batch_size": upload.max_batch,
            "max_pixels": upload.max_pixels,
            "max_width": upload.max_width,
            "max_height": upload.max_height,
            "formats": upload.formats,
            "max_concurrent_uploads": upload.max_concurrent_uploads,
            "queue_depth": state.settings.upload().await?.queue_depth,
            "unlimited": limits.unlimited,
        },
        // 安装时创建的那个管理员账号。界面据此把对它的操作入口关掉（服务端本来
        // 也会拒）；普通用户拿到的是 null
        "install_admin_uid": if user.is_admin() {
            state.install_admin_uid().await.ok().flatten()
        } else {
            None
        },
    }))
}

/// 契约 3.1 的完整响应体：`{ user, csrf_token }`。
///
/// `csrf_token` 在会话模式下就是 `pixora_csrf` Cookie 的值（前端存内存后刷新页面
/// 靠 Cookie 还原），所以这里把它从请求里读回来原样回显；Token 模式下没有
/// CSRF 这条线，给 `null`。
///
/// `impersonator` 是管理员免登录切换用户时多出来的一段：有值说明当前身份是
/// 「管理员切过来的」，前端据此显示提示条与切回入口。它由服务端的
/// `pixora_impersonator` Cookie 决定，前端伪造不了。
pub async fn me_payload(
    state: &Arc<AppState>,
    user: &User,
    csrf_token: Option<String>,
    impersonator: Option<serde_json::Value>,
) -> pixora_core::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "user": user_view(state, user).await?,
        "csrf_token": csrf_token,
        "impersonator": impersonator,
    }))
}

/// 从请求头里解出「谁切过来的」。
///
/// Cookie 缺失或已失效都返回 `None`——那只是「切不回去了」，
/// 不该让整次请求失败。
pub async fn impersonator_info(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
) -> Option<serde_json::Value> {
    let token = crate::http::impersonator_cookie(headers)?;
    let admin = state.impersonator_user(&token).await.ok()?;
    Some(serde_json::json!({
        "id": admin.id,
        "uid": admin.uid,
        "email": admin.email,
    }))
}

/// 审计里记「管理员看了谁」，与 `AppState::audit_cross_user` 同一动作名。
pub async fn audit_admin_view(
    state: &AppState,
    admin: &User,
    target_uid: &str,
    page: &str,
    ip: Option<&str>,
) {
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
            samesite: pixora_core::config::SameSite::Lax,
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
                None,
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

    fn post_json(
        uri: &str,
        cookie: Option<&str>,
        csrf: Option<&str>,
        body: serde_json::Value,
    ) -> Request<Body> {
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
        assert!(
            body["data"].is_null(),
            "失败响应里 data 必须是 null：{body}"
        );
    }

    fn patch_json(
        uri: &str,
        cookie: Option<&str>,
        csrf: Option<&str>,
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("PATCH")
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
    async fn api_read_limit_is_enforced_for_sessions() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "limit@example.com", "6601", false).await;
        let cookie = login_cookie(&state, &user).await;

        let mut policy = pixora_repo::model::RateLimitPolicy::defaults();
        policy.api_read_per_minute = 1;
        policy.api_per_minute = 1;
        state
            .repo
            .upsert_global_policy(&policy)
            .await
            .expect("写限流策略");

        let first = app(state.clone())
            .oneshot(get("/api/v1/me", Some(&cookie)))
            .await
            .expect("第一次请求");
        assert_eq!(first.status(), StatusCode::OK);

        let second = app(state)
            .oneshot(get("/api/v1/me", Some(&cookie)))
            .await
            .expect("第二次请求");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_json(second).await;
        assert_error(&body, "RATE_LIMITED");
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
        // 自动关闭注册在自动模式下跟宿主机磁盘占用挂钩，磁盘有多满随机器变化；
        // 这个用例验的是 /site 本身，把容量那条线换成手填模式（本项目占用 0）保确定性
        state
            .settings
            .set("capacity.mode", "manual")
            .await
            .expect("容量模式");

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

    /// 带 Bearer 令牌的 multipart 上传请求。
    fn multipart_upload(uri: &str, token: &str, file: &[u8], name: &str) -> Request<Body> {
        const BOUNDARY: &str = "pixora-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\ncontent-disposition: form-data; name=\"file\"; \
                 filename=\"{name}\"\r\ncontent-type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .expect("构造请求")
    }

    /// 通用 multipart 上传：额外字段、缩略图、会话 Cookie 都带上。
    ///
    /// 上传接口的字段（tags / visibility / reuse_existing / thumb）各自有分支，
    /// 想验哪一条就往 `fields` 里塞哪一条，不必为每种组合再写一个 helper。
    fn multipart_form(
        uri: &str,
        auth: MultipartAuth<'_>,
        file: &[u8],
        name: &str,
        fields: &[(&str, &str)],
        thumb: Option<&[u8]>,
    ) -> Request<Body> {
        const BOUNDARY: &str = "pixora-test-boundary";
        let mut body = Vec::new();
        for (key, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\ncontent-disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\ncontent-disposition: form-data; name=\"file\"; \
                 filename=\"{name}\"\r\ncontent-type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n");
        if let Some(thumb) = thumb {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\ncontent-disposition: form-data; name=\"thumb\"; \
                     filename=\"thumb.webp\"\r\ncontent-type: image/webp\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(thumb);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        let mut builder = Request::builder().method("POST").uri(uri).header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
        match auth {
            MultipartAuth::Token(token) => {
                builder = builder.header("authorization", format!("Bearer {token}"));
            }
            MultipartAuth::Session { cookie, csrf } => {
                builder = builder
                    .header("cookie", cookie)
                    .header("x-csrf-token", csrf);
            }
            MultipartAuth::None => {}
        }
        builder.body(Body::from(body)).expect("构造请求")
    }

    /// multipart 请求怎么证明自己：API Token、会话（Cookie + CSRF），或者什么都不带。
    #[derive(Clone, Copy)]
    enum MultipartAuth<'a> {
        Token(&'a str),
        Session { cookie: &'a str, csrf: &'a str },
        None,
    }

    /// 一张最小可用的 WebP（嗅探只看 RIFF/WEBP 魔数）。
    fn tiny_webp() -> Vec<u8> {
        // 1×1 的 VP8X WebP：真实 RIFF/VP8X 头，尺寸解析能读出 1×1。
        // 早先这里只写了 RIFF+WEBP 加 14 个零字节，嗅探能过、尺寸读不出，
        // 新的缩略图像素校验会把它拦掉。
        let mut bytes = Vec::with_capacity(30);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&22u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8X");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0]); // width - 1
        bytes.extend_from_slice(&[0, 0, 0]); // height - 1
        bytes
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

    /// 关着游客上传时，未登录的批量预检也回 401。
    ///
    /// 预检是唯一会把「文件哈希」送进服务端的入口（客户端本地算完再来问哪些已有）。
    /// 它跟着 `/upload` 一起挡在鉴权后面，改前端代码也换不来一次比对——
    /// 既不给「某个文件在不在站里」的探测口子，也不让请求白占服务端。
    #[tokio::test]
    async fn preflight_needs_a_token_when_guest_upload_is_off() {
        let (state, _dir) = test_state().await;
        assert!(
            !state.guest_upload_enabled().await.expect("读站点设置"),
            "默认就是「拒绝游客上传」，这个用例的前提是它关着"
        );

        let response = app(state)
            .oneshot(post_json(
                "/api/v1/blobs/exists",
                None,
                None,
                serde_json::json!({ "hashes": ["a".repeat(64)] }),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_error(&body, "UNAUTHORIZED");
    }

    /// 访客账号不接受登录：它是游客上传的归属地，不是给人用的账号。
    #[tokio::test]
    async fn guest_account_cannot_log_in() {
        let (state, _dir) = test_state().await;
        let guest = state.ensure_guest_user().await.expect("访客账号");

        let response = app(state)
            .oneshot(post_json(
                "/api/v1/auth/login",
                None,
                None,
                serde_json::json!({
                    "email": guest.email,
                    "password": "Visitor-Login-2026x",
                }),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_error(&body, "FORBIDDEN");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("访客账号"),
            "拒绝的理由要写明白：{body}"
        );
    }

    /// 管理员也打不开这扇门：改密码、改邮箱、改成管理员、切进去用，全都拒绝。
    ///
    /// 这几样是「让访客账号变成能用的账号」的全部途径（密码重置的两条路都收尾在
    /// `apply_new_password`，那里同样挡着）。配额、状态不受影响——那是管理员
    /// 按住游客上传的手段。
    #[tokio::test]
    async fn admin_cannot_open_or_impersonate_the_guest_account() {
        let (state, _dir) = test_state().await;
        let admin = add_user(&state, "root@example.com", "1000", true).await;
        let cookie = login_cookie(&state, &admin).await;
        let csrf = "csrf-1000";
        let guest = state.ensure_guest_user().await.expect("访客账号");

        // 密码与角色决定「这个账号能不能被登进来、是不是管理员」，两样都要拒。
        // 邮箱不在此列：它只是个记号（访客登不进来、也不收发信），管理员可以改
        for (field, body) in [
            (
                "password",
                serde_json::json!({ "password": "Visitor-Login-2026x" }),
            ),
            ("role", serde_json::json!({ "role": "admin" })),
        ] {
            let response = app(state.clone())
                .oneshot(patch_json(
                    &format!("/api/v1/admin/users/{}", guest.id),
                    Some(&cookie),
                    Some(csrf),
                    body,
                ))
                .await
                .expect("发请求");
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "改 {field} 必须被拒"
            );
            let payload = body_json(response).await;
            assert_error(&payload, "FORBIDDEN");
        }

        // 改邮箱放行：访客账号的邮箱只用来认「这只相册是谁的」，
        // 不走验证、不发通知——它既不登录，也没法收发信
        let response = app(state.clone())
            .oneshot(patch_json(
                &format!("/api/v1/admin/users/{}", guest.id),
                Some(&cookie),
                Some(csrf),
                serde_json::json!({ "email": "visitor-bucket@example.com" }),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK, "访客账号的邮箱可以改");
        let payload = body_json(response).await;
        assert!(
            payload["data"]["mail"].is_null(),
            "访客账号不该有变更通知邮件：{payload}"
        );
        assert_eq!(
            payload["data"]["user"]["email"], "visitor-bucket@example.com",
            "返回里要带上改后的邮箱：{payload}"
        );

        let after = state
            .repo
            .find_user_by_id(guest.id)
            .await
            .expect("读用户")
            .expect("访客账号还在");
        assert_eq!(after.password_hash, "!", "口令哈希不能被换掉");
        assert_eq!(after.role, "user", "角色不能被提成管理员");
        assert_eq!(
            after.email, "visitor-bucket@example.com",
            "邮箱这一列要给管理员留着"
        );

        // 邮件的另一条出口（重置邮件）仍然对它关着：这个账号不接受登录
        let response = app(state.clone())
            .oneshot(post_json(
                &format!("/api/v1/admin/users/{}/password-reset-mail", guest.id),
                Some(&cookie),
                Some(csrf),
                serde_json::json!({}),
            ))
            .await
            .expect("发请求");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "访客账号不该能收重置邮件"
        );
        assert_error(&body_json(response).await, "FORBIDDEN");

        // 免登录切换同样要拒：那是给访客账号发一张登录凭据
        let response = app(state.clone())
            .oneshot(post_json(
                &format!("/api/v1/admin/users/{}/impersonate", guest.id),
                Some(&cookie),
                Some(csrf),
                serde_json::json!({}),
            ))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let payload = body_json(response).await;
        assert_error(&payload, "FORBIDDEN");

        // 被拒之后不该留下会话：访客账号名下必须一张凭据都没有
        let sessions = state.repo.list_sessions(guest.id).await.expect("读会话");
        assert!(
            sessions.is_empty(),
            "访客账号不能留下任何会话：{sessions:?}"
        );
    }

    /// 后台入口对访客账号关门。
    ///
    /// 它连登录都进不来（上一个用例），这里验的是第二道：即便手里有了会话，
    /// 后台也认不出它——「后台只认人的账号」不依赖它恰好没有管理员角色。
    #[tokio::test]
    async fn guest_account_is_turned_away_from_the_admin_panel() {
        let (state, _dir) = test_state().await;
        let guest = state.ensure_guest_user().await.expect("访客账号");
        // 造一个直接指向访客账号的会话：登录进不来，就用会话本身来试这道门
        let cookie = login_cookie(&state, &guest).await;

        let response = app(state)
            .oneshot(get("/api/v1/admin/users", Some(&cookie)))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_error(&body, "FORBIDDEN");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("访客账号"),
            "要说清是访客账号被挡，而不是泛泛的权限不足：{body}"
        );
    }

    /// 上传链路端到端：Bearer 鉴权 → multipart 收件 → 魔数与尺寸解析 →
    /// 内容寻址落盘。同内容的第二份要命中去重：物理文件只有一份，
    /// 但各自生成独立的图片记录。
    #[tokio::test]
    async fn uploaded_svg_is_sanitized_before_serving() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "svg@example.com", "8801", false).await;
        let cookie = login_cookie(&state, &user).await;
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"
              onload="alert(1)">
            <script>alert(2)</script>
            <rect width="10" height="10" fill="#7fd9c4"/>
        </svg>"##;

        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf: "csrf-8801",
                },
                svg.as_bytes(),
                "evil.svg",
                &[],
                None,
            ))
            .await
            .expect("发请求");
        assert_eq!(uploaded.status(), StatusCode::OK);
        let body = body_json(uploaded).await;
        let url = body["data"]["url"]
            .as_str()
            .expect("直链")
            .replace("https://img.example.com", "");

        let response = app(state)
            .oneshot(get(&url, Some(&cookie)))
            .await
            .expect("取图");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("<script"), "脚本标签必须删掉：{text}");
        assert!(!text.contains("onload"), "事件属性必须删掉：{text}");
        assert!(!text.contains("alert(2)"), "脚本正文必须删掉：{text}");
        assert!(text.contains("<rect"), "正常图形要保留：{text}");
    }

    #[tokio::test]
    async fn upload_stores_a_real_png_and_dedups_identical_content() {
        let (state, dir) = test_state().await;
        let user = add_user(&state, "upload@example.com", "8402", false).await;

        // 库内只存哈希：按与登录相同的算法存一遍，走的是真鉴权路径
        let plaintext = "px_live_test_upload_token";
        state
            .repo
            .create_api_token(&NewApiToken {
                user_id: user.id,
                name: "测试上传".into(),
                token_hash: pixora_crypto::token::token_hash(plaintext),
                display: "px_live_test…".into(),
                scopes: vec!["upload".into(), "read".into()],
                expires_at: None,
            })
            .await
            .expect("签发令牌");

        let first = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        assert_eq!(first.status(), StatusCode::OK);
        let first = body_json(first).await;
        assert!(first["error"].is_null(), "首次上传不该失败：{first}");
        assert_eq!(first["data"]["dedup"], false, "首次上传不该是去重：{first}");
        assert_eq!(first["data"]["size"], PNG_1X1.len() as i64);
        assert_eq!(first["data"]["storage_type"], "local");
        let sha = first["data"]["sha256"]
            .as_str()
            .expect("响应带哈希")
            .to_string();
        assert_eq!(sha.len(), 64, "哈希应是 SHA-256 十六进制");
        let url = first["data"]["url"]
            .as_str()
            .expect("响应带直链")
            .to_string();
        assert!(
            url.contains(&format!("{sha}.png")),
            "直链应指向内容寻址文件：{url}"
        );

        // 字节真的落到 blobs/{前2位}/{sha}.png
        let blob_path = dir
            .path()
            .join("data")
            .join("blobs")
            .join(&sha[..2])
            .join(format!("{sha}.png"));
        assert!(
            blob_path.exists(),
            "物理文件应落盘：{}",
            blob_path.display()
        );

        // 同内容再传一次：命中去重，独立图片记录，直链不变
        let second = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        let second = body_json(second).await;
        assert_eq!(second["data"]["dedup"], true, "同内容要命中去重：{second}");
        assert_eq!(second["data"]["sha256"], sha);
        assert_eq!(second["data"]["url"], url);
        assert_ne!(
            second["data"]["id"].as_i64(),
            first["data"]["id"].as_i64(),
            "去重不等于复用图片记录"
        );
    }

    /// 上传策略按用户设置并真正生效：站点白名单收紧时全局一致地拦，
    /// 给某个用户单独放开后只有他能传；单文件上限的覆盖在收流那一步就拦住。
    #[tokio::test]
    async fn per_user_upload_policy_governs_the_upload_path() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "policy@example.com", "8403", false).await;

        let plaintext = "px_live_test_policy_token";
        state
            .repo
            .create_api_token(&NewApiToken {
                user_id: user.id,
                name: "策略测试".into(),
                token_hash: pixora_crypto::token::token_hash(plaintext),
                display: "px_live_policy…".into(),
                scopes: vec!["upload".into(), "read".into()],
                expires_at: None,
            })
            .await
            .expect("签发令牌");

        // 站点白名单收紧到不含 png：这个用户先被拦住
        state
            .settings
            .set(pixora_core::settings::keys::UPLOAD_FORMATS, "jpg,gif")
            .await
            .expect("站点格式白名单");
        let blocked = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        let blocked = body_json(blocked).await;
        assert_eq!(
            blocked["error"]["code"], "UNSUPPORTED_FORMAT",
            "站点白名单不含 png 时应当拦住：{blocked}"
        );

        // 给这个用户单独放开 png：只有他传得上去
        state
            .repo
            .upsert_user_policy(
                user.id,
                &pixora_repo::model::RateLimitPolicy {
                    formats: Some("png".into()),
                    ..pixora_repo::model::RateLimitPolicy::defaults()
                },
            )
            .await
            .expect("写用户覆盖");
        let allowed = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        assert_eq!(allowed.status(), StatusCode::OK, "单独放开后应当能传");
        let allowed = body_json(allowed).await;
        assert!(allowed["error"].is_null(), "{allowed}");

        // /me 的 limits 要带上这份生效值：客户端队列据此限流，而不是拿站点全局值
        let me = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/me")
                    .header("authorization", format!("Bearer {plaintext}"))
                    .body(Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("发请求");
        let me = body_json(me).await;
        let formats = me["data"]["user"]["limits"]["formats"]
            .as_array()
            .expect("limits.formats");
        assert_eq!(
            formats
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>(),
            vec!["png"],
            "生效格式要按用户覆盖给：{me}"
        );

        // 单文件上限的覆盖：10 字节的用户传 67 字节的 PNG，收流时就该拦。
        // 覆盖是整行写入，所以每次都带上「允许 png」——否则上一个格式覆盖被清掉，
        // 断言失败的原因会变成格式而不是尺寸
        let with_png =
            |policy: pixora_repo::model::RateLimitPolicy| pixora_repo::model::RateLimitPolicy {
                formats: Some("png".into()),
                ..policy
            };
        state
            .repo
            .upsert_user_policy(
                user.id,
                &with_png(pixora_repo::model::RateLimitPolicy {
                    max_file_size: 10,
                    ..pixora_repo::model::RateLimitPolicy::defaults()
                }),
            )
            .await
            .expect("写用户覆盖");
        let too_big = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        let too_big = body_json(too_big).await;
        assert_eq!(
            too_big["error"]["code"], "FILE_TOO_LARGE",
            "更紧的单文件上限必须生效：{too_big}"
        );

        // 像素上限同理：站点调到 1 万像素以下后，1x1 仍然合法；上限以下不该误伤
        state
            .repo
            .upsert_user_policy(
                user.id,
                &with_png(pixora_repo::model::RateLimitPolicy {
                    max_pixels: 1,
                    ..pixora_repo::model::RateLimitPolicy::defaults()
                }),
            )
            .await
            .expect("写用户覆盖");
        let pixels = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        let pixels = body_json(pixels).await;
        assert!(
            pixels["error"].is_null(),
            "1x1 正好在上限内不该被拦：{pixels}"
        );

        // 把用户覆盖整行删掉：立刻回到站点白名单（不含 png），又被拦住
        state
            .repo
            .delete_user_policy(user.id)
            .await
            .expect("删用户覆盖");
        let restored = app(state.clone())
            .oneshot(multipart_upload(
                "/api/v1/upload",
                plaintext,
                PNG_1X1,
                "pixel.png",
            ))
            .await
            .expect("发请求");
        let restored = body_json(restored).await;
        assert_eq!(
            restored["error"]["code"], "UNSUPPORTED_FORMAT",
            "恢复默认后要重新跟随站点白名单：{restored}"
        );
    }

    /// `?thumb=1` 要真的给缩略图。
    ///
    /// 它以前拿原图哈希去 `kind = 'thumb'` 里查，永远查不到，于是回落到原图——
    /// 列表页每张格子都在传几百 KB 到几 MB 的原图（真实数据里 212 KB 对 26 KB），
    /// 页面迟迟加载不完、请求被中断，表现就是「部分图片预览加载失败」。
    #[tokio::test]
    async fn thumbnail_requests_serve_the_thumbnail() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "thumb@example.com", "7701", false).await;
        let cookie = login_cookie(&state, &user).await;
        let thumb = tiny_webp();

        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf: "csrf-7701",
                },
                PNG_1X1,
                "pixel.png",
                &[],
                Some(&thumb),
            ))
            .await
            .expect("发请求");
        assert_eq!(uploaded.status(), StatusCode::OK);
        let body = body_json(uploaded).await;
        let url = body["data"]["url"]
            .as_str()
            .expect("直链")
            .replace("https://img.example.com", "");
        let thumb_url = body["data"]["thumb_url"]
            .as_str()
            .expect("缩略图直链")
            .replace("https://img.example.com", "");

        let response = app(state.clone())
            .oneshot(get(&thumb_url, Some(&cookie)))
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("image/webp"),
            "带 ?thumb=1 时要发缩略图那一份（webp）"
        );
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes();
        assert_eq!(
            bytes.as_ref(),
            thumb.as_slice(),
            "返回的应当正是客户端传上来的缩略图"
        );

        // 不带 ?thumb=1：仍是原图
        let response = app(state)
            .oneshot(get(&url, Some(&cookie)))
            .await
            .expect("发请求");
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes();
        assert_eq!(bytes.as_ref(), PNG_1X1, "原图直链要给原图");
    }

    /// 访客上传只有公开一种：那个账号登不进来，私有图片传完谁也看不见。
    #[tokio::test]
    async fn guest_uploads_are_always_public() {
        let (state, _dir) = test_state().await;
        state
            .settings
            .set(pixora_core::settings::keys::UPLOAD_REQUIRE_LOGIN, "false")
            .await
            .expect("打开游客上传");

        // 访客的 CSRF Cookie 由 /site 下发（没有会话，靠双提交）
        let site = app(state.clone())
            .oneshot(get("/api/v1/site", None))
            .await
            .expect("发请求");
        let set_cookie = site
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .expect("访客该拿到 CSRF Cookie")
            .to_string();
        let csrf = set_cookie
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("pixora_csrf=")
            .to_string();
        let guest_cookie = format!("pixora_csrf={csrf}");

        // 客户端要私有也不行：服务端按账号定，不听传进来的值
        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &guest_cookie,
                    csrf: &csrf,
                },
                PNG_1X1,
                "guest.png",
                &[("visibility", "private")],
                None,
            ))
            .await
            .expect("发请求");
        assert_eq!(uploaded.status(), StatusCode::OK, "开着游客上传时该收下");
        let body = body_json(uploaded).await;
        let id = body["data"]["id"].as_i64().expect("图片 id");

        let row = state
            .repo
            .db()
            .fetch_optional(
                "SELECT visibility FROM images WHERE id = ?",
                pixora_repo::Args::new().int(id),
            )
            .await
            .expect("查图片")
            .expect("图片行");
        assert_eq!(
            pixora_repo::model::get_str(&row, "visibility").unwrap(),
            "public",
            "访客上传必须落在公开上"
        );
    }

    /// 访客账号不随「关闭游客上传」消失，也不该被管理员删掉。
    #[tokio::test]
    async fn guest_account_outlives_the_switch_and_cannot_be_deleted() {
        let (state, _dir) = test_state().await;
        let admin = add_user(&state, "root@example.com", "1000", true).await;
        let cookie = login_cookie(&state, &admin).await;
        let guest = state.ensure_guest_user().await.expect("访客账号");

        // 开关来回切：账号与它的相册都留着
        for value in ["true", "false", "true"] {
            state
                .settings
                .set(pixora_core::settings::keys::UPLOAD_REQUIRE_LOGIN, value)
                .await
                .expect("切开关");
            assert!(
                state
                    .repo
                    .find_user_by_id(guest.id)
                    .await
                    .unwrap()
                    .is_some(),
                "关闭游客上传不该动这个账号"
            );
            assert!(
                state
                    .repo
                    .find_album_by_slug(guest.id, pixora_repo::repo::albums::DEFAULT_ALBUM_SLUG)
                    .await
                    .unwrap()
                    .is_some(),
                "访客相册也要留着"
            );
        }

        // 管理员也删不掉它：删了等于把全站游客的图一次性清空
        let response = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!(
                        "/api/v1/admin/users/{}?confirm={}",
                        guest.id, guest.email
                    ))
                    .header("cookie", &cookie)
                    .header("x-csrf-token", "csrf-1000")
                    .body(Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("发请求");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_json(response).await;
        assert_error(&body, "FORBIDDEN");
        assert!(
            state
                .repo
                .find_user_by_id(guest.id)
                .await
                .unwrap()
                .is_some(),
            "被拒之后账号必须还在"
        );
    }

    /// `reuse_existing`：同一份内容已经在图库里，就引用那一张，不再多出一条记录。
    /// 头像就是靠它解决「同一张图设两次头像，相册里并排两张一样的」。
    #[tokio::test]
    async fn upload_can_reuse_an_existing_image() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "avatar@example.com", "8801", false).await;
        let cookie = login_cookie(&state, &user).await;
        let csrf = "csrf-8801";

        let upload = |fields: Vec<(&'static str, &'static str)>| {
            multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf,
                },
                PNG_1X1,
                "avatar.png",
                &fields,
                None,
            )
        };

        let first = app(state.clone())
            .oneshot(upload(vec![("tags", "avatar")]))
            .await
            .expect("发请求");
        let first = body_json(first).await;
        let first_id = first["data"]["id"].as_i64().expect("图片 id");

        let reused = app(state.clone())
            .oneshot(upload(vec![("tags", "avatar"), ("reuse_existing", "1")]))
            .await
            .expect("发请求");
        let reused = body_json(reused).await;
        assert_eq!(reused["data"]["id"], first_id, "复用时要指回图库里那一张");
        assert_eq!(reused["data"]["reused"], true);

        // 不带这个开关时保持原样：同内容再传一次会多一条引用（图库里两张一样的图）
        let again = app(state.clone())
            .oneshot(upload(vec![]))
            .await
            .expect("发请求");
        let again = body_json(again).await;
        assert_ne!(again["data"]["id"], first_id, "普通上传仍是一条新记录");

        let count = state
            .repo
            .db()
            .count(
                "SELECT COUNT(*) AS total FROM images WHERE owner_id = ? AND deleted_at IS NULL",
                pixora_repo::Args::new().int(user.id),
            )
            .await
            .expect("数图片");
        assert_eq!(count, 2, "两次上传加一次复用：图库里应当只有两条记录");
    }

    /// 私有图片的直链要鉴权：匿名和旁人都拿到 404，只有本人与管理员能看。
    #[tokio::test]
    async fn private_direct_links_need_the_owner() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "private@example.com", "8901", false).await;
        let cookie = login_cookie(&state, &user).await;
        let other = add_user(&state, "stranger@example.com", "8902", false).await;
        let other_cookie = login_cookie(&state, &other).await;
        let admin = add_user(&state, "root@example.com", "1000", true).await;
        let admin_cookie = login_cookie(&state, &admin).await;
        let thumb = tiny_webp();

        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf: "csrf-8901",
                },
                PNG_1X1,
                "secret.png",
                &[("visibility", "private")],
                Some(&thumb),
            ))
            .await
            .expect("发请求");
        assert_eq!(uploaded.status(), StatusCode::OK);
        let body = body_json(uploaded).await;
        let url = body["data"]["url"]
            .as_str()
            .expect("直链")
            .replace("https://img.example.com", "");
        let thumb_url = body["data"]["thumb_url"]
            .as_str()
            .expect("缩略图直链")
            .replace("https://img.example.com", "");

        for (label, path) in [("原图", &url), ("缩略图", &thumb_url)] {
            let anonymous = app(state.clone())
                .oneshot(get(path, None))
                .await
                .expect("发请求");
            assert_eq!(
                anonymous.status(),
                StatusCode::NOT_FOUND,
                "匿名请求私有{label}直链应当 404"
            );
            let stranger = app(state.clone())
                .oneshot(get(path, Some(&other_cookie)))
                .await
                .expect("发请求");
            assert_eq!(
                stranger.status(),
                StatusCode::NOT_FOUND,
                "别的用户请求私有{label}直链应当 404"
            );
            let owner = app(state.clone())
                .oneshot(get(path, Some(&cookie)))
                .await
                .expect("发请求");
            assert_eq!(owner.status(), StatusCode::OK, "本人应当能看私有{label}");
            let admin_response = app(state.clone())
                .oneshot(get(path, Some(&admin_cookie)))
                .await
                .expect("发请求");
            assert_eq!(
                admin_response.status(),
                StatusCode::OK,
                "管理员应当能看私有{label}"
            );
        }
    }

    /// 回收站里的图片：本人（与管理员）拿直链能看到预览，别人拿不到——
    /// 那些软删的行对本人来说仍然「有权限」，回收站页面上的预览就靠它。
    #[tokio::test]
    async fn recycled_direct_links_stay_with_the_owner() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "recycle@example.com", "8903", false).await;
        let cookie = login_cookie(&state, &user).await;
        let other = add_user(&state, "stranger2@example.com", "8904", false).await;
        let other_cookie = login_cookie(&state, &other).await;

        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf: "csrf-8903",
                },
                PNG_1X1,
                "pixel.png",
                &[("visibility", "public")],
                Some(&tiny_webp()),
            ))
            .await
            .expect("发请求");
        let body = body_json(uploaded).await;
        let id = body["data"]["id"].as_i64().expect("图片 id");
        let url = body["data"]["url"]
            .as_str()
            .expect("直链")
            .replace("https://img.example.com", "");
        let thumb_url = body["data"]["thumb_url"]
            .as_str()
            .expect("缩略图直链")
            .replace("https://img.example.com", "");

        // 公开图片：谁都能取
        let anonymous = app(state.clone())
            .oneshot(get(&url, None))
            .await
            .expect("发请求");
        assert_eq!(anonymous.status(), StatusCode::OK);

        // 进回收站（软删）
        let deleted = app(state.clone())
            .oneshot(post_json(
                "/api/v1/images/batch",
                Some(&cookie),
                Some("csrf-8903"),
                serde_json::json!({ "action": "delete", "ids": [id] }),
            ))
            .await
            .expect("发请求");
        assert_eq!(deleted.status(), StatusCode::OK);
        assert_eq!(body_json(deleted).await["data"]["affected"], 1);

        for (label, path) in [("原图", &url), ("缩略图", &thumb_url)] {
            let owner = app(state.clone())
                .oneshot(get(path, Some(&cookie)))
                .await
                .expect("发请求");
            assert_eq!(
                owner.status(),
                StatusCode::OK,
                "本人应当能看回收站里的{label}"
            );
            let stranger = app(state.clone())
                .oneshot(get(path, Some(&other_cookie)))
                .await
                .expect("发请求");
            assert_eq!(
                stranger.status(),
                StatusCode::NOT_FOUND,
                "别人拿回收站里的{label}应当 404"
            );
            let anonymous = app(state.clone())
                .oneshot(get(path, None))
                .await
                .expect("发请求");
            assert_eq!(
                anonymous.status(),
                StatusCode::NOT_FOUND,
                "匿名拿回收站里的{label}应当 404"
            );
        }
    }

    /// 没有缩略图的图片：带 `?thumb` 的直链回落到原图，权限判定仍然按原图走。
    #[tokio::test]
    async fn thumb_requests_fall_back_to_the_original_with_the_same_guard() {
        let (state, _dir) = test_state().await;
        let user = add_user(&state, "nothumb@example.com", "8905", false).await;
        let cookie = login_cookie(&state, &user).await;

        let uploaded = app(state.clone())
            .oneshot(multipart_form(
                "/api/v1/upload",
                MultipartAuth::Session {
                    cookie: &cookie,
                    csrf: "csrf-8905",
                },
                PNG_1X1,
                "pixel.png",
                &[("visibility", "private")],
                None,
            ))
            .await
            .expect("发请求");
        let body = body_json(uploaded).await;
        assert!(
            body["data"]["thumb_url"].is_null(),
            "没传缩略图时不该给缩略图直链"
        );
        let url = body["data"]["url"].as_str().expect("直链");
        let with_thumb = format!("{url}?thumb=2");

        let anonymous = app(state.clone())
            .oneshot(get(
                &with_thumb.replace("https://img.example.com", ""),
                None,
            ))
            .await
            .expect("发请求");
        assert_eq!(
            anonymous.status(),
            StatusCode::NOT_FOUND,
            "私有图片带 ?thumb 也不能匿名取（回落的是原图，权限照原图判）"
        );
        let owner = app(state.clone())
            .oneshot(get(
                &with_thumb.replace("https://img.example.com", ""),
                Some(&cookie),
            ))
            .await
            .expect("发请求");
        assert_eq!(owner.status(), StatusCode::OK);
        assert_eq!(
            owner
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("image/png"),
            "回落时就发原图那一份"
        );
    }

    /// 账号注销之后，账号历史仍然查得到，并且看得出是管理员做的还是本人做的。
    #[tokio::test]
    async fn account_log_survives_account_deletion() {
        let (state, _dir) = test_state().await;
        let admin = add_user(&state, "root@example.com", "1000", true).await;
        let cookie = login_cookie(&state, &admin).await;
        let target = add_user(&state, "gone@example.com", "9001", false).await;

        let deleted = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!(
                        "/api/v1/admin/users/{}?confirm=gone@example.com",
                        target.id
                    ))
                    .header("cookie", &cookie)
                    .header("x-csrf-token", "csrf-1000")
                    .body(Body::empty())
                    .expect("构造请求"),
            )
            .await
            .expect("发请求");
        assert_eq!(deleted.status(), StatusCode::OK);
        assert!(
            state
                .repo
                .find_user_by_id(target.id)
                .await
                .unwrap()
                .is_none(),
            "账号确实删掉了"
        );

        // 账号没了，历史还在，而且写明是哪个管理员注销的
        let logs = app(state)
            .oneshot(get("/api/v1/admin/logs?uid=9001", Some(&cookie)))
            .await
            .expect("发请求");
        assert_eq!(logs.status(), StatusCode::OK);
        let body = body_json(logs).await;
        let items = body["data"].as_array().expect("列表");
        let row = items
            .iter()
            .find(|item| item["action"] == "account_delete")
            .unwrap_or_else(|| panic!("注销记录应当还在：{body}"));
        assert_eq!(row["target_uid"], "9001");
        assert_eq!(row["actor_kind"], "admin");
        assert!(
            row["actor_label"]
                .as_str()
                .unwrap_or_default()
                .contains("root@example.com"),
            "要写清是哪个管理员做的：{}",
            row["actor_label"]
        );
        assert_eq!(row["email"], "gone@example.com", "记录里带着注销时的邮箱");
    }
}
