//! 管理后台（`/api/v1/admin/*`）。
//!
//! 全部要求 `role = admin`（会话或带 `admin` 范围的 Token），越权会写审计。
//! 这一层有三条必须守住的规矩：
//!
//!   1. **不自己算业务**。用户列表、存储面板、配额校验、限流策略、条款发布
//!      都有领域方法，路由只做「取参数 → 调用 → 包信封」。管理后台是最容易
//!      长出第二套实现的地方（「后台就这么用一下」），长了之后两套口径必然打架。
//!   2. **密钥只进不出**。存储后端与 SMTP 的凭据入库前加密，出库时只回
//!      「配没配」（`mask_config` / `secret_configured`），永远不会把明文
//!      回显给浏览器。
//!   3. **分段写设置**。`PUT /admin/settings` 的 body 是「只写传入的键」，
//!      没传的键保持原值——一次请求把整页配置写回默认值，是运维事故的常见来源。

use std::sync::Arc;

use crate::http::{
    cookie_value, error_response, finish, finish_page, parse_i64, parse_str, request_context,
    rfc3339, CSRF_COOKIE,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::admin::Disposition;
use pixora_core::audit::actions;
use pixora_core::quota::QuotaInput;
use pixora_core::settings::{
    defaults as settings_defaults, keys, normalize_auto_refresh_seconds, normalize_prefix,
};
use pixora_core::stats::BackendView;
use pixora_core::{AppError, AppState};
use pixora_repo::model::{now, RateLimitPolicy};
use pixora_repo::repo::audit::AuditFilter;
use pixora_repo::repo::backends::{BackendPatch, NewBackend};
use pixora_repo::repo::identities::provider;
use pixora_repo::repo::iprules::{IpRuleFilter, NewIpRule};
use pixora_repo::repo::users::UserFilter;
use pixora_repo::PageRequest;

use super::{
    bad_request, field_i64, field_opt_bool, field_opt_i64, field_opt_str, field_str, not_found,
    require_admin, PeerAddr,
};

/// 管理端统一的「先过鉴权再干活」入口。
///
/// 每个 handler 都要先拿 `rectx`（IP 要进审计），再 `require_admin`。
macro_rules! admin_guard {
    ($state:expr, $headers:expr, $method:expr, $peer:expr, $path:expr) => {{
        let rectx = request_context(&$state, &$headers, $peer.addr()).await;
        let auth =
            match require_admin(&$state, &$headers, rectx.ip.as_deref(), &$method, $path).await {
                Ok(auth) => auth,
                Err(resp) => return resp,
            };
        (rectx, auth)
    }};
}

// ============================================================
// 8.1 用户
// ============================================================

/// `GET /api/v1/admin/users` — 分页用户列表。
pub async fn users_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/users");

    finish_page(
        state
            .admin_user_list(
                parse_str(&query, "query"),
                parse_str(&query, "role"),
                parse_str(&query, "status"),
                parse_i64(&query, "page"),
                parse_i64(&query, "per_page"),
            )
            .await,
    )
}

/// `GET /api/v1/admin/users/{id}` — 详情：账号 + 处置记录 + 生效限额。
///
/// 看别人的详情属于「跨用户访问」，要留审计——这是设计稿点名的合规要求。
pub async fn user_detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}");

    if let Ok(Some(target)) = state.repo.find_user_by_id(user_id).await {
        if target.id != auth.user.id {
            super::audit_admin_view(
                &state,
                &auth.user,
                &target.uid,
                "admin_user_detail",
                rectx.ip.as_deref(),
            )
            .await;
        }
    }

    match state.admin_user_detail(user_id).await {
        Ok(detail) => {
            let mut body = serde_json::to_value(&detail).unwrap_or_else(|_| serde_json::json!({}));
            add_user_detail_extras(&mut body, &detail);
            body["csrf_token"] = serde_json::to_value(cookie_value(&headers, CSRF_COOKIE))
                .unwrap_or(serde_json::Value::Null);
            finish::<serde_json::Value>(Ok(body))
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// 契约 8.1 的详情里除了 `policy` 还有一块 `limits`。
///
/// 领域层的 `AdminUserDetail` 只给 `policy`（它已经把限额字段都列全了），
/// 而契约的前端读的是 `limits`；两者是同一份数据的两种键名，
/// 所以在这里补一个 `limits` 别名，不额外查库。
fn add_user_detail_extras(
    body: &mut serde_json::Value,
    detail: &pixora_core::admin::AdminUserDetail,
) {
    let policy = &detail.policy;
    let limits = serde_json::json!({
        "api_per_minute": policy.api_per_minute,
        "api_per_hour": policy.api_per_hour,
        "api_read_per_minute": policy.api_read_per_minute,
        "api_burst": policy.api_burst,
        "upload_per_minute": policy.upload_per_minute,
        "upload_per_hour": policy.upload_per_hour,
        "upload_per_day": policy.upload_per_day,
        "upload_bytes_per_day": policy.upload_bytes_per_day,
        "max_concurrent_uploads": policy.max_concurrent_uploads,
        "max_file_size": policy.max_file_size,
        "max_batch_size": policy.max_batch_size,
        "max_pixels": policy.max_pixels,
        "max_width": policy.max_width,
        "max_height": policy.max_height,
        "formats": policy.formats,
        "download_per_minute": policy.download_per_minute,
        "download_per_day": policy.download_per_day,
        "unlimited": policy.unlimited,
        "custom": policy.custom,
        "note": policy.note,
    });
    if let Some(object) = body.as_object_mut() {
        object.insert("limits".into(), limits);
    }
}

/// 把后台提交的背景图条目归一成存储形态。
///
/// 上传图在界面上展示为公开地址，保存时再折回 `asset` / `asset:<id>`；
/// 这样「保存一次设置」不会把刚上传的图误删。外链与站内相对地址原样保留。
fn normalize_background_entry(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == keys::BACKGROUND_ASSET_MARKER {
        return Some(keys::BACKGROUND_ASSET_MARKER.to_string());
    }
    if let Some(id) = raw.strip_prefix("asset:") {
        return is_asset_id(id).then(|| format!("asset:{id}"));
    }
    if raw == keys::BACKGROUND_PUBLIC_PATH {
        return Some(keys::BACKGROUND_ASSET_MARKER.to_string());
    }
    if let Some(rest) = raw.strip_prefix(keys::BACKGROUND_PUBLIC_PREFIX) {
        let id = rest.split(|ch| ch == '?' || ch == '#').next().unwrap_or("");
        return is_asset_id(id).then(|| format!("asset:{id}"));
    }
    if raw.starts_with("https://") || raw.starts_with("http://") || raw.starts_with('/') {
        return Some(raw.to_string());
    }
    None
}

fn is_asset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn background_asset_key(entry: &str) -> Option<String> {
    if entry == keys::BACKGROUND_ASSET_MARKER {
        return Some(keys::BACKGROUND_ASSET_KEY.to_string());
    }
    let id = entry.strip_prefix("asset:")?;
    is_asset_id(id).then(|| format!("{}{id}", keys::BACKGROUND_ASSET_PREFIX))
}

/// `POST /api/v1/admin/users` — 管理端直接建号。
///
/// 邮箱直接标记为已验证：管理员手输的地址不需要再走一遍验证邮件。
/// 配额取当前默认值，建完可以在详情页单独调。
pub async fn user_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users");

    let email = field_opt_str(&body, "email")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(email) = email else {
        return bad_request("邮箱不能为空");
    };
    let password = field_opt_str(&body, "password").filter(|value| !value.is_empty());
    let Some(password) = password else {
        return bad_request("密码不能为空");
    };
    let role = field_opt_str(&body, "role");

    let detail = match state
        .admin_create_user(
            &auth.user,
            &email,
            &password,
            role.as_deref(),
            rectx.ip.as_deref(),
        )
        .await
    {
        Ok(detail) => detail,
        Err(err) => return crate::http::error_response(err),
    };

    // 与详情/更新同一个形状：补 limits 别名并按契约带上 csrf_token
    let mut payload = serde_json::to_value(&detail).unwrap_or_else(|_| serde_json::json!({}));
    add_user_detail_extras(&mut payload, &detail);
    payload["csrf_token"] = serde_json::to_value(cookie_value(&headers, CSRF_COOKIE))
        .unwrap_or(serde_json::Value::Null);
    finish::<serde_json::Value>(Ok(payload))
}

/// `PATCH /api/v1/admin/users/{id}` — 改邮箱 / 重置密码 / 调角色。
///
/// 三个字段都可选，只写传了的；领域层负责「改邮箱重置验证状态」「重置密码踢下线」
/// 「不能把最后一个管理员降级」这些规则。
pub async fn user_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}");

    let email = field_opt_str(&body, "email");
    let password = field_opt_str(&body, "password");
    let role = field_opt_str(&body, "role");
    if email.is_none() && password.is_none() && role.is_none() {
        return bad_request("没有可修改的字段（email / password / role）");
    }

    let (detail, mail) = match state
        .admin_update_user(
            &auth.user,
            user_id,
            email.as_deref(),
            password.as_deref(),
            role.as_deref(),
            rectx.ip.as_deref(),
        )
        .await
    {
        Ok((detail, mail)) => (detail, mail),
        Err(err) => return crate::http::error_response(err),
    };

    // 与详情同一个形状：补 `limits` 别名，并按契约第 11 章带上 csrf_token
    let mut body = serde_json::to_value(&detail).unwrap_or_else(|_| serde_json::json!({}));
    add_user_detail_extras(&mut body, &detail);
    // 设过密码的话再把邮件投递结果带上：界面要如实说「密码发出去了没有」
    body["mail"] = serde_json::to_value(&mail).unwrap_or(serde_json::Value::Null);
    body["csrf_token"] = serde_json::to_value(cookie_value(&headers, CSRF_COOKIE))
        .unwrap_or(serde_json::Value::Null);
    finish::<serde_json::Value>(Ok(body))
}

/// `POST /api/v1/admin/users/{id}/password-reset-mail` — 管理员代发重置邮件。
///
/// 与「直接设一个新密码」是两条路：这条把链接发给用户，让他自己设。
/// 返回真实的投递结果——管理员点的是「帮我发封信」，不能只回一句已发送。
pub async fn user_password_reset_mail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(
        state,
        headers,
        method,
        peer,
        "/admin/users/{id}/password-reset-mail"
    );

    match state
        .admin_send_reset_mail(&auth.user, user_id, rectx.ip.as_deref())
        .await
    {
        Ok(delivery) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "delivered": delivery.delivered,
            "detail": delivery.detail,
            "message": delivery.describe(),
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/admin/users/{id}/quota` — 单个用户调配额。
///
/// 校验（单用户上限、合计不超可分配上限 = 系统总额 − 系统占用）在领域层，
/// 超出时错误里带「还差多少字节」；已经越界时只拦往上加，调低放行。
pub async fn user_quota(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (_rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}/quota");

    let quota_bytes = match field_i64(&body, "quota_bytes") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    finish(
        state
            .set_user_quota(auth.user.id, user_id, quota_bytes)
            .await,
    )
}

/// `POST /api/v1/admin/users/{id}/status` — 处置：暂停 / 封禁 / 恢复。
///
/// body 直接映射到 `DispositionInput`。`action` 用 `Disposition::parse` 解析，
/// 拒绝未知动作而不是默认成「恢复」——猜错方向的默认值是安全事故。
pub async fn user_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}/status");

    let action = match field_opt_str(&body, "action")
        .map(|value| Disposition::parse(&value))
        .transpose()
    {
        Ok(Some(action)) => action,
        Ok(None) => return bad_request("缺少参数 action"),
        Err(err) => return crate::http::error_response(err),
    };

    let input = pixora_core::admin::DispositionInput {
        action,
        reason: field_opt_str(&body, "reason"),
        // 时长单位为分钟：界面上可以填小时或天，换算在这里之前就做完。
        // 不填或填 0 = 永久，只能人工解封
        duration_minutes: field_opt_i64(&body, "duration_minutes"),
        requirements: field_opt_str(&body, "requirements"),
        appeal_email: field_opt_str(&body, "appeal_email"),
        // 默认发通知：处置不通知用户，用户只会觉得「网站坏了」
        notify: field_opt_bool(&body, "notify").unwrap_or(true),
    };

    finish(
        state
            .dispose_user(&auth.user, user_id, input, rectx.ip.as_deref())
            .await,
    )
}

/// `DELETE /api/v1/admin/users/{id}?confirm=<邮箱>` — 删除账号。
///
/// 账号名下的图片、相册、令牌、会话一起删，不再被别人引用的物理文件当场回收。
/// 不可逆，所以要求把账号邮箱原样填进 `confirm`：界面让管理员照着敲一遍。
pub async fn user_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}");

    match state
        .admin_delete_user(
            &auth.user,
            user_id,
            query.get("confirm").map(String::as_str),
            rectx.ip.as_deref(),
        )
        .await
    {
        Ok(outcome) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "email": outcome.email,
            "uid": outcome.uid,
            "blobs": outcome.blobs,
            "reclaimed_files": outcome.reclaimed_files,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/admin/stats` — 后台首页的数字。
pub async fn stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/stats");
    finish(state.admin_overview().await)
}

/// `GET /api/v1/admin/sessions` — 所有账号的会话 / cookie 记录。
pub async fn sessions_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (_rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/sessions");
    match state
        .admin_session_list(
            parse_str(&query, "query"),
            parse_i64(&query, "page"),
            parse_i64(&query, "per_page"),
            auth.session_id,
        )
        .await
    {
        Ok(page) => finish_page(Ok(page)),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/admin/sessions/{id}` — 删除一条会话记录。
pub async fn session_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(session_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/sessions/{id}");
    if auth.session_id == Some(session_id) {
        return bad_request("当前正在使用的会话不能从后台删除；如需退出，请使用页面右上角的退出登录");
    }
    let target = state
        .repo
        .find_session_by_id(session_id)
        .await
        .ok()
        .flatten();
    let target_user = match target.as_ref() {
        Some(session) => state
            .repo
            .find_user_by_id(session.user_id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    match state.admin_delete_session(session_id).await {
        Ok(deleted) => {
            if deleted > 0 {
                let target_label = target_user
                    .as_ref()
                    .map(|user| user.uid.clone())
                    .unwrap_or_else(|| format!("session:{session_id}"));
                state
                    .audit_detail(
                        actions::LOGOUT,
                        Some(auth.user.id),
                        target_label,
                        serde_json::json!({
                            "via": "admin_delete_session",
                            "session_id": session_id,
                            "target_user_id": target.as_ref().map(|session| session.user_id),
                        }),
                        rectx.ip.as_deref(),
                    )
                    .await;
            }
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "deleted": deleted,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/admin/sessions` — 清理全站已结束的会话记录。
pub async fn sessions_history_clear(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/sessions");
    match state.admin_clear_session_history().await {
        Ok(deleted) => {
            if deleted > 0 {
                state
                    .audit_detail(
                        actions::LOGOUT,
                        Some(auth.user.id),
                        "sessions:history",
                        serde_json::json!({ "via": "admin_cleanup", "deleted": deleted }),
                        rectx.ip.as_deref(),
                    )
                    .await;
            }
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "deleted": deleted,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/admin/settings/oauth-health` — 第三方登录出网自检。
pub async fn oauth_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/settings/oauth-health");
    finish::<pixora_core::identities::OAuthHealth>(Ok(state.oauth_health().await))
}

// ============================================================
// 8.2 存储
// ============================================================

/// `GET /api/v1/admin/storage` — 存储面板（一次取全，避免各块口径分裂）。
pub async fn storage_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/storage");
    finish(state.storage_admin_payload().await)
}

/// `POST /api/v1/admin/storage/thumbs/rebuild` — 重算全部缩略图。
///
/// 客户端早期按 400px 生成缩略图，宽屏上放大两倍多就是糊的。改过尺寸之后
/// 老图不会自己变清楚，所以给管理员一个「按新尺寸重算一遍」的入口：
/// 这里只把游标归零并记下总数，真正的重算在后台队列里分批做。
pub async fn storage_thumbs_rebuild(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (rectx, auth) = admin_guard!(
        state,
        headers,
        method,
        peer,
        "/admin/storage/thumbs/rebuild"
    );
    match state.start_thumb_rebuild().await {
        Ok(progress) => {
            // 立刻开跑：队列那边的轮询默认是 5 分钟一轮，管理员点了按钮不该干等。
            // 队列里同样会推进这个游标，它兜的是「进程中途重启」这种情况
            let drain = state.clone();
            tokio::spawn(async move {
                loop {
                    match pixora_queue::thumbs::regenerate_batch(&drain, 20).await {
                        Ok(0) => break,
                        Ok(count) => tracing::debug!(count, "重算缩略图"),
                        Err(err) => {
                            tracing::warn!(error = %err, "重算缩略图中断");
                            break;
                        }
                    }
                }
            });
            state
                .audit_detail(
                    actions::STORAGE_CHANGE,
                    Some(auth.user.id),
                    "thumb.rebuild",
                    serde_json::json!({ "total": progress.total }),
                    rectx.ip.as_deref(),
                )
                .await;
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "ok": true,
                "running": progress.running,
                "done": progress.done,
                "total": progress.total,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `PUT /api/v1/admin/storage/default` — 切换默认后端。
///
/// 优先认 `backend_id`（精确到实例）；只给 `driver` 时找同类型的实例，
/// 找不到且是 `local` 就补一行——「把默认后端设回本地」是回滚操作，
/// 不该因为「库里还没有 local 这行配置」而失败。
pub async fn storage_default(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/default");

    let backend_id = field_opt_i64(&body, "backend_id");
    let driver = field_opt_str(&body, "driver");

    let target_id = match (backend_id, driver.as_deref()) {
        (Some(id), _) => match state.repo.find_backend(id).await {
            Ok(Some(backend)) => backend.id,
            Ok(None) => return not_found("存储后端"),
            Err(err) => return crate::http::error_response(err.into()),
        },
        (None, Some(driver)) => {
            if !pixora_storage::names::all().contains(&driver) {
                return bad_request(format!("未知的存储驱动：{driver}"));
            }
            match state.repo.find_backend_by_driver(driver).await {
                Ok(Some(backend)) => backend.id,
                Ok(None) if driver == pixora_storage::names::LOCAL => {
                    // 库里没有 local 行：补一行，让「回退本地」这个操作始终可用
                    match state
                        .repo
                        .create_backend(&NewBackend {
                            driver: pixora_storage::names::LOCAL.to_string(),
                            label: pixora_storage::names::label(driver).to_string(),
                            config: "{}".to_string(),
                            is_default: false,
                            enabled: true,
                            capacity_mode: "auto".to_string(),
                            capacity_total: None,
                        })
                        .await
                    {
                        Ok(backend) => backend.id,
                        Err(err) => return crate::http::error_response(err.into()),
                    }
                }
                Ok(None) => {
                    return bad_request(format!("{driver} 还没有配置后端实例，请先添加"));
                }
                Err(err) => return crate::http::error_response(err.into()),
            }
        }
        (None, None) => return bad_request("缺少参数 driver 或 backend_id"),
    };

    if let Err(err) = state.repo.set_default_backend(target_id).await {
        return crate::http::error_response(err.into());
    }
    // 装配是缓存式：改完必须重建注册表，否则「改了默认后端但上传还写老地方」
    if let Err(err) = state.storage.reload().await {
        return crate::http::error_response(err);
    }
    let actor = auth.user.id;
    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(actor),
            format!("default:{target_id}"),
            serde_json::json!({ "backend_id": target_id }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "backend_id": target_id,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `PUT /api/v1/admin/storage/prefix` — 改直链一级目录。
///
/// 改完旧链依然有效（老文件的实际键没动，只是新拼的地址变了）。
/// 值要过 `normalize_prefix`：它进 URL 的第一段，斜杠与点都可能被用来构造奇怪路由。
pub async fn storage_prefix(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/prefix");

    let raw = match field_str(&body, "upload_prefix") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let normalized = normalize_prefix(&raw);
    if let Err(err) = state.settings.set(keys::UPLOAD_PREFIX, &normalized).await {
        return crate::http::error_response(err);
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(actor),
            "upload_prefix",
            serde_json::json!({ "from": raw, "to": normalized }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "upload_prefix": normalized,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `PUT /api/v1/admin/storage/capacity` — 容量模式与手填总额。
pub async fn storage_capacity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/capacity");

    let mode = match field_opt_str(&body, "mode") {
        Some(value) => {
            let mode = value.trim().to_ascii_lowercase();
            if !matches!(mode.as_str(), "auto" | "manual" | "hidden") {
                return bad_request(format!(
                    "容量模式只能是 auto / manual / hidden，收到 {value}"
                ));
            }
            Some(mode)
        }
        None => None,
    };
    let manual_total = field_opt_i64(&body, "manual_total");
    if manual_total.is_some_and(|value| value <= 0) {
        return bad_request("系统总额必须是正数");
    }

    if let Some(mode) = mode.as_deref() {
        if let Err(err) = state.settings.set(keys::CAPACITY_MODE, mode).await {
            return crate::http::error_response(err);
        }
    }
    if let Some(total) = manual_total {
        if let Err(err) = state
            .settings
            .set_integer(keys::CAPACITY_MANUAL_TOTAL, total)
            .await
        {
            return crate::http::error_response(err);
        }
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            "capacity",
            serde_json::json!({ "mode": mode, "manual_total": manual_total }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `POST /api/v1/admin/storage/backends` — 新增后端实例。
///
/// `config` 必须过 `encrypt_config`：敏感字段（token / password / secret_key…）
/// 在库里只能以密文存在，装配时由 `StorageService` 解密。
pub async fn backend_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/backends");

    let driver = match field_str(&body, "driver") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if !pixora_storage::names::all().contains(&driver.as_str()) {
        return bad_request(format!("未知的存储驱动：{driver}"));
    }
    let label = field_opt_str(&body, "label")
        .unwrap_or_else(|| pixora_storage::names::label(&driver).to_string());
    let config = body
        .get("config")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if !config.is_object() {
        return bad_request("config 必须是对象");
    }

    let encrypted = match state.storage.encrypt_config(&config) {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };

    let created = match state
        .repo
        .create_backend(&NewBackend {
            driver: driver.clone(),
            label: label.clone(),
            config: encrypted,
            is_default: false,
            enabled: field_opt_bool(&body, "enabled").unwrap_or(true),
            capacity_mode: field_opt_str(&body, "capacity_mode").unwrap_or_else(|| "auto".into()),
            capacity_total: field_opt_i64(&body, "capacity_total"),
        })
        .await
    {
        Ok(backend) => backend,
        Err(err) => return crate::http::error_response(err.into()),
    };
    if let Err(err) = state.storage.reload().await {
        return crate::http::error_response(err);
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(actor),
            format!("backend:{}", created.id),
            serde_json::json!({ "driver": driver, "label": label, "action": "create" }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(backend_payload(
        &backend_view(&created),
        cookie_value(&headers, CSRF_COOKIE),
    )))
}

/// 后端行的对外载荷：配置里的敏感字段只回显布尔，并按契约第 11 章带上 `csrf_token`。
fn backend_payload(view: &BackendView, csrf_token: Option<String>) -> serde_json::Value {
    let mut body = serde_json::to_value(view).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "csrf_token".into(),
            serde_json::to_value(csrf_token).unwrap_or(serde_json::Value::Null),
        );
    }
    body
}

/// `PATCH /api/v1/admin/storage/backends/{id}` — 改后端。
///
/// `config` 是**增量**而不是整体替换：界面上只改一个字段（比如换 repo），
/// 整体替换会把没碰过的字段（包括已经加密的 token）抹掉。
pub async fn backend_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(backend_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/backends/{id}");

    let existing = match state.repo.find_backend(backend_id).await {
        Ok(Some(backend)) => backend,
        Ok(None) => return not_found("存储后端"),
        Err(err) => return crate::http::error_response(err.into()),
    };

    let config_text = match body.get("config").cloned() {
        Some(patch) => {
            if !patch.is_object() {
                return bad_request("config 必须是对象");
            }
            let merged = merge_backend_config(&existing.config, &patch);
            match state.storage.encrypt_config(&merged) {
                Ok(value) => Some(value),
                Err(err) => return crate::http::error_response(err),
            }
        }
        // 只改了 label / 开关时不该重写 config：重写一次就要再加密一遍，
        // 白白增加「把密文洗坏」的机会
        None => None,
    };

    let patch = BackendPatch {
        label: field_opt_str(&body, "label"),
        config: config_text,
        enabled: field_opt_bool(&body, "enabled"),
        capacity_mode: field_opt_str(&body, "capacity_mode"),
        capacity_total: super::field_present(&body, "capacity_total")
            .then(|| field_opt_i64(&body, "capacity_total")),
    };
    if let Err(err) = state.repo.update_backend(backend_id, &patch).await {
        return crate::http::error_response(err.into());
    }
    if let Err(err) = state.storage.reload().await {
        return crate::http::error_response(err);
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(actor),
            format!("backend:{backend_id}"),
            serde_json::json!({ "action": "update", "fields": body.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() }),
            rectx.ip.as_deref(),
        )
        .await;

    match state.repo.find_backend(backend_id).await {
        Ok(Some(updated)) => finish::<serde_json::Value>(Ok(backend_payload(
            &backend_view(&updated),
            cookie_value(&headers, CSRF_COOKIE),
        ))),
        Ok(None) => not_found("存储后端"),
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// `DELETE /api/v1/admin/storage/backends/{id}`。
pub async fn backend_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(backend_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/backends/{id}");

    let backend = match state.repo.find_backend(backend_id).await {
        Ok(Some(backend)) => backend,
        Ok(None) => return not_found("存储后端"),
        Err(err) => return crate::http::error_response(err.into()),
    };

    // 还挂着实体就不让删：配置行一删，驱动就装配不出来了（远端驱动的地址与凭据
    // 都在配置行里），那些对象再也没人读得到、也删不掉。宁可让管理员先把图挪走
    // 或删掉，也不要留下永远收不回的孤儿。
    let attached = state
        .repo
        .count_blobs_for_backend(backend_id)
        .await
        .unwrap_or(0);
    if attached > 0 {
        return crate::http::error_response(AppError::Conflict(format!(
            "这个后端上还有 {attached} 个实体（原图与缩略图）：先把它们删掉，或把默认后端切到别处并清理，再删这个后端。配置行一删，那些文件就再也读不出来也删不掉了。"
        )));
    }
    // backend_id 为空的历史实体也算在这个驱动名下：本地与数据库内两种驱动共用
    // 一份落点，删掉配置行同样会让它们变成孤儿
    if backend.driver == pixora_storage::names::LOCAL
        || backend.driver == pixora_storage::names::DATABASE
    {
        let by_driver = state
            .repo
            .count_blobs_by_driver(&backend.driver)
            .await
            .unwrap_or(0);
        if by_driver > attached {
            return crate::http::error_response(AppError::Conflict(format!(
                "这个驱动上还有 {} 个历史实体（没绑定到具体后端）：先清理它们再删。",
                by_driver - attached
            )));
        }
    }

    if let Err(err) = state.repo.delete_backend(backend_id).await {
        return crate::http::error_response(err.into());
    }
    if let Err(err) = state.storage.reload().await {
        return crate::http::error_response(err);
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(actor),
            format!("backend:{backend_id}"),
            serde_json::json!({ "action": "delete" }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `POST /api/v1/admin/storage/backends/{id}/test` — 连通性自检。
pub async fn backend_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(backend_id): Path<i64>,
) -> Response {
    let (_rectx, _auth) = admin_guard!(
        state,
        headers,
        method,
        peer,
        "/admin/storage/backends/{id}/test"
    );

    match state.storage.test_backend(backend_id).await {
        Ok(message) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "message": message,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// 后端配置的增量合并。
///
/// 规则：以库里那份为底，body 里出现的键覆盖它。敏感字段的「空值」
/// （空串、或界面回显的布尔掩码）一律视为「不改」——把掩码 `true` 当成
/// 新 token 存进去，会让后端立刻 401，而界面上看不出任何异常。
fn merge_backend_config(
    existing: &serde_json::Value,
    patch: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = existing.as_object().cloned().unwrap_or_default();
    if let Some(fields) = patch.as_object() {
        for (key, value) in fields {
            if pixora_core::storage_svc::SECRET_FIELDS.contains(&key.as_str()) {
                let untouched = value
                    .as_str()
                    .map(|text| text.trim().is_empty())
                    .unwrap_or(true);
                if untouched {
                    continue;
                }
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    serde_json::Value::Object(merged)
}

/// 后端行 → 面板行（敏感字段只回显布尔）。
fn backend_view(backend: &pixora_repo::model::StorageBackend) -> BackendView {
    BackendView {
        id: backend.id,
        driver: backend.driver.clone(),
        label: backend.label.clone(),
        is_default: backend.is_default,
        enabled: backend.enabled,
        capacity_mode: backend.capacity_mode.clone(),
        capacity_total: backend.capacity_total,
        config: pixora_core::storage_svc::StorageService::mask_config(&backend.config),
    }
}

// ============================================================
// 8.3 配额与容量
// ============================================================

/// `GET /api/v1/admin/quota`。
pub async fn quota_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/quota");
    finish(state.quota_payload().await)
}

/// `PUT /api/v1/admin/quota` — 整页配额配置。
///
/// 字段缺少时保持原值（`QuotaInput` 全是 `Option`），校验失败时库里
/// 一点都不改——「保存成功 = 校验通过」这条必须成立。
pub async fn quota_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (_rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/quota");

    let input: QuotaInput = match super::parse_body(&body, "配额配置") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match state.quota_update(auth.user.id, input).await {
        // 保存后把整页生效值回给界面（与 GET 同一个形状）：改了哪几项、允许
        // 改到多少、注册入口现在开没开，页面上立刻是服务端确认过的那份，
        // 不用靠「保存成功」这四个字去猜
        Ok(()) => match state.quota_payload().await {
            Ok(payload) => finish(Ok(payload)),
            Err(err) => crate::http::error_response(err),
        },
        Err(err) => crate::http::error_response(err),
    }
}

// ============================================================
// 8.4 频率限制
// ============================================================

/// 限流触发后的处置策略默认值。存在设置表里（JSON），没有键时用这套。
fn on_breach_default() -> serde_json::Value {
    serde_json::json!({
        "respond": true,
        "audit": true,
        "temp_ban": true,
        "threshold": pixora_core::settings::defaults::BAN_THRESHOLD,
        "threshold_minutes": 10,
        "ban_minutes": pixora_core::settings::defaults::BAN_MINUTES,
    })
}

/// 读 `on_breach` 设置。库里存的是 JSON 文本，读不出来就用默认值。
async fn read_on_breach(state: &AppState) -> serde_json::Value {
    let raw = state
        .settings
        .raw(keys::RATELIMIT_ON_BREACH)
        .await
        .ok()
        .flatten();
    match raw
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        Some(Ok(serde_json::Value::Object(map))) => {
            let mut out = on_breach_default();
            if let Some(target) = out.as_object_mut() {
                for (key, value) in map {
                    target.insert(key, value);
                }
            }
            out
        }
        _ => on_breach_default(),
    }
}

/// `GET /api/v1/admin/ratelimit` — 策略、队列深度、用户覆盖与实时用量。
pub async fn ratelimit_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit");

    let global = match state.repo.global_policy().await {
        Ok(policy) => policy,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let upload_settings = match state.settings.upload().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let download_settings = match state.settings.download().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let depth = match state.repo.queue_depth().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };
    // 用户覆盖表一页给够（200 行）：管理页要一眼看到「谁被单独调过」
    let rows = match state
        .repo
        .list_user_policies(
            &UserFilter::default(),
            PageRequest::new(Some(1), Some(200), 50, 200),
        )
        .await
    {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };

    let mut users = Vec::with_capacity(rows.items.len());
    let mut realtime = Vec::with_capacity(rows.items.len());
    for row in &rows.items {
        let effective = row.policy.clone().unwrap_or_else(|| global.clone());
        // 上传策略的生效值（单用户覆盖 → 全局默认 → 系统设置）：弹窗拿它做基线，
        // 界面展示的是解析后的数字，不会让人看到「0 = 跟随」这种内部约定
        let upload = pixora_core::uploads::EffectiveUploadLimits::resolve(
            &upload_settings,
            &global,
            row.policy.as_ref(),
        );
        let counters = state
            .limits
            .counters_for(row.user.id)
            .await
            .unwrap_or_default();
        users.push(serde_json::json!({
            "uid": row.user.uid,
            "email": row.user.email,
            "role": row.user.role,
            "status": row.user.status,
            "custom": row.policy.is_some(),
            "unlimited": effective.unlimited,
            "api_per_minute": effective.api_per_minute,
            "upload_per_minute": effective.upload_per_minute,
            "upload_per_day": effective.upload_per_day,
            "upload_bytes_per_day": effective.upload_bytes_per_day,
            "upload": {
                "max_file_size": upload.max_file_bytes,
                "max_batch_size": upload.max_batch,
                "max_pixels": upload.max_pixels,
                "max_width": upload.max_width,
                "max_height": upload.max_height,
                "formats": upload.formats,
                "max_concurrent_uploads": upload.max_concurrent_uploads,
            },
            "download_per_minute": effective.download_per_minute,
            "download_per_day": effective.download_per_day,
            "hits_24h": row.hits_24h,
        }));
        realtime.push(serde_json::json!({
            "uid": row.user.uid,
            "email": row.user.email,
            "status": row.user.status,
            "api_minute": counters.api_minute,
            "api_minute_limit": effective.api_per_minute,
            "upload_minute": counters.upload_minute,
            "upload_minute_limit": effective.upload_per_minute,
            "upload_day": counters.upload_day,
            "upload_day_limit": effective.upload_per_day,
            "hits_24h": row.hits_24h,
        }));
    }

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "global": policy_json(&global),
        "global_enabled": state
            .settings
            .bool(keys::RATELIMIT_GLOBAL_ENABLED, false)
            .await
            .unwrap_or(false),
        // 站点级上传策略（系统设置里那份）：界面上的「单文件上限 / 单批张数 /
        // 队列深度 / 格式白名单」全局值就是它——按用户覆盖时以它为底线
        "upload_policy": {
            "max_file_bytes": upload_settings.max_file_bytes,
            "max_batch_size": upload_settings.max_batch,
            "max_pixels": upload_settings.max_pixels,
            "max_width": upload_settings.max_width,
            "max_height": upload_settings.max_height,
            "queue_depth": upload_settings.queue_depth,
            "formats": upload_settings.formats,
        },
        // 游客的下载限额：登录用户那两格在 `global` 里（可按用户覆盖），
        // 游客没有账号可挂，只能放站点设置里
        "download_policy": {
            "anon_per_minute": download_settings.anon_per_minute,
            "anon_per_day": download_settings.anon_per_day,
        },
        "on_breach": read_on_breach(&state).await,
        "queue_depth": { "threshold": upload_settings.queue_depth, "current": depth.total() },
        "users": users,
        "realtime": realtime,
    })))
}

fn policy_json(policy: &RateLimitPolicy) -> serde_json::Value {
    serde_json::json!({
        "api_per_minute": policy.api_per_minute,
        "api_per_hour": policy.api_per_hour,
        "api_read_per_minute": policy.api_read_per_minute,
        "api_burst": policy.api_burst,
        "upload_per_minute": policy.upload_per_minute,
        "upload_per_hour": policy.upload_per_hour,
        "upload_per_day": policy.upload_per_day,
        "upload_bytes_per_day": policy.upload_bytes_per_day,
        "max_concurrent_uploads": policy.max_concurrent_uploads,
        // 上传策略的三项：0 / null 表示「跟随上一层」（系统设置），
        // 界面上的全局输入直接写系统设置，这几列留给单用户覆盖用
        "max_batch_size": policy.max_batch_size,
        "max_file_size": policy.max_file_size,
        "max_pixels": policy.max_pixels,
        "max_width": policy.max_width,
        "max_height": policy.max_height,
        "formats": policy
            .formats
            .as_deref()
            .map(pixora_core::settings::parse_format_list)
            .unwrap_or_default(),
        "download_per_minute": policy.download_per_minute,
        "download_per_day": policy.download_per_day,
        "unlimited": policy.unlimited,
    })
}

/// `PUT /api/v1/admin/ratelimit/global` — 写全局默认策略。
///
/// body 里缺的字段沿用当前值，而不是回落到内置默认——「只调上传额度」
/// 不该顺手把 API 额度也改掉。
pub async fn ratelimit_global_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit/global");

    let current = match state.repo.global_policy().await {
        Ok(policy) => policy,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let policy = policy_with(Some(&current), &body);

    if let Err(err) = state.repo.upsert_global_policy(&policy).await {
        return crate::http::error_response(err.into());
    }
    // 游客的下载限额是系统设置（游客没有账号，挂不进策略表）。同一个卡片一起
    // 提交，所以在这里一并落库——分成两个接口会让「保存」按两次才有用
    for (key, value) in [
        (
            keys::DOWNLOAD_ANON_PER_MINUTE,
            field_opt_i64(&body, "download_anon_per_minute"),
        ),
        (
            keys::DOWNLOAD_ANON_PER_DAY,
            field_opt_i64(&body, "download_anon_per_day"),
        ),
    ] {
        if let Some(value) = value {
            if value < 0 {
                return bad_request("下载限额不能是负数（0 表示不限）");
            }
            if let Err(err) = state.settings.set_integer(key, value).await {
                return crate::http::error_response(err);
            }
        }
    }
    if let Some(enabled) = field_opt_bool(&body, "global_enabled") {
        if let Err(err) = state
            .settings
            .set_bool(keys::RATELIMIT_GLOBAL_ENABLED, enabled)
            .await
        {
            return crate::http::error_response(err);
        }
    }
    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            "ratelimit:global",
            serde_json::json!({ "after": policy_json(&policy) }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// 用请求体覆盖一份策略。
///
/// `base` 为 `None` 时从内置默认值起步（用户级覆盖没有「上一版」可继承时）。
/// `user_id` 与 `id` 由调用方补：契约里 body 是完整的策略字段，不含 id。
fn policy_with(base: Option<&RateLimitPolicy>, body: &serde_json::Value) -> RateLimitPolicy {
    let fallback = base.cloned().unwrap_or_else(RateLimitPolicy::defaults);
    RateLimitPolicy {
        id: 0,
        user_id: fallback.user_id,
        api_per_minute: field_opt_i64(body, "api_per_minute").unwrap_or(fallback.api_per_minute),
        api_per_hour: field_opt_i64(body, "api_per_hour").unwrap_or(fallback.api_per_hour),
        api_read_per_minute: field_opt_i64(body, "api_read_per_minute")
            .unwrap_or(fallback.api_read_per_minute),
        api_burst: field_opt_i64(body, "api_burst").unwrap_or(fallback.api_burst),
        upload_per_minute: field_opt_i64(body, "upload_per_minute")
            .unwrap_or(fallback.upload_per_minute),
        upload_per_hour: field_opt_i64(body, "upload_per_hour").unwrap_or(fallback.upload_per_hour),
        upload_per_day: field_opt_i64(body, "upload_per_day").unwrap_or(fallback.upload_per_day),
        upload_bytes_per_day: field_opt_i64(body, "upload_bytes_per_day")
            .unwrap_or(fallback.upload_bytes_per_day),
        max_concurrent_uploads: field_opt_i64(body, "max_concurrent_uploads")
            .unwrap_or(fallback.max_concurrent_uploads),
        max_batch_size: field_opt_i64(body, "max_batch_size").unwrap_or(fallback.max_batch_size),
        max_file_size: field_opt_i64(body, "max_file_size").unwrap_or(fallback.max_file_size),
        max_pixels: field_opt_i64(body, "max_pixels").unwrap_or(fallback.max_pixels),
        max_width: field_opt_i64(body, "max_width").unwrap_or(fallback.max_width),
        max_height: field_opt_i64(body, "max_height").unwrap_or(fallback.max_height),
        formats: field_opt_formats(body, "formats").or(fallback.formats),
        download_per_minute: field_opt_i64(body, "download_per_minute")
            .unwrap_or(fallback.download_per_minute),
        download_per_day: field_opt_i64(body, "download_per_day")
            .unwrap_or(fallback.download_per_day),
        unlimited: field_opt_bool(body, "unlimited").unwrap_or(fallback.unlimited),
        note: field_opt_str(body, "note").or(fallback.note),
        updated_at: now(),
    }
}

/// 请求体里的格式白名单：接受字符串数组（界面发这个）或逗号分隔的字符串
/// （手写 curl 常见）。空数组按「不覆盖」处理——空串表示跟随上一层，
/// 想清空覆盖得用 DELETE 恢复默认，不该靠传空数组把它变成「谁都不能传」。
fn field_opt_formats(body: &serde_json::Value, key: &str) -> Option<String> {
    match body.get(key) {
        Some(serde_json::Value::Array(items)) => {
            let list: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
            (!list.is_empty()).then(|| list.join(","))
        }
        Some(serde_json::Value::String(text)) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        _ => None,
    }
}

/// `PUT /api/v1/admin/ratelimit/users/{uid}` — 用户级覆盖。
///
/// 路径里给的是 **uid**（对外标识），而策略表的外键是自增 id，
/// 所以这里要换一次；换不出来就是 404，不能把 uid 当 id 用。
pub async fn ratelimit_user_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(uid): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit/users/{uid}");

    let target = match state.repo.find_user_by_uid(&uid).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found("用户"),
        Err(err) => return crate::http::error_response(err.into()),
    };

    // 用户级覆盖从「全局默认」起步：界面上填的是完整策略字段，
    // 但没填的那些应当继承全局，而不是继承内置默认值
    let base = match state.repo.global_policy().await {
        Ok(policy) => policy,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let mut policy = policy_with(Some(&base), &body);
    policy.id = 0; // 由仓储按 user_id 做 UPSERT
    policy.user_id = Some(target.id);
    policy.updated_at = now();

    match state
        .admin_set_user_policy(&auth.user, target.id, Some(policy), rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/admin/ratelimit/users/{uid}` — 恢复继承全局默认。
pub async fn ratelimit_user_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(uid): Path<String>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit/users/{uid}");

    let target = match state.repo.find_user_by_uid(&uid).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found("用户"),
        Err(err) => return crate::http::error_response(err.into()),
    };

    match state
        .admin_set_user_policy(&auth.user, target.id, None, rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `PUT /api/v1/admin/ratelimit/on_breach` — 触发限流后的处置策略。
///
/// 整块存成 JSON（阈值与封禁时长另写一份到各自的键，便于其它组件读）。
pub async fn ratelimit_breach_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit/on_breach");

    let mut merged = read_on_breach(&state).await;
    if let (Some(target), Some(source)) = (merged.as_object_mut(), body.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    let text = match serde_json::to_string(&merged) {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(AppError::internal(err.to_string())),
    };
    if let Err(err) = state.settings.set(keys::RATELIMIT_ON_BREACH, &text).await {
        return crate::http::error_response(err);
    }
    // 阈值与时长是其它组件会单独读的两项，同步写一份，避免两处口径不一致
    if let Some(threshold) = field_opt_i64(&merged, "threshold") {
        if let Err(err) = state
            .settings
            .set_integer(keys::RATELIMIT_BAN_THRESHOLD, threshold)
            .await
        {
            return crate::http::error_response(err);
        }
    }
    if let Some(minutes) = field_opt_i64(&merged, "ban_minutes") {
        if let Err(err) = state
            .settings
            .set_integer(keys::RATELIMIT_BAN_MINUTES, minutes)
            .await
        {
            return crate::http::error_response(err);
        }
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            "ratelimit:on_breach",
            serde_json::json!({ "after": merged }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "on_breach": merged,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `GET /api/v1/admin/ratelimit/logs` — 限流事件分页（来自审计日志）。
pub async fn ratelimit_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/ratelimit/logs");

    // uid → 数字 id：审计表按 actor_id 记，契约里筛选参数给的是 uid
    let actor_id = match parse_str(&query, "uid") {
        Some(uid) => match state.repo.find_user_by_uid(uid).await {
            Ok(Some(user)) => Some(user.id),
            // 查无此人时给一个不可能命中的 id，而不是"不过滤"——
            // 「筛了但没筛掉」比「什么都查不到」更容易误导排查的人
            Ok(None) => Some(-1),
            Err(err) => return crate::http::error_response(err.into()),
        },
        None => None,
    };

    let filter = AuditFilter {
        actor_id,
        action: Some(actions::RATE_LIMITED.to_string()),
        ..Default::default()
    };
    let page = super::page_request(&query, 20, 100);

    match state.list_audit(&filter, page).await {
        Ok(result) => {
            let items: Vec<serde_json::Value> = result
                .items
                .iter()
                .map(|log| {
                    serde_json::json!({
                        "id": log.id,
                        "actor_id": log.actor_id,
                        "action": log.action,
                        "target": log.target,
                        "detail": log.detail,
                        "ip": log.ip,
                        "created_at": rfc3339(log.created_at),
                    })
                })
                .collect();
            finish_page::<serde_json::Value>(Ok(pixora_repo::Page::new(items, result.total, page)))
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/admin/logs` — 账号历史（后台「系统日志」那一栏）。
///
/// 注册、换邮箱、处置、注销与登录记录都在这里。记录以 UID 为目标，账号删掉
/// 之后流水仍在，每条还会说明是本人做的还是管理员做的——这两点正是它和
/// 用户详情页那张「处置记录」的区别（那张随账号一起消失）。
///
/// `action` 支持逗号分隔的一串动作名，也接受空（用默认那一组账号动作）；
/// `uid` 精确筛某个账号，`query` 在 UID、详情与 IP 里找关键词。
pub async fn account_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/logs");

    let filter = pixora_core::account_log::AccountLogFilter {
        action: parse_str(&query, "action").map(str::to_string),
        uid: parse_str(&query, "uid").map(str::to_string),
        query: parse_str(&query, "query").map(str::to_string),
    };
    let page = super::page_request(&query, 30, 200);
    match state.account_log(&filter, page).await {
        Ok(result) => finish_page::<pixora_core::account_log::AccountLogRow>(Ok(result)),
        Err(err) => crate::http::error_response(err),
    }
}

// ============================================================
// 8.5 IP 规则
// ============================================================
/// `GET /api/v1/admin/ip` — 策略 + 规则清单。
pub async fn ip_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/ip");

    let policy = match state.ip_guard.policy().await {
        Ok(policy) => policy,
        Err(err) => return crate::http::error_response(err),
    };
    let filter = IpRuleFilter::default();
    let rules = match state.ip_guard.list(&filter, 200).await {
        Ok(rules) => rules,
        Err(err) => return crate::http::error_response(err),
    };
    let count = match state.repo.count_ip_rules(&filter).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "policy": {
            "enabled": policy.enabled,
            "trust_proxy": policy.trust_proxy,
            "proxy_hops": policy.proxy_hops,
            "auto_ban": policy.auto_ban,
            "fail_threshold": policy.fail_threshold,
            "fail_window_minutes": policy.fail_window_minutes,
            "ban_minutes": policy.ban_minutes,
        },
        "rules": rules.iter().map(ip_rule_json).collect::<Vec<_>>(),
        "count": count,
    })))
}

fn ip_rule_json(rule: &pixora_repo::model::IpRule) -> serde_json::Value {
    serde_json::json!({
        "id": rule.id,
        "cidr": rule.cidr,
        "kind": rule.kind,
        "scope": rule.scope,
        "scope_label": ip_scope_label(&rule.scope),
        "note": rule.note,
        "hits": rule.hits,
        "is_auto": rule.is_auto,
        "expires_at": rule.expires_at.map(rfc3339),
        "created_at": rfc3339(rule.created_at),
    })
}

fn ip_scope_label(scope: &str) -> &'static str {
    match scope {
        "register" => "仅注册",
        "login" => "仅登录",
        "upload" => "仅上传",
        _ => "全站",
    }
}

/// `POST /api/v1/admin/ip/rules` — 新增规则。
///
/// CIDR 必须能被 `ipnet` 解析：规则缓存里解析失败的网段会被跳过（不误伤），
/// 但那等于「管理员以为加上了、其实没生效」，所以在入口就拒绝。
pub async fn ip_rule_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ip/rules");

    let cidr = match field_str(&body, "cidr") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if cidr.parse::<ipnet::IpNet>().is_err() && cidr.parse::<std::net::IpAddr>().is_err() {
        return bad_request(format!("{cidr} 不是合法的 IP 或网段"));
    }

    let kind = field_opt_str(&body, "kind").unwrap_or_else(|| "deny".into());
    if !matches!(kind.as_str(), "deny" | "allow") {
        return bad_request("kind 只能是 deny 或 allow");
    }
    let scope = field_opt_str(&body, "scope").unwrap_or_else(|| "all".into());
    if !matches!(scope.as_str(), "all" | "register" | "login" | "upload") {
        return bad_request("scope 只能是 all / register / login / upload");
    }
    let expires_at = match field_opt_str(&body, "expires_at") {
        Some(raw) => match chrono::DateTime::parse_from_rfc3339(&raw) {
            Ok(at) => Some(at.with_timezone(&chrono::Utc)),
            Err(_) => return bad_request("expires_at 必须是 RFC3339 时间串"),
        },
        None => None,
    };

    let created = match state
        .repo
        .create_ip_rule(&NewIpRule {
            cidr: cidr.clone(),
            kind: kind.clone(),
            scope: scope.clone(),
            note: field_opt_str(&body, "note"),
            is_auto: false,
            expires_at,
            created_by: Some(auth.user.id),
        })
        .await
    {
        Ok(rule) => rule,
        Err(err) => return crate::http::error_response(err.into()),
    };
    // 缓存 30 秒，改完立刻失效一次，否则管理员会以为「加了没生效」
    state.ip_guard.invalidate();

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::IP_RULE_CHANGE,
            Some(actor),
            cidr,
            serde_json::json!({ "kind": kind, "scope": scope, "action": "create" }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(ip_rule_json(&created)))
}

/// `DELETE /api/v1/admin/ip/rules/{id}` — 删除规则（也用于提前解除自动封禁）。
pub async fn ip_rule_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(rule_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ip/rules/{id}");

    if let Err(err) = state.repo.delete_ip_rule(rule_id).await {
        return crate::http::error_response(err.into());
    }
    state.ip_guard.invalidate();

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::IP_RULE_CHANGE,
            Some(actor),
            format!("rule:{rule_id}"),
            serde_json::json!({ "action": "delete" }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `PUT /api/v1/admin/ip/policy` — IP 策略。
///
/// `trust_proxy` 与 `proxy_hops` 是安全开关：设错（比如信任 0 层却又依赖
/// 转发头）会让黑名单失效或把所有用户看成同一个 IP，所以项项都写、项项都记审计。
pub async fn ip_policy_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/ip/policy");

    let writes: [(&str, Option<String>); 6] = [
        (
            keys::IP_ENABLED,
            field_opt_bool(&body, "enabled").map(|value| value.to_string()),
        ),
        (
            keys::IP_TRUST_PROXY,
            field_opt_bool(&body, "trust_proxy").map(|value| value.to_string()),
        ),
        (
            keys::IP_PROXY_HOPS,
            field_opt_i64(&body, "proxy_hops").map(|value| value.to_string()),
        ),
        (
            keys::IP_AUTO_BAN,
            field_opt_bool(&body, "auto_ban").map(|value| value.to_string()),
        ),
        (
            keys::IP_FAIL_THRESHOLD,
            field_opt_i64(&body, "fail_threshold").map(|value| value.to_string()),
        ),
        (
            keys::IP_BAN_MINUTES,
            field_opt_i64(&body, "ban_minutes").map(|value| value.to_string()),
        ),
    ];

    for (key, value) in writes {
        if let Some(value) = value {
            if let Err(err) = state.settings.set(key, &value).await {
                return crate::http::error_response(err);
            }
        }
    }
    state.ip_guard.invalidate();

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::IP_RULE_CHANGE,
            Some(actor),
            "policy",
            body.clone(),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

// ============================================================
// 8.6 系统设置
// ============================================================

/// `GET /api/v1/admin/settings` — 分段返回，形状与 `PUT` 的入参一一对应。
pub async fn settings_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/settings");

    let site = match state.settings.site(state.config.base_url.as_deref()).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let upload = match state.settings.upload().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let mail = match state.settings.mail().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let default_avatar = match state.settings.default_avatar_url().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let background = match state.settings.background_public_url().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let appearance = match state.settings.appearance().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let background_urls = match state.settings.background_public_urls().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let session = match state.settings.session().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let hotlink = match state.settings.hotlink().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let moderation = match state.settings.moderation().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let music = match state.settings.music().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let plaza_albums = match state.repo.list_public_albums().await {
        Ok(rows) => rows,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let plaza_collections = match state.settings.plaza_collections().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let announcement_version = site.announcement.version();
    let announcement_reads = state
        .announcement_read_count(&announcement_version)
        .await
        .unwrap_or(0);
    let user_total = state.repo.count_users().await.unwrap_or(0);

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "site": {
            "name": site.name,
            "base_url": site.base_url,
            "plaza_public": site.plaza_public,
            "plaza_album_ids": site.plaza_album_ids,
            "plaza_collections": plaza_collections,
            "require_login_to_upload": site.require_login_to_upload,
            "register_enabled": site.register_enabled,
            // 注册开关可能被「容量触顶自动关闭」替管理员翻成关闭：把状态与口径
            // 一并给出，界面上要能解释「为什么开关自己关了」
            "register_auto_closed": state
                .settings
                .bool(pixora_core::stats::KEY_AUTO_CLOSED, false)
                .await
                .unwrap_or(false),
            "register_closed_reason": state.auto_close_reason().await.unwrap_or(None),
            // 阈值与当前占用率：手开之后那一格要说清「已超阈值」，
            // 光靠 register_closed_reason 拿不到（覆盖生效时它是 None）
            "register_auto_close": {
                "on": state.auto_close_policy().await.map(|p| p.on).unwrap_or(0),
                "off": state.auto_close_policy().await.map(|p| p.off).unwrap_or(0),
                "enabled": state.auto_close_policy().await.map(|p| p.enabled).unwrap_or(false),
                "percent": state.occupancy_percent_now().await,
            },
            "register_auto_close_override": state
                .settings
                .bool(pixora_core::stats::KEY_AUTO_CLOSE_OVERRIDE, false)
                .await
                .unwrap_or(false),
            "email_domains": site.email_domains.to_value(),
            "plaza_grid_columns": site.plaza_grid_columns,
            "plaza_auto_refresh_seconds": site.plaza_auto_refresh_seconds,
            "announcement": {
                "enabled": site.announcement.enabled,
                "title": site.announcement.title,
                "body": site.announcement.body,
                "starts_at": site.announcement.starts_at,
                "ends_at": site.announcement.ends_at,
                "active": site.announcement.is_active(chrono::Utc::now()),
                "reads": announcement_reads,
                "user_total": user_total,
            },
            "recycle_retention_days": state
                .settings
                .retention_days()
                .await
                .unwrap_or(0),
            "maintenance_enabled": site.maintenance.enabled,
            "maintenance_message": site.maintenance.message,
        },
        "upload": {
            "max_file_bytes": upload.max_file_bytes,
            "max_batch_size": upload.max_batch,
            "max_pixels": upload.max_pixels,
            "max_width": upload.max_width,
            "max_height": upload.max_height,
            "queue_depth": upload.queue_depth,
            "formats": upload.formats,
            // 可勾选的格式清单（含 apng 这类界面以前漏掉的），
            // 以及每一项的显示名（jpeg 归一到 jpg，界面上写 JPEG）
            "format_options": format_options(),
        },
        "session": {
            "idle_minutes": session.idle_minutes,
            "absolute_days": session.absolute_days,
            "remember_days": session.remember_days,
            "history_retention_days": session.history_retention_days,
        },
        "security": {
            "hotlink_enabled": hotlink.enabled,
            "hotlink_allow_empty": hotlink.allow_empty,
            "hotlink_domains": hotlink.domains.join(", "),
        },
        "moderation": {
            "enabled": moderation.enabled,
            "provider": moderation.provider,
            "threshold": moderation.threshold,
            "score_field": moderation.score_field,
            "endpoint": moderation.endpoint,
            "region": moderation.region,
            "app_id": moderation.app_id,
            "access_key_set": !moderation.access_key.trim().is_empty(),
            "secret_key_set": !moderation.secret_key.trim().is_empty(),
            "token_set": !moderation.token.trim().is_empty(),
            "user_enabled": moderation.user_enabled,
            "admin_enabled": moderation.admin_enabled,
            "ban_after": moderation.ban_after,
            "ban_minutes": moderation.ban_minutes,
            "notify_admin": moderation.notify_admin,
            "fail_open": moderation.fail_open,
        },
        "thumb": {
            "client": upload.thumb_client,
            "server_fallback": upload.thumb_server_fallback,
        },
        "mail": {
            "smtp_host": mail.smtp_host,
            "smtp_port": mail.smtp_port,
            "smtp_security": mail.smtp_security,
            "smtp_user": mail.smtp_user,
            "password_set": mail.password_set,
            "from_name": mail.from_name,
            "from_email": mail.from_email,
            "appeal_email": mail.appeal_email,
            "disposable_check": mail.disposable_check,
            "mx_check": mail.mx_check,
            // 「是否用第三方校验」与「密钥配没配」在实现上是同一件事
            // （只有配了密钥才谈得上启用），所以两个字段同值
            "third_party_check": mail.third_party_set,
            "third_party_set": mail.third_party_set,
        },
        "appearance": {
            "background": background,
            "backgrounds": background_urls,
            "background_opacity": appearance.opacity,
            "background_blur": appearance.blur,
            "background_mode": appearance.mode,
            "background_interval": appearance.interval,
            "default_avatar": default_avatar,
        },
        "plaza_albums": plaza_albums.iter().map(|row| serde_json::json!({
            "id": row.id,
            "name": row.name,
            "slug": row.slug,
            "owner_uid": row.owner_uid,
            "owner_email": row.owner_email,
            "image_count": row.image_count,
        })).collect::<Vec<_>>(),
        "music": {
            "enabled": music.enabled,
            "source": music.source,
            // api_url 保留给旧前端；新界面用有序的 api_urls 管理主备
            "api_url": music.api_urls.first().cloned().unwrap_or_default(),
            "api_urls": music.api_urls,
            "platform": music.platform,
            "playlist_id": music.playlist_id,
            "local_dir": music.local_dir,
            "local_dir_env": state.config.music_dir.as_ref().map(|path| path.display().to_string()),
            "autoplay": music.autoplay,
            "default_volume": music.default_volume,
        },
    })))
}

/// `PUT /api/v1/admin/settings` — 分段写，只写传入的键。
///
/// 三类特殊处理：
///   - **SMTP 密码**走 `set_secret`（加密入库），空串视为「不改」；
///   - **格式白名单**不在这里改（有单独的 `POST /admin/settings/formats`），
///     免得「改个站点名」顺手把格式列表写空；
///   - 写完**重建邮件器**：SMTP 是长连接配置，不重建就要重启进程才生效。
pub async fn settings_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings");

    let mut touched: Vec<String> = Vec::new();

    // 格式白名单有专门的端点：在这里静默忽略会让人以为「改过了」
    if body
        .get("upload")
        .and_then(|value| value.get("formats"))
        .is_some()
    {
        return bad_request("图片格式请走 POST /admin/settings/formats");
    }

    if let Some(site) = body.get("site").and_then(|value| value.as_object()) {
        let site = serde_json::Value::Object(site.clone());
        // 这个值要在下面单独处理：注册开关一动，就得联动自动关闭的覆盖标记
        let register_enabled_value = field_opt_bool(&site, "register_enabled");
        let plaza_album_ids = site
            .get("plaza_album_ids")
            .and_then(|value| value.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|item| item.as_i64())
                    .filter(|id| *id > 0)
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            });
        let plaza_collections = site
            .get("plaza_collections")
            .and_then(|value| value.as_object())
            .map(|object| {
                let mut clean = serde_json::Map::new();
                for (name, item) in object.iter().take(50) {
                    let name = name.trim();
                    if name.is_empty() || name.chars().count() > 64 {
                        continue;
                    }
                    let ids: Vec<i64> = match item {
                        serde_json::Value::Array(array) => array
                            .iter()
                            .filter_map(|id| id.as_i64())
                            .filter(|id| *id > 0)
                            .take(200)
                            .collect(),
                        serde_json::Value::String(text) => {
                            pixora_core::settings::parse_id_list(Some(text))
                        }
                        _ => Vec::new(),
                    };
                    if !ids.is_empty() {
                        clean.insert(name.to_string(), serde_json::json!(ids));
                    }
                }
                serde_json::Value::Object(clean).to_string()
            });
        let announcement = site
            .get("announcement")
            .and_then(|value| value.as_object())
            .map(|object| serde_json::Value::Object(object.clone()));
        // 时间字段要区分「没传」「传了空串（清除定时）」「传了时间」三种情况：
        // 空串必须真的写回空，否则管理员清不掉旧的定时窗口。
        let announcement_field = |key: &str| {
            announcement.as_ref().and_then(|value| value.get(key)).map(|value| {
                let raw = value.as_str().unwrap_or("").trim().to_string();
                if raw.is_empty() {
                    None
                } else {
                    Some(raw)
                }
            })
        };
        let announcement_starts_field = announcement_field("starts_at");
        let announcement_ends_field = announcement_field("ends_at");
        let parse_field_time = |raw: Option<&str>, key: &str| {
            raw.map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|at| at.with_timezone(&chrono::Utc))
                    .map_err(|_| format!("公告的 {key} 不是合法时间"))
            })
            .transpose()
        };
        let announcement_starts = match parse_field_time(
            announcement_starts_field.as_ref().and_then(|value| value.as_deref()),
            "starts_at",
        ) {
            Ok(value) => value,
            Err(message) => return bad_request(message),
        };
        let announcement_ends = match parse_field_time(
            announcement_ends_field.as_ref().and_then(|value| value.as_deref()),
            "ends_at",
        ) {
            Ok(value) => value,
            Err(message) => return bad_request(message),
        };
        if let (Some(start), Some(end)) = (announcement_starts, announcement_ends) {
            if end <= start {
                return bad_request("公告结束时间必须晚于开始时间");
            }
        }
        let writes: [(&str, Option<String>); 18] = [
            (keys::SITE_NAME, field_opt_str(&site, "name")),
            (keys::SITE_BASE_URL, field_opt_str(&site, "base_url")),
            (
                keys::PLAZA_PUBLIC,
                field_opt_bool(&site, "plaza_public").map(|value| value.to_string()),
            ),
            (keys::PLAZA_ALBUM_IDS, plaza_album_ids),
            (keys::PLAZA_COLLECTIONS, plaza_collections),
            (
                keys::UPLOAD_REQUIRE_LOGIN,
                field_opt_bool(&site, "require_login_to_upload").map(|value| value.to_string()),
            ),
            (
                keys::REGISTER_ENABLED,
                field_opt_bool(&site, "register_enabled").map(|value| value.to_string()),
            ),
            (
                keys::REGISTER_EMAIL_DOMAINS,
                // 不能用 field_opt_str：它把空串当「这次没给」，而清空白名单
                // 恰恰就是要写一个空串。先归一再落库，判定时就不必猜格式
                site.get("email_domains")
                    .and_then(|value| value.as_str())
                    .map(|value| {
                        pixora_core::settings::RegisterDomains::parse(Some(value)).to_value()
                    }),
            ),
            (
                keys::PLAZA_GRID_COLUMNS,
                field_opt_i64(&site, "plaza_grid_columns").map(|value| {
                    value
                        .clamp(
                            settings_defaults::GRID_COLUMNS_MIN,
                            settings_defaults::GRID_COLUMNS_MAX,
                        )
                        .to_string()
                }),
            ),
            (
                keys::PLAZA_AUTO_REFRESH_SECONDS,
                field_opt_i64(&site, "plaza_auto_refresh_seconds")
                    .map(|value| normalize_auto_refresh_seconds(value).to_string()),
            ),
            (
                keys::RECYCLE_RETENTION_DAYS,
                field_opt_i64(&site, "recycle_retention_days").map(|value| {
                    value
                        .clamp(0, settings_defaults::RECYCLE_RETENTION_MAX)
                        .to_string()
                }),
            ),
            (
                keys::ANNOUNCEMENT_ENABLED,
                announcement
                    .as_ref()
                    .and_then(|value| field_opt_bool(value, "enabled"))
                    .map(|value| value.to_string()),
            ),
            (
                keys::ANNOUNCEMENT_TITLE,
                announcement
                    .as_ref()
                    .and_then(|value| value.get("title"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.trim().chars().take(120).collect()),
            ),
            (
                keys::ANNOUNCEMENT_BODY,
                announcement
                    .as_ref()
                    .and_then(|value| value.get("body"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.chars().take(5000).collect()),
            ),
            (
                keys::ANNOUNCEMENT_STARTS_AT,
                announcement_starts_field.map(|value| value.unwrap_or_default()),
            ),
            (
                keys::ANNOUNCEMENT_ENDS_AT,
                announcement_ends_field.map(|value| value.unwrap_or_default()),
            ),
            (
                keys::SITE_MAINTENANCE_ENABLED,
                field_opt_bool(&site, "maintenance_enabled").map(|value| value.to_string()),
            ),
            (
                keys::SITE_MAINTENANCE_MESSAGE,
                site.get("maintenance_message")
                    .and_then(|value| value.as_str())
                    .map(|value| value.trim().chars().take(200).collect()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
        // 注册开关与「容量触顶自动关闭」是一对：管理员手动打开时，把自动关闭的
        // 覆盖标记立起来——不然占用率还在阈值之上，开关刚打开就被自动关闭按回去，
        // 管理员会以为开关坏了。管理员显式关掉时把标记清掉，让策略重新接管
        if let Some(_enabled) = register_enabled_value {
            // 管理员一动手就由他接管：
            //   - 覆盖标记立起来：抑制自动关闭（不然刚打开就被按回去，看起来像
            //     开关坏了；关掉时不立这个标记，容量恢复又会被自动打开）
            //   - 闩锁与「是我们关的」一起清掉：那个闩锁的含义是「此刻因容量而关」，
            //     开关已经在管理员手里，这个说法不再成立
            for key in [
                pixora_core::stats::KEY_AUTO_CLOSE_OVERRIDE,
                pixora_core::stats::KEY_AUTO_CLOSED,
                pixora_core::stats::KEY_AUTO_CLOSE_OWNED,
            ] {
                let value = key == pixora_core::stats::KEY_AUTO_CLOSE_OVERRIDE;
                if let Err(err) = state.settings.set_bool(key, value).await {
                    return crate::http::error_response(err);
                }
                touched.push(key.to_string());
            }
        }
    }

    if let Some(music) = body.get("music").and_then(|value| value.as_object()) {
        let music = serde_json::Value::Object(music.clone());
        let enabled = field_opt_bool(&music, "enabled").unwrap_or(false);
        let source = field_opt_str(&music, "source")
            .unwrap_or_else(|| "api".into())
            .to_ascii_lowercase();
        let source = if source == "local" { "local" } else { "api" };
        let platform = field_opt_str(&music, "platform")
            .unwrap_or_else(|| "netease".into())
            .to_ascii_lowercase();
        if !pixora_core::music::PLATFORMS.contains(&platform.as_str()) {
            return bad_request("不支持的音乐平台");
        }
        let api_urls: Vec<String> = match music.get("api_urls") {
            Some(serde_json::Value::Array(items)) => {
                let mut urls = Vec::with_capacity(items.len());
                for item in items {
                    let Some(text) = item.as_str() else {
                        return bad_request("music.api_urls 必须是字符串数组");
                    };
                    let text = text.trim();
                    if !text.is_empty() {
                        urls.push(text.to_string());
                    }
                }
                urls
            }
            // 兼容旧客户端：仍可只传一个 api_url
            Some(serde_json::Value::Null) | None => {
                let legacy = field_opt_str(&music, "api_url")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if legacy.is_empty() {
                    Vec::new()
                } else {
                    vec![legacy]
                }
            }
            Some(_) => return bad_request("music.api_urls 必须是字符串数组"),
        };
        if api_urls.len() > 10 {
            return bad_request("音乐 API 最多 10 个");
        }
        for url in &api_urls {
            if url.chars().count() > 1000 {
                return bad_request("音乐 API 地址太长了");
            }
            match reqwest::Url::parse(url) {
                Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                _ => return bad_request("音乐 API 地址必须是以 http(s) 开头的完整地址"),
            }
        }
        let api_url = api_urls.first().cloned().unwrap_or_default();
        let api_urls_json =
            serde_json::to_string(&api_urls).unwrap_or_else(|_| "[]".to_string());
        let playlist_id = field_opt_str(&music, "playlist_id")
            .unwrap_or_default()
            .trim()
            .to_string();
        let local_dir = field_opt_str(&music, "local_dir")
            .unwrap_or_default()
            .trim()
            .to_string();
        if enabled && source == "api" && (api_urls.is_empty() || playlist_id.is_empty()) {
            return bad_request("启用 API 音乐时，至少填一个 API 地址和歌单 ID");
        }
        if enabled && source == "local" && local_dir.is_empty() && state.config.music_dir.is_none()
        {
            return bad_request("启用本地音乐时，要填音乐目录或设置 PIXORA_MUSIC_DIR");
        }
        if playlist_id.chars().count() > 200 {
            return bad_request("歌单 ID 太长了");
        }
        if local_dir.chars().count() > 1000 {
            return bad_request("音乐目录路径太长了");
        }
        let default_volume = match music.get("default_volume") {
            Some(serde_json::Value::Number(value)) => {
                let Some(parsed) = value.as_f64() else {
                    return bad_request("music.default_volume 必须是 0 到 1 之间的数字");
                };
                Some(parsed.clamp(0.0, 1.0))
            }
            Some(serde_json::Value::Null) | None => None,
            Some(_) => return bad_request("music.default_volume 必须是 0 到 1 之间的数字"),
        };
        let writes: [(&str, Option<String>); 9] = [
            (
                keys::MUSIC_ENABLED,
                field_opt_bool(&music, "enabled").map(|value| value.to_string()),
            ),
            (
                keys::MUSIC_AUTOPLAY,
                field_opt_bool(&music, "autoplay").map(|value| value.to_string()),
            ),
            (
                keys::MUSIC_DEFAULT_VOLUME,
                default_volume.map(|value| format!("{value:.2}")),
            ),
            (keys::MUSIC_SOURCE, Some(source.to_string())),
            (keys::MUSIC_API_URLS, Some(api_urls_json)),
            (keys::MUSIC_API_URL, Some(api_url)),
            (keys::MUSIC_PLATFORM, Some(platform)),
            (keys::MUSIC_PLAYLIST_ID, Some(playlist_id)),
            (keys::MUSIC_LOCAL_DIR, Some(local_dir)),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(upload) = body.get("upload").and_then(|value| value.as_object()) {
        let upload = serde_json::Value::Object(upload.clone());
        let writes: [(&str, Option<String>); 6] = [
            (
                keys::UPLOAD_MAX_FILE,
                field_opt_i64(&upload, "max_file_bytes").map(|value| value.to_string()),
            ),
            (
                keys::UPLOAD_MAX_BATCH,
                field_opt_i64(&upload, "max_batch_size").map(|value| value.to_string()),
            ),
            (
                keys::UPLOAD_MAX_PIXELS,
                field_opt_i64(&upload, "max_pixels").map(|value| value.to_string()),
            ),
            (
                keys::UPLOAD_MAX_WIDTH,
                field_opt_i64(&upload, "max_width").map(|value| value.max(0).to_string()),
            ),
            (
                keys::UPLOAD_MAX_HEIGHT,
                field_opt_i64(&upload, "max_height").map(|value| value.max(0).to_string()),
            ),
            (
                keys::UPLOAD_QUEUE_DEPTH,
                field_opt_i64(&upload, "queue_depth").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(session) = body.get("session").and_then(|value| value.as_object()) {
        let session = serde_json::Value::Object(session.clone());
        let writes: [(&str, Option<String>); 4] = [
            (
                keys::SESSION_IDLE_MINUTES,
                field_opt_i64(&session, "idle_minutes").map(|value| {
                    value
                        .clamp(0, settings_defaults::SESSION_MINUTES_MAX)
                        .to_string()
                }),
            ),
            (
                keys::SESSION_ABSOLUTE_DAYS,
                field_opt_i64(&session, "absolute_days").map(|value| {
                    value
                        .clamp(0, settings_defaults::SESSION_DAYS_MAX)
                        .to_string()
                }),
            ),
            (
                keys::SESSION_REMEMBER_DAYS,
                field_opt_i64(&session, "remember_days").map(|value| {
                    value
                        .clamp(1, settings_defaults::SESSION_DAYS_MAX)
                        .to_string()
                }),
            ),
            (
                keys::SESSION_HISTORY_RETENTION_DAYS,
                field_opt_i64(&session, "history_retention_days").map(|value| {
                    value
                        .clamp(0, settings_defaults::SESSION_DAYS_MAX)
                        .to_string()
                }),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(security) = body.get("security").and_then(|value| value.as_object()) {
        let security = serde_json::Value::Object(security.clone());
        let writes: [(&str, Option<String>); 3] = [
            (
                keys::HOTLINK_ENABLED,
                field_opt_bool(&security, "hotlink_enabled").map(|value| value.to_string()),
            ),
            (
                keys::HOTLINK_ALLOW_EMPTY,
                field_opt_bool(&security, "hotlink_allow_empty").map(|value| value.to_string()),
            ),
            (
                keys::HOTLINK_DOMAINS,
                field_opt_str(&security, "hotlink_domains"),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(moderation) = body.get("moderation").and_then(|value| value.as_object()) {
        let moderation = serde_json::Value::Object(moderation.clone());
        let writes: [(&str, Option<String>); 9] = [
            (
                keys::MODERATION_ENABLED,
                field_opt_bool(&moderation, "enabled").map(|value| value.to_string()),
            ),
            (
                keys::MODERATION_PROVIDER,
                field_opt_str(&moderation, "provider"),
            ),
            (
                keys::MODERATION_THRESHOLD,
                field_opt_i64(&moderation, "threshold")
                    .map(|value| value.clamp(1, 100).to_string()),
            ),
            (
                keys::MODERATION_SCORE_FIELD,
                field_opt_str(&moderation, "score_field"),
            ),
            (
                keys::MODERATION_ENDPOINT,
                field_opt_str(&moderation, "endpoint"),
            ),
            (
                keys::MODERATION_REGION,
                field_opt_str(&moderation, "region"),
            ),
            (
                keys::MODERATION_APP_ID,
                field_opt_str(&moderation, "app_id"),
            ),
            (
                keys::MODERATION_USER_ENABLED,
                field_opt_bool(&moderation, "user_enabled").map(|value| value.to_string()),
            ),
            (
                keys::MODERATION_ADMIN_ENABLED,
                field_opt_bool(&moderation, "admin_enabled").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
        let extras: [(&str, Option<String>); 4] = [
            (
                keys::MODERATION_BAN_AFTER,
                field_opt_i64(&moderation, "ban_after").map(|value| value.max(0).to_string()),
            ),
            (
                keys::MODERATION_BAN_MINUTES,
                field_opt_i64(&moderation, "ban_minutes").map(|value| value.max(0).to_string()),
            ),
            (
                keys::MODERATION_NOTIFY,
                field_opt_bool(&moderation, "notify_admin").map(|value| value.to_string()),
            ),
            (
                keys::MODERATION_FAIL_OPEN,
                field_opt_bool(&moderation, "fail_open").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &extras, &mut touched).await {
            return resp;
        }
        // 三家云服务的凭据都走加密存储；空串表示「不改」，
        // 界面上那几个框永远是空的，只有填了才写
        for (field, key) in [
            ("access_key", keys::MODERATION_ACCESS_KEY),
            ("secret_key", keys::MODERATION_SECRET_KEY),
            ("token", keys::MODERATION_TOKEN),
        ] {
            if let Some(secret) = field_opt_str(&moderation, field) {
                if let Err(err) = state.settings.set_secret(key, &secret).await {
                    return crate::http::error_response(err);
                }
                touched.push(key.to_string());
            }
        }
    }

    if let Some(thumb) = body.get("thumb").and_then(|value| value.as_object()) {
        let thumb = serde_json::Value::Object(thumb.clone());
        let writes: [(&str, Option<String>); 2] = [
            (
                keys::THUMB_CLIENT,
                field_opt_bool(&thumb, "client").map(|value| value.to_string()),
            ),
            (
                keys::THUMB_SERVER_FALLBACK,
                field_opt_bool(&thumb, "server_fallback").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(mail) = body.get("mail").and_then(|value| value.as_object()) {
        let mail = serde_json::Value::Object(mail.clone());
        let writes: [(&str, Option<String>); 8] = [
            (keys::MAIL_SMTP_HOST, field_opt_str(&mail, "smtp_host")),
            (
                keys::MAIL_SMTP_PORT,
                field_opt_i64(&mail, "smtp_port").map(|value| value.to_string()),
            ),
            (
                keys::MAIL_SMTP_SECURITY,
                field_opt_str(&mail, "smtp_security"),
            ),
            (keys::MAIL_SMTP_USER, field_opt_str(&mail, "smtp_user")),
            (keys::MAIL_FROM_NAME, field_opt_str(&mail, "from_name")),
            (keys::MAIL_FROM_EMAIL, field_opt_str(&mail, "from_email")),
            (
                keys::MAIL_APPEAL_EMAIL,
                field_opt_str(&mail, "appeal_email"),
            ),
            (
                keys::ANTISPAM_DISPOSABLE,
                field_opt_bool(&mail, "disposable_check").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
        if let Some(value) = field_opt_bool(&mail, "mx_check") {
            if let Err(err) = state.settings.set_bool(keys::ANTISPAM_MX, value).await {
                return crate::http::error_response(err);
            }
            touched.push(keys::ANTISPAM_MX.to_string());
        }
        // 第三方校验密钥：给了才写，空串当「不改」
        if let Some(secret) = field_opt_str(&mail, "third_party_key") {
            if let Err(err) = state
                .settings
                .set_secret(keys::ANTISPAM_THIRD_PARTY, &secret)
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::ANTISPAM_THIRD_PARTY.to_string());
        }
        // SMTP 密码同样只写「给了」的情况；空串是界面上的「保持原密码」
        if let Some(password) = field_opt_str(&mail, "password") {
            if let Err(err) = state
                .settings
                .set_secret(keys::MAIL_SMTP_PASSWORD, &password)
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::MAIL_SMTP_PASSWORD.to_string());
        }
    }

    if let Some(appearance) = body.get("appearance").and_then(|value| value.as_object()) {
        let appearance = serde_json::Value::Object(appearance.clone());

        if let Some(value) = appearance
            .get("background_opacity")
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|raw| raw.trim().parse().ok()))
            })
        {
            let value = value.clamp(0.0, 1.0);
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND_OPACITY, &value.to_string())
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::APPEARANCE_BACKGROUND_OPACITY.to_string());
        }
        if let Some(value) = field_opt_i64(&appearance, "background_blur") {
            let value = value.clamp(0, 40);
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND_BLUR, &value.to_string())
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::APPEARANCE_BACKGROUND_BLUR.to_string());
        }
        if let Some(value) = field_opt_str(&appearance, "background_mode") {
            let value = match value.trim().to_ascii_lowercase().as_str() {
                "slideshow" | "random" => value.trim().to_ascii_lowercase(),
                _ => "single".to_string(),
            };
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND_MODE, &value)
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::APPEARANCE_BACKGROUND_MODE.to_string());
        }
        if let Some(value) = field_opt_i64(&appearance, "background_interval") {
            let value = value.clamp(
                settings_defaults::BACKGROUND_INTERVAL_MIN,
                settings_defaults::BACKGROUND_INTERVAL_MAX,
            );
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND_INTERVAL, &value.to_string())
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::APPEARANCE_BACKGROUND_INTERVAL.to_string());
        }

        if super::field_present(&appearance, "backgrounds") {
            let raw_list = appearance
                .get("backgrounds")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            let mut entries: Vec<String> = Vec::new();
            for item in raw_list {
                if let Some(entry) = item.as_str().and_then(normalize_background_entry) {
                    if !entries.contains(&entry) {
                        entries.push(entry);
                    }
                }
            }
            entries.truncate(60);

            let old_entries = state
                .settings
                .appearance()
                .await
                .map(|value| value.backgrounds)
                .unwrap_or_default();
            let new_assets: std::collections::HashSet<String> =
                entries.iter().filter_map(|entry| background_asset_key(entry)).collect();
            for entry in &old_entries {
                if let Some(key) = background_asset_key(entry) {
                    if !new_assets.contains(&key) {
                        if let Err(err) = state.repo.delete_asset(&key).await {
                            tracing::warn!(error = %err, key, "删除旧背景图资产失败");
                        }
                    }
                }
            }

            let encoded = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into());
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUNDS, &encoded)
                .await
            {
                return crate::http::error_response(err);
            }
            // 背景列表成为唯一来源；旧版单图字段只在列表为空时作回退。
            // 列表里不再引用旧上传资产时，把旧资产一起清掉。
            if !entries.iter().any(|entry| entry == keys::BACKGROUND_ASSET_MARKER) {
                if let Err(err) = state
                    .settings
                    .set(keys::APPEARANCE_BACKGROUND, "")
                    .await
                {
                    return crate::http::error_response(err);
                }
                if let Err(err) = state
                    .repo
                    .delete_asset(pixora_core::settings::keys::BACKGROUND_ASSET_KEY)
                    .await
                {
                    tracing::warn!(error = %err, "删除旧版背景图资产失败");
                }
            }
            touched.push(keys::APPEARANCE_BACKGROUNDS.to_string());
        } else if super::field_present(&appearance, "background") {
            // 兼容旧前端/脚本：只给一张图。提交值如果已经是当前上传图的公开
            // 地址，就按「继续用这张」处理，不能把资产删掉。
            let submitted = field_opt_str(&appearance, "background");
            let current = state
                .settings
                .background_public_url()
                .await
                .ok()
                .flatten();
            let keeps_asset = submitted.as_deref() == Some(keys::BACKGROUND_ASSET_MARKER)
                || (submitted.is_some() && submitted == current);
            let stored = if keeps_asset {
                keys::BACKGROUND_ASSET_MARKER.to_string()
            } else {
                submitted.clone().unwrap_or_default()
            };
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND, &stored)
                .await
            {
                return crate::http::error_response(err);
            }
            if !keeps_asset {
                if let Err(err) = state
                    .repo
                    .delete_asset(pixora_core::settings::keys::BACKGROUND_ASSET_KEY)
                    .await
                {
                    tracing::warn!(error = %err, "删除背景图资产失败");
                }
            }
            touched.push(keys::APPEARANCE_BACKGROUND.to_string());
        }

        if super::field_present(&appearance, "default_avatar") {
            // 与背景图同一条规矩：显式 null 表示清掉；提交当前公开地址时
            // 视为「继续用上传的这张」，不能把资产删掉。
            let submitted = field_opt_str(&appearance, "default_avatar");
            let current = state.settings.default_avatar_url().await.ok().flatten();
            let keeps_asset = submitted.as_deref() == Some(pixora_core::settings::keys::AVATAR_ASSET_MARKER)
                || (submitted.is_some() && submitted == current);
            let stored = if keeps_asset {
                pixora_core::settings::keys::AVATAR_ASSET_MARKER.to_string()
            } else {
                submitted.clone().unwrap_or_default()
            };
            if let Err(err) = state
                .settings
                .set(pixora_core::settings::keys::SITE_DEFAULT_AVATAR, &stored)
                .await
            {
                return crate::http::error_response(err);
            }
            if !keeps_asset {
                if let Err(err) = state
                    .repo
                    .delete_asset(pixora_core::settings::keys::AVATAR_ASSET_KEY)
                    .await
                {
                    tracing::warn!(error = %err, "删除默认头像资产失败");
                }
            }
            touched.push(pixora_core::settings::keys::SITE_DEFAULT_AVATAR.to_string());
        }
    }

    // SMTP 改了要重建邮件器，否则要重启进程才生效
    state.reload_mailer().await;

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            "settings",
            serde_json::json!({ "keys": touched }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "updated": touched,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// 批量写设置，记录改过的键名。
async fn write_settings(
    state: &Arc<AppState>,
    writes: &[(&str, Option<String>)],
    touched: &mut Vec<String>,
) -> Result<(), Response> {
    for (key, value) in writes {
        if let Some(value) = value {
            state
                .settings
                .set(key, value)
                .await
                .map_err(crate::http::error_response)?;
            touched.push(key.to_string());
        }
    }
    Ok(())
}

/// `GET /api/v1/admin/mail/templates` — 邮件模板列表（内置 + 改过的）。
pub async fn mail_templates_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/mail/templates");
    finish(state.mail_template_list().await)
}

/// `PUT /api/v1/admin/mail/templates/{name}` ← `{ "subject": "…", "body": "…" }`。
pub async fn mail_template_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/mail/templates/{name}");
    let subject = match field_str(&body, "subject") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let body_text = match field_str(&body, "body") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    finish(
        state
            .mail_template_save(
                auth.user.id,
                &name,
                &subject,
                &body_text,
                rectx.ip.as_deref(),
            )
            .await,
    )
}

/// `DELETE /api/v1/admin/mail/templates/{name}` — 恢复内置文案。
pub async fn mail_template_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(name): Path<String>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/mail/templates/{name}");
    finish(
        state
            .mail_template_reset(auth.user.id, &name, rectx.ip.as_deref())
            .await,
    )
}

/// `POST /api/v1/admin/settings/mail/test` — 发一封测试信。
///
/// 用它诊断「配置到底通不通」：SMTP 没配就说清楚没配，而不是抛一个
/// 「连接失败」让管理员去猜。
pub async fn mail_test(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings/mail/test");

    let to = field_opt_str(&body, "to").unwrap_or_else(|| auth.user.email.clone());
    // 先看这个地址收不收得到信：域名都没了的话，测出来的「失败」跟 SMTP 配置
    // 没关系，会把管理员引到错的方向去查
    if let Err(err) = state.ensure_deliverable(&to).await {
        return crate::http::error_response(AppError::bad_request(format!(
            "测试收件地址不可用：{}",
            err.message()
        )));
    }
    let Some(mailer) = state.mailer() else {
        // 说清楚是缺哪块，而不是笼统一句「没配置完整」
        let reason = state
            .mailer_problem()
            .unwrap_or_else(|| "SMTP 服务器与发信邮箱都要填".to_string());
        return crate::http::error_response(AppError::bad_request(format!(
            "还没法发信：{reason}。在下面改好、点「保存设置」，再测一次"
        )));
    };

    // 这次实际用的是哪套配置：端口或账号填错时，对着这行一眼就能看出来
    let config = mailer.config();
    let used = format!(
        "{}:{}（{}），发信账号 {}",
        config.host,
        config.port,
        config.security.as_str(),
        if config.username.trim().is_empty() {
            "（空）"
        } else {
            config.username.as_str()
        }
    );

    match mailer.send_test(&to).await {
        Ok(()) => {
            let actor = auth.user.id;
            state
                .audit_detail(
                    actions::SETTINGS_CHANGE,
                    Some(actor),
                    "mail_test",
                    serde_json::json!({ "to": to }),
                    rectx.ip.as_deref(),
                )
                .await;
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "ok": true,
                "message": format!("测试邮件已发出（{to}），请查收。本次用的配置：{used}"),
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        // 发不出去基本是配置问题：回 400 而不是 500，并把用的配置带上
        Err(err) => crate::http::error_response(AppError::bad_request(format!(
            "测试发信失败：{err}。本次用的配置：{used}"
        ))),
    }
}

/// `POST /api/v1/admin/settings/formats` — 上传格式白名单。
///
/// 每个扩展名都要能在 `media::FORMATS` 里查到：白名单里写一个服务端
/// 根本不认识的格式，等于给用户开了个「传上来必失败」的坑。
pub async fn formats_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings/formats");

    let Some(list) = super::field_string_list(&body, "formats") else {
        return bad_request("缺少参数 formats");
    };
    if list.is_empty() {
        return bad_request("至少要启用一种上传格式");
    }
    let mut normalized: Vec<String> = Vec::new();
    for ext in &list {
        let ext = pixora_core::media::normalize_ext(ext);
        if pixora_core::media::rule_for_ext(&ext).is_none() {
            return bad_request(format!("不支持的图片格式：{ext}"));
        }
        if !normalized.contains(&ext) {
            normalized.push(ext);
        }
    }

    let value = normalized.join(",");
    if let Err(err) = state.settings.set(keys::UPLOAD_FORMATS, &value).await {
        return crate::http::error_response(err);
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            "upload.formats",
            serde_json::json!({ "formats": normalized }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "formats": normalized,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `POST /api/v1/admin/storage/gc` — 立即回收无引用的物理文件。
///
/// 彻底删除之后文件要等后台扫描才回收，存储面板上会看到「待回收」那一行迟迟不清。
/// 这个入口就是「现在就跑一轮」，免得管理员盯着一个数字等下一个扫描周期。
/// 回收本身仍然只走 `pixora_queue::gc`，物理文件依然只有那一个地方会删。
pub async fn storage_gc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/storage/gc");

    let config = pixora_queue::QueueConfig::default();
    let retention = state.settings.retention_days().await.unwrap_or(0);
    let mut removed = 0u64;
    let mut pending = 0i64;
    // 一轮最多处理一个批次，积压多的时候要跑几轮。上限给在这里：
    // 管理员点一下不该把请求挂住太久，剩下的下一轮后台扫描自然会收
    for _ in 0..5 {
        match pixora_queue::gc::collect_blobs(
            &state,
            config.gc_grace_minutes,
            config.gc_batch,
            retention,
        )
        .await
        {
            Ok(outcome) => {
                removed += outcome.removed;
                pending = outcome.pending_grace;
                if outcome.removed == 0 {
                    break;
                }
            }
            Err(err) => return crate::http::error_response(err),
        }
    }

    state
        .audit_detail(
            actions::STORAGE_CHANGE,
            Some(auth.user.id),
            "storage:gc",
            serde_json::json!({ "removed": removed, "pending": pending }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "removed": removed,
        "pending": pending,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

// ============================================================
// 8.6.1 背景图上传 / 免登录切换用户
// ============================================================

/// 站点资产（背景图 / 默认头像）上传的体积上限。
///
/// 界面会先压缩再上传，正常只有几百 KB；这个上限是防止有人绕过界面
/// 直接 POST 一张 50MB 的原图进库。
const BACKGROUND_MAX_BYTES: usize = 4 * 1024 * 1024;
const AVATAR_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 一个「站点资产」在库里的落点：字节存哪、设置里写哪个键、报错时叫它什么。
struct SiteAsset<'a> {
    /// `storage_objects` 里的键
    asset_key: &'a str,
    /// 设置键，上传成功后写成本资产的标志值
    settings_key: &'a str,
    /// 设置里表示「用上传的那张」的值
    marker: &'a str,
    /// 报错与审计里用的名字（背景图 / 默认头像）
    label: &'a str,
    max_bytes: usize,
}

/// `POST /api/v1/admin/settings/background` — 上传站点背景图。
pub async fn background_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    multipart: axum::extract::Multipart,
) -> Response {
    let asset = SiteAsset {
        asset_key: pixora_core::settings::keys::BACKGROUND_ASSET_KEY,
        settings_key: pixora_core::settings::keys::APPEARANCE_BACKGROUND,
        marker: pixora_core::settings::keys::BACKGROUND_ASSET_MARKER,
        label: "背景图",
        max_bytes: BACKGROUND_MAX_BYTES,
    };
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings/background");
    let stored = match upload_site_asset(&state, rectx, auth, multipart, &asset).await {
        Ok(stored) => stored,
        Err(response) => return response,
    };
    let url = state
        .settings
        .background_public_url()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    site_asset_response(&headers, url, &stored, &asset)
}

/// `POST /api/v1/admin/settings/avatar` — 上传站点默认头像。
///
/// 与背景图同一条路：字节进库、设置里写 `asset` 标记，`/site` 把它翻译成
/// 公开地址。没设过头像的账号（含新注册的）都用它。
pub async fn avatar_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    multipart: axum::extract::Multipart,
) -> Response {
    let asset = SiteAsset {
        asset_key: pixora_core::settings::keys::AVATAR_ASSET_KEY,
        settings_key: pixora_core::settings::keys::SITE_DEFAULT_AVATAR,
        marker: pixora_core::settings::keys::AVATAR_ASSET_MARKER,
        label: "默认头像",
        max_bytes: AVATAR_MAX_BYTES,
    };
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings/avatar");
    let stored = match upload_site_asset(&state, rectx, auth, multipart, &asset).await {
        Ok(stored) => stored,
        Err(response) => return response,
    };
    let url = state
        .settings
        .default_avatar_url()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    site_asset_response(&headers, url, &stored, &asset)
}

/// `POST /api/v1/admin/settings/backgrounds` — 一次上传多张背景图。
///
/// 每张独立存成 `config:background:<id>`；返回的公开地址由前端放进
/// `appearance.backgrounds`，保存设置时才真正启用。这样上传多张、删掉一张、
/// 调整顺序都不必每次都重写设置。
pub async fn backgrounds_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/settings/backgrounds");
    let mut files: Vec<Vec<u8>> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                return bad_request(format!("表单解析失败：{err}"));
            }
        };
        let name = field.name().unwrap_or_default().to_ascii_lowercase();
        if !matches!(name.as_str(), "file" | "image" | "background") {
            continue;
        }
        match field.bytes().await {
            Ok(chunk) => {
                if chunk.len() > BACKGROUND_MAX_BYTES {
                    return bad_request(format!(
                        "单张背景图不能超过 {} MiB",
                        BACKGROUND_MAX_BYTES / 1024 / 1024
                    ));
                }
                files.push(chunk.to_vec());
            }
            Err(err) => return bad_request(format!("读取背景图失败：{err}")),
        }
    }
    if files.is_empty() {
        return bad_request("没有收到图片文件（字段名用 file）");
    }
    if files.len() > 12 {
        return bad_request("一次最多上传 12 张背景图");
    }

    let mut urls: Vec<String> = Vec::new();
    for (index, bytes) in files.into_iter().enumerate() {
        let Some(sniffed) = pixora_core::media::sniff(&bytes) else {
            return bad_request("背景图格式无法识别");
        };
        if !sniffed.mime.starts_with("image/") {
            return bad_request("背景图要是图片");
        }
        let stored = if sniffed.mime == "image/svg+xml" {
            pixora_core::media::sanitize_svg(&bytes)
        } else {
            bytes
        };
        let id = format!(
            "{}-{index}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let key = format!("{}{id}", keys::BACKGROUND_ASSET_PREFIX);
        if let Err(err) = state.repo.put_asset(&key, &stored).await {
            return crate::http::error_response(err.into());
        }
        let version = state
            .repo
            .asset_version(&key)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let version: String = version.chars().filter(|ch| ch.is_ascii_digit()).collect();
        urls.push(format!(
            "{}{}?v={}",
            keys::BACKGROUND_PUBLIC_PREFIX,
            id,
            version
        ));
        state
            .audit_detail(
                actions::SETTINGS_CHANGE,
                Some(auth.user.id),
                &key,
                serde_json::json!({ "bytes": stored.len(), "mime": sniffed.mime }),
                rectx.ip.as_deref(),
            )
            .await;
    }

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "urls": urls,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// 收图 → 净化 → 进库 → 写设置标记。返回存下来的字节；出错时返回打包好的响应。
///
/// 存进 `storage_objects` 而不是文件系统：这些属于配置，备份数据库时该跟着走，
/// 也不该混在用户内容目录里。
async fn upload_site_asset(
    state: &Arc<AppState>,
    rectx: crate::http::RequestContext,
    auth: crate::http::Authenticated,
    mut multipart: axum::extract::Multipart,
    asset: &SiteAsset<'_>,
) -> Result<Vec<u8>, Response> {
    let mut bytes: Option<Vec<u8>> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => {
                return Err(crate::http::error_response(AppError::bad_request(format!(
                    "表单解析失败：{err}"
                ))))
            }
        };
        let name = field.name().unwrap_or_default().to_ascii_lowercase();
        if !matches!(name.as_str(), "file" | "image" | "background" | "avatar") {
            continue;
        }
        match field.bytes().await {
            Ok(chunk) => {
                if chunk.len() > asset.max_bytes {
                    return Err(crate::http::error_response(AppError::bad_request(format!(
                        "{}不能超过 {} MiB；界面会自动压缩，这个提示说明你在直接调接口",
                        asset.label,
                        asset.max_bytes / 1024 / 1024
                    ))));
                }
                bytes = Some(chunk.to_vec());
            }
            Err(err) => {
                return Err(crate::http::error_response(AppError::bad_request(format!(
                    "读取{}失败：{err}",
                    asset.label
                ))))
            }
        }
    }

    let Some(bytes) = bytes else {
        return Err(crate::http::error_response(AppError::bad_request(
            "没有收到图片文件（字段名用 file）",
        )));
    };
    let Some(sniffed) = pixora_core::media::sniff(&bytes) else {
        return Err(crate::http::error_response(AppError::bad_request(format!(
            "{}格式无法识别",
            asset.label
        ))));
    };
    if !sniffed.mime.starts_with("image/") {
        return Err(crate::http::error_response(AppError::bad_request(format!(
            "{}要是图片",
            asset.label
        ))));
    }
    // SVG 也让它过，但要走一次净化：这两样都是站内渲染的，别把脚本带进来
    let stored = if sniffed.mime == "image/svg+xml" {
        pixora_core::media::sanitize_svg(&bytes)
    } else {
        bytes
    };

    if let Err(err) = state.repo.put_asset(asset.asset_key, &stored).await {
        return Err(crate::http::error_response(err.into()));
    }
    if let Err(err) = state.settings.set(asset.settings_key, asset.marker).await {
        return Err(crate::http::error_response(err));
    }

    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(auth.user.id),
            asset.asset_key,
            serde_json::json!({ "bytes": stored.len(), "mime": sniffed.mime }),
            rectx.ip.as_deref(),
        )
        .await;
    Ok(stored)
}

/// 站点资产上传的响应：新地址 + 字节数。
fn site_asset_response(
    headers: &HeaderMap,
    url: String,
    stored: &[u8],
    asset: &SiteAsset<'_>,
) -> Response {
    let _ = asset;
    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "url": url,
        "bytes": stored.len(),
        "csrf_token": cookie_value(headers, CSRF_COOKIE),
    })))
}

/// 可勾选的图片格式清单。`label` 是给界面用的显示名：JPEG 归一到 `jpg` 之后
/// 界面上仍然要写「JPEG」，否则管理员会以为这个格式没了。
fn format_options() -> serde_json::Value {
    let items: Vec<serde_json::Value> = pixora_core::media::FORMATS
        .iter()
        .filter(|rule| !matches!(rule.ext, "jpeg" | "tif" | "heif"))
        .map(|rule| {
            serde_json::json!({
                "ext": rule.ext,
                "label": match rule.ext {
                    // 上传页与直链都用 JPG 这个名字，后台也照同一套写，
                    // 两个界面对同一个格式给两个名字最容易让人以为漏了什么
                    "jpg" => "JPG".to_string(),
                    other => other.to_ascii_uppercase(),
                },
            })
        })
        .collect();
    serde_json::Value::Array(items)
}

/// `POST /api/v1/admin/users/{id}/impersonate` — 免登录切换到某个用户。
///
/// 「免登录」指的是不用知道对方密码：服务端直接为目标用户签发一个会话，
/// 同时把管理员自己的会话令牌写进 `pixora_impersonator` Cookie。
/// 于是刷新页面之后仍能切回来——这正是这个功能与「知道密码再登录一次」的区别。
pub async fn user_impersonate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(
        state,
        headers,
        method,
        peer,
        "/admin/users/{id}/impersonate"
    );

    // 必须能拿到管理员自己的会话令牌：用 API Token 调这个接口就没法切回来
    let Some(admin_token) = crate::http::session_cookie(&headers) else {
        return crate::http::error_response(AppError::bad_request(
            "免登录切换需要在网页登录的会话里操作（API Token 没有可返回的会话）",
        ));
    };

    match state
        .impersonate_start(&auth.user, &admin_token, user_id, &rectx.device())
        .await
    {
        Ok(issued) => {
            let cookies = crate::http::merge_cookies(
                crate::http::session_cookies(
                    &state,
                    &issued.session_token,
                    &issued.csrf_token,
                    false,
                )
                .await,
                crate::http::impersonator_cookie_header(&state, &admin_token),
            );
            let payload = match super::me_payload(
                &state,
                &issued.user,
                Some(issued.csrf_token.clone()),
                Some(serde_json::json!({
                    "id": auth.user.id,
                    "uid": auth.user.uid,
                    "email": auth.user.email,
                })),
            )
            .await
            {
                Ok(payload) => payload,
                Err(err) => return crate::http::error_response(err),
            };
            crate::http::with_cookies(finish::<serde_json::Value>(Ok(payload)), cookies)
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/admin/impersonate/stop` — 从目标用户切回管理员。
///
/// 注意它**不能用 `require_admin`**：当前身份是目标用户（可能只是普通用户），
/// 管理权限来自 `pixora_impersonator` 里那个管理员的会话，所以自己校验。
pub async fn impersonate_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    // CSRF 与登录态照旧要过：这一步会换 Cookie
    let auth = match super::require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    let Some(admin_token) = crate::http::impersonator_cookie(&headers) else {
        return crate::http::error_response(AppError::bad_request(
            "当前不是切换过来的身份（没有 pixora_impersonator Cookie）",
        ));
    };
    let admin = match state.impersonator_user(&admin_token).await {
        Ok(admin) => admin,
        Err(err) => return crate::http::error_response(err),
    };

    let csrf = cookie_value(&headers, CSRF_COOKIE).unwrap_or_default();
    let cookies = crate::http::merge_cookies(
        crate::http::session_cookies(&state, &admin_token, &csrf, false).await,
        crate::http::clear_impersonator_cookie(&state),
    );

    state
        .audit_detail(
            actions::IMPERSONATE_STOP,
            Some(admin.id),
            &auth.user.uid,
            serde_json::json!({ "back_to": admin.uid }),
            rectx.ip.as_deref(),
        )
        .await;

    let payload = match super::me_payload(&state, &admin, Some(csrf), None).await {
        Ok(payload) => payload,
        Err(err) => return crate::http::error_response(err),
    };
    crate::http::with_cookies(finish::<serde_json::Value>(Ok(payload)), cookies)
}

// ============================================================
// 8.7 服务条款管理
// ============================================================

/// 条款管理行的形状（契约 8.7）。
fn terms_row_json(row: &pixora_repo::repo::terms::TermsRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.version.id,
        "version": row.version.version,
        "title": row.version.title,
        // 正文要带上：管理端只有这一个列表接口，不带正文的话编辑区每次保存后
        // 都被刷成空的，看起来就像「正文没存进去」
        "body": row.version.body,
        "summary": row.version.summary,
        "status": row.version.status,
        "status_label": terms_status_label(&row.version.status),
        "effective_at": row.version.effective_at.map(rfc3339),
        "published_at": row.version.published_at.map(rfc3339),
        "require_reconsent": row.version.require_reconsent,
        "consent_count": row.consent_count,
        "user_total": row.user_total,
        "updated_at": rfc3339(row.version.created_at),
    })
}

fn terms_status_label(status: &str) -> &'static str {
    match status {
        "active" => "生效中",
        "archived" => "已归档",
        _ => "草稿",
    }
}

/// `GET /api/v1/admin/terms` — 版本列表（带同意人数）。
pub async fn terms_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/terms");

    match state.repo.list_terms().await {
        Ok(rows) => finish::<Vec<serde_json::Value>>(Ok(rows.iter().map(terms_row_json).collect())),
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// 从 body 组一版条款内容（新建与修改共用）。
///
/// `version` 与 `title` 在修改时也必填：界面上它们本来就是一起提交的，
/// 少给一个就等于「把版本号/标题清空」，那是不能接受的默认行为。
fn terms_input(
    body: &serde_json::Value,
    existing: Option<&pixora_repo::model::TermsVersion>,
    admin_id: i64,
) -> Result<pixora_repo::repo::terms::NewTermsVersion, Response> {
    let version = match field_opt_str(body, "version") {
        Some(value) => value,
        None => existing
            .map(|item| item.version.clone())
            .ok_or_else(|| bad_request("缺少参数 version"))?,
    };
    let title = match field_opt_str(body, "title") {
        Some(value) => value,
        None => existing
            .map(|item| item.title.clone())
            .ok_or_else(|| bad_request("缺少参数 title"))?,
    };
    let body_text = match field_opt_str(body, "body") {
        Some(value) => value,
        None => existing
            .map(|item| item.body.clone())
            .ok_or_else(|| bad_request("缺少参数 body"))?,
    };
    let summary =
        field_opt_str(body, "summary").or_else(|| existing.and_then(|item| item.summary.clone()));
    let require_reconsent = field_opt_bool(body, "require_reconsent")
        .or_else(|| existing.map(|item| item.require_reconsent))
        .unwrap_or(true);
    let effective_at = if super::field_present(body, "effective_at") {
        match field_opt_str(body, "effective_at") {
            Some(raw) => match chrono::DateTime::parse_from_rfc3339(&raw) {
                Ok(at) => Some(at.with_timezone(&chrono::Utc)),
                Err(_) => return Err(bad_request("effective_at 必须是 RFC3339 时间串")),
            },
            None => None,
        }
    } else {
        existing.and_then(|item| item.effective_at)
    };

    Ok(pixora_repo::repo::terms::NewTermsVersion {
        version,
        title,
        body: body_text,
        summary,
        require_reconsent,
        effective_at,
        created_by: Some(admin_id),
    })
}

/// `POST /api/v1/admin/terms` — 新建一版。
///
/// 仓储一律以 `draft` 落地；body 里 `status: "active"` 时再走一次发布，
/// 让「写好了但没对外」与「已对外」在库里始终区分得开。
pub async fn terms_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/terms");

    let input = match terms_input(&body, None, auth.user.id) {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let status = field_opt_str(&body, "status").unwrap_or_else(|| "draft".into());
    if !matches!(status.as_str(), "draft" | "active") {
        return bad_request("status 只能是 draft 或 active");
    }

    // 走领域服务：版本号格式校验与「版本号已存在」都在那里，
    // 直接写仓储的话冲突会以一句数据库唯一约束错误抛给管理员
    let terms_input_value = pixora_core::terms::TermsInput {
        version: input.version.clone(),
        title: input.title.clone(),
        body: input.body.clone(),
        summary: input.summary.clone(),
        require_reconsent: input.require_reconsent,
        effective_at: input.effective_at,
        publish: status == "active",
    };
    let created = match state.terms_create(auth.user.id, terms_input_value).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    if status == "active" {
        let actor = auth.user.id;
        state
            .audit_detail(
                actions::TERMS_PUBLISH,
                Some(actor),
                created.version.clone(),
                serde_json::json!({ "via": "create" }),
                rectx.ip.as_deref(),
            )
            .await;
    }

    let rows = match state.repo.list_terms().await {
        Ok(rows) => rows,
        Err(err) => return crate::http::error_response(err.into()),
    };
    match rows.iter().find(|row| row.version.id == created.id) {
        Some(row) => finish::<serde_json::Value>(Ok(terms_row_json(row))),
        None => not_found("条款版本"),
    }
}

/// `PATCH /api/v1/admin/terms/{id}` — 只能改草稿。
///
/// 已生效版本的正文被改过，之前那些「同意」记录就说不清了
/// （用户同意的是另一份文本）。所以先拒，再让管理员走「发新版」。
pub async fn terms_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(terms_id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/terms/{id}");

    let existing = match state.repo.find_terms(terms_id).await {
        Ok(Some(version)) => version,
        Ok(None) => return not_found("条款版本"),
        Err(err) => return crate::http::error_response(err.into()),
    };

    let input = match terms_input(&body, Some(&existing), auth.user.id) {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let terms_input_value = pixora_core::terms::TermsInput {
        version: input.version.clone(),
        title: input.title.clone(),
        body: input.body.clone(),
        summary: input.summary.clone(),
        require_reconsent: input.require_reconsent,
        effective_at: input.effective_at,
        publish: false,
    };
    // 领域服务里判「生效中的版本不能就地改」，并给出该怎么做的提示
    if let Err(err) = state
        .terms_update(auth.user.id, terms_id, terms_input_value)
        .await
    {
        return crate::http::error_response(err);
    }

    let _ = rectx;

    let rows = match state.repo.list_terms().await {
        Ok(rows) => rows,
        Err(err) => return crate::http::error_response(err.into()),
    };
    match rows.iter().find(|row| row.version.id == terms_id) {
        Some(row) => finish::<serde_json::Value>(Ok(terms_row_json(row))),
        None => not_found("条款版本"),
    }
}

/// `POST /api/v1/admin/terms/{id}/publish` — 发布并归档旧版本。
///
/// 返回被归档的那一版，界面可以显示「v2.0 已归档」。
pub async fn terms_publish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(terms_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/terms/{id}/publish");

    // 先记下当前生效版本：发布之后它就变成 archived，查不出来了
    let previous = state.repo.active_terms().await.ok().flatten();
    if let Err(err) = state.repo.publish_terms(terms_id, now()).await {
        return crate::http::error_response(err.into());
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::TERMS_PUBLISH,
            Some(actor),
            format!("terms:{terms_id}"),
            serde_json::json!({ "archived": previous.as_ref().map(|item| item.version.clone()) }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "status": "active",
        "archived": previous.map(|item| item.version),
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `DELETE /api/v1/admin/terms/{id}` — 生效中的版本拒绝删除。
pub async fn terms_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(terms_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/terms/{id}");

    let existing = match state.repo.find_terms(terms_id).await {
        Ok(Some(version)) => version,
        Ok(None) => return not_found("条款版本"),
        Err(err) => return crate::http::error_response(err.into()),
    };
    if existing.is_active() {
        return bad_request("生效中的版本不能删除，请先发布一个替代版本");
    }

    // 删除是真删：版本行与它的同意记录一起消失（同一个事务）。
    // 删掉多少条同意记录要写进审计——那是「谁同意过哪一版」的历史
    match state.repo.delete_terms(terms_id).await {
        Ok(consents) => {
            let actor = auth.user.id;
            state
                .audit_detail(
                    actions::SETTINGS_CHANGE,
                    Some(actor),
                    existing.version.clone(),
                    serde_json::json!({ "action": "delete", "consents_removed": consents }),
                    rectx.ip.as_deref(),
                )
                .await;
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "ok": true,
                "consents_removed": consents,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// 某个提供方在设置表里的三个键名（开关、客户端 ID、密钥）。
/// 拼错一个字符就会写到一个没人读的键上，所以收在一处。
fn login_keys(provider_name: &str) -> (String, String, String) {
    (
        format!("login.{provider_name}.enabled"),
        format!("login.{provider_name}.client_id"),
        format!("login.{provider_name}.client_secret"),
    )
}
/// `GET /api/v1/admin/login-methods` — 后台「登录方式」页要的现状。
///
/// 密钥只回「配没配」（`has_secret`），不回明文：界面上显示「已配置」就够了，
/// 明文每回一次就多一份泄露面。
pub async fn admin_login_methods_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/login-methods");

    let mut providers = serde_json::Map::new();
    for name in provider::ALL {
        if name == provider::PHONE {
            continue;
        }
        let (enabled_key, id_key, secret_key) = login_keys(name);
        providers.insert(
            name.to_string(),
            serde_json::json!({
                "label": provider::label(name),
                "enabled": state.settings.bool(&enabled_key, false).await.unwrap_or(false),
                "client_id": state.settings.string(&id_key, "").await.unwrap_or_default(),
                "has_secret": state.settings.secret_configured(&secret_key).await.unwrap_or(false),
                // 回调地址要在对方平台原样登记，页面上直接给出来让人复制
                "callback": crate::routes::oauth::redirect_uri(&state, name).await,
            }),
        );
    }
    finish::<serde_json::Value>(Ok(serde_json::json!({
        "providers": providers,
        "phone": {
            "enabled": state
                .settings
                .bool(keys::LOGIN_PHONE_ENABLED, false)
                .await
                .unwrap_or(false),
        },
        "sms": {
            "enabled": state.settings.bool(keys::SMS_ENABLED, false).await.unwrap_or(false),
            "access_key_id": state.settings.string(keys::SMS_ACCESS_KEY_ID, "").await.unwrap_or_default(),
            "has_secret": state
                .settings
                .secret_configured(keys::SMS_ACCESS_KEY_SECRET)
                .await
                .unwrap_or(false),
            "sign_name": state.settings.string(keys::SMS_SIGN_NAME, "").await.unwrap_or_default(),
            "template_code": state.settings.string(keys::SMS_TEMPLATE_CODE, "").await.unwrap_or_default(),
        },
    })))
}

/// `PUT /api/v1/admin/login-methods` — 保存登录方式的配置。
///
/// 密钥类字段只在「给了非空值」时写：界面上的空输入意思是「保持原值」，
/// 与 SMTP 密码同一套规矩。清空某个提供方的凭据就关掉它的开关。
pub async fn admin_login_methods_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/login-methods");
    let mut touched: Vec<String> = Vec::new();

    if let Some(providers) = body.get("providers").and_then(|value| value.as_object()) {
        for (name, entry) in providers {
            if !provider::is_known(name) || name == provider::PHONE {
                return bad_request(format!("不支持的登录方式：{name}"));
            }
            let (enabled_key, id_key, secret_key) = login_keys(name);
            if let Some(enabled) = entry.get("enabled").and_then(|value| value.as_bool()) {
                if let Err(err) = state.settings.set_bool(&enabled_key, enabled).await {
                    return error_response(err);
                }
                touched.push(enabled_key);
            }
            if let Some(client_id) = field_opt_str(entry, "client_id") {
                if let Err(err) = state.settings.set(&id_key, client_id.trim()).await {
                    return error_response(err);
                }
                touched.push(id_key);
            }
            if let Some(secret) = field_opt_str(entry, "client_secret") {
                if !secret.trim().is_empty() {
                    if let Err(err) = state.settings.set_secret(&secret_key, secret.trim()).await {
                        return error_response(err);
                    }
                }
            }
        }
    }

    if let Some(phone) = body.get("phone") {
        if let Some(enabled) = phone.get("enabled").and_then(|value| value.as_bool()) {
            if let Err(err) = state
                .settings
                .set_bool(keys::LOGIN_PHONE_ENABLED, enabled)
                .await
            {
                return error_response(err);
            }
            touched.push(keys::LOGIN_PHONE_ENABLED.to_string());
        }
    }

    if let Some(sms) = body.get("sms") {
        let writes: [(&str, Option<String>); 4] = [
            (
                keys::SMS_ENABLED,
                sms.get("enabled")
                    .and_then(|value| value.as_bool())
                    .map(|value| value.to_string()),
            ),
            (
                keys::SMS_ACCESS_KEY_ID,
                field_opt_str(sms, "access_key_id").map(|value| value.trim().to_string()),
            ),
            (
                keys::SMS_SIGN_NAME,
                field_opt_str(sms, "sign_name").map(|value| value.trim().to_string()),
            ),
            (
                keys::SMS_TEMPLATE_CODE,
                field_opt_str(sms, "template_code").map(|value| value.trim().to_string()),
            ),
        ];
        for (key, value) in writes {
            if let Some(value) = value {
                if let Err(err) = state.settings.set(key, &value).await {
                    return error_response(err);
                }
                touched.push(key.to_string());
            }
        }
        if let Some(secret) = field_opt_str(sms, "access_key_secret") {
            if !secret.trim().is_empty() {
                if let Err(err) = state
                    .settings
                    .set_secret(keys::SMS_ACCESS_KEY_SECRET, secret.trim())
                    .await
                {
                    return error_response(err);
                }
            }
        }
    }

    state
        .audit_detail(
            pixora_core::audit::actions::SETTINGS_CHANGE,
            Some(auth.user.id),
            "login-methods",
            serde_json::json!({ "touched": touched }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "touched": touched,
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// 管理端：把某个用户的某个登录方式摘掉（用户自己来不了时才用得上）。
pub async fn admin_identity_unbind(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path((user_id, provider_name)): Path<(i64, String)>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_admin(
        &state,
        &headers,
        rectx.ip.as_deref(),
        &method,
        "/admin/users",
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let target = match state.repo.find_user_by_id(user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found("用户"),
        Err(err) => return error_response(err.into()),
    };
    if let Err(err) = state.ensure_not_install_admin(&auth.user, &target).await {
        return error_response(err);
    }
    let provider_name = provider_name.to_ascii_lowercase();
    match state
        .unbind_identity(&target, &provider_name, rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => error_response(err),
    }
}
