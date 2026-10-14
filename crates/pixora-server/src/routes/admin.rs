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

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::admin::Disposition;
use pixora_core::audit::actions;
use pixora_core::quota::QuotaInput;
use pixora_core::settings::{defaults as settings_defaults, keys, normalize_prefix};
use pixora_core::stats::BackendView;
use pixora_core::{AppError, AppState};
use pixora_repo::model::{now, RateLimitPolicy};
use pixora_repo::repo::audit::AuditFilter;
use pixora_repo::repo::backends::{BackendPatch, NewBackend};
use pixora_repo::repo::iprules::{IpRuleFilter, NewIpRule};
use pixora_repo::repo::users::UserFilter;
use pixora_repo::PageRequest;
use crate::http::{
    cookie_value, finish, finish_page, parse_i64, parse_str, request_context, rfc3339, CSRF_COOKIE,
};

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
        let auth = match require_admin(
            &$state,
            &$headers,
            rectx.ip.as_deref(),
            &$method,
            $path,
        )
        .await
        {
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
            super::audit_admin_view(&state, &auth.user, &target.uid, "admin_user_detail", rectx.ip.as_deref())
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
        "unlimited": policy.unlimited,
        "custom": policy.custom,
        "note": policy.note,
    });
    if let Some(object) = body.as_object_mut() {
        object.insert("limits".into(), limits);
    }
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

    let detail = match state
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
        Ok(detail) => detail,
        Err(err) => return crate::http::error_response(err),
    };

    // 与详情同一个形状：补 `limits` 别名，并按契约第 11 章带上 csrf_token
    let mut body = serde_json::to_value(&detail).unwrap_or_else(|_| serde_json::json!({}));
    add_user_detail_extras(&mut body, &detail);
    body["csrf_token"] = serde_json::to_value(cookie_value(&headers, CSRF_COOKIE))
        .unwrap_or(serde_json::Value::Null);
    finish::<serde_json::Value>(Ok(body))
}

/// `POST /api/v1/admin/users/{id}/quota` — 单个用户调配额。
///
/// 校验（单用户上限、合计不超系统总额）在领域层，超出时错误里带「还差多少字节」。
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

    finish(state.set_user_quota(auth.user.id, user_id, quota_bytes).await)
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
        duration_days: field_opt_i64(&body, "duration_days"),
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

/// `DELETE /api/v1/admin/users/{id}` — 删除账号（内容随外键级联，物理文件等 GC）。
pub async fn user_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(user_id): Path<i64>,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/users/{id}");

    match state
        .admin_delete_user(&auth.user, user_id, rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
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
    if let Err(err) = state
        .settings
        .set(keys::UPLOAD_PREFIX, &normalized)
        .await
    {
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
                return bad_request(format!("容量模式只能是 auto / manual / hidden，收到 {value}"));
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
    let label = field_opt_str(&body, "label").unwrap_or_else(|| pixora_storage::names::label(&driver).to_string());
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

    match state.repo.find_backend(backend_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found("存储后端"),
        Err(err) => return crate::http::error_response(err.into()),
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
    let (_rectx, _auth) = admin_guard!(state, headers, method, peer, "/admin/storage/backends/{id}/test");

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
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
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
    match raw.as_deref().map(serde_json::from_str::<serde_json::Value>) {
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
    let depth = match state.repo.queue_depth().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };
    // 用户覆盖表一页给够（200 行）：管理页要一眼看到「谁被单独调过」
    let rows = match state
        .repo
        .list_user_policies(&UserFilter::default(), PageRequest::new(Some(1), Some(200), 50, 200))
        .await
    {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };

    let mut users = Vec::with_capacity(rows.items.len());
    let mut realtime = Vec::with_capacity(rows.items.len());
    for row in &rows.items {
        let effective = row.policy.clone().unwrap_or_else(|| global.clone());
        let counters = state.limits.counters_for(row.user.id).await.unwrap_or_default();
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
        "max_batch_size": policy.max_batch_size,
        "max_file_size": policy.max_file_size,
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
        unlimited: field_opt_bool(body, "unlimited").unwrap_or(fallback.unlimited),
        note: field_opt_str(body, "note").or(fallback.note),
        updated_at: now(),
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
            finish_page::<serde_json::Value>(Ok(pixora_repo::Page::new(
                items, result.total, page,
            )))
        }
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

    let site = match state
        .settings
        .site(state.config.base_url.as_deref())
        .await
    {
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
    let background = state
        .settings
        .raw(keys::APPEARANCE_BACKGROUND)
        .await
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty());

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "site": {
            "name": site.name,
            "base_url": site.base_url,
            "plaza_public": site.plaza_public,
            "require_login_to_upload": site.require_login_to_upload,
            "register_enabled": site.register_enabled,
            "plaza_grid_columns": site.plaza_grid_columns,
        },
        "upload": {
            "max_file_bytes": upload.max_file_bytes,
            "max_batch_size": upload.max_batch,
            "max_pixels": upload.max_pixels,
            "queue_depth": upload.queue_depth,
            "formats": upload.formats,
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
        "appearance": { "background": background },
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
        let writes: [(&str, Option<String>); 6] = [
            (keys::SITE_NAME, field_opt_str(&site, "name")),
            (keys::SITE_BASE_URL, field_opt_str(&site, "base_url")),
            (
                keys::PLAZA_PUBLIC,
                field_opt_bool(&site, "plaza_public").map(|value| value.to_string()),
            ),
            (
                keys::UPLOAD_REQUIRE_LOGIN,
                field_opt_bool(&site, "require_login_to_upload").map(|value| value.to_string()),
            ),
            (
                keys::REGISTER_ENABLED,
                field_opt_bool(&site, "register_enabled").map(|value| value.to_string()),
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
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
    }

    if let Some(upload) = body.get("upload").and_then(|value| value.as_object()) {
        let upload = serde_json::Value::Object(upload.clone());
        let writes: [(&str, Option<String>); 4] = [
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
                keys::UPLOAD_QUEUE_DEPTH,
                field_opt_i64(&upload, "queue_depth").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
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
            (keys::MAIL_SMTP_SECURITY, field_opt_str(&mail, "smtp_security")),
            (keys::MAIL_SMTP_USER, field_opt_str(&mail, "smtp_user")),
            (keys::MAIL_FROM_NAME, field_opt_str(&mail, "from_name")),
            (keys::MAIL_FROM_EMAIL, field_opt_str(&mail, "from_email")),
            (keys::MAIL_APPEAL_EMAIL, field_opt_str(&mail, "appeal_email")),
            (
                keys::ANTISPAM_DISPOSABLE,
                field_opt_bool(&mail, "disposable_check").map(|value| value.to_string()),
            ),
        ];
        if let Err(resp) = write_settings(&state, &writes, &mut touched).await {
            return resp;
        }
        if let Some(value) = field_opt_bool(&mail, "mx_check") {
            if let Err(err) = state
                .settings
                .set_bool(keys::ANTISPAM_MX, value)
                .await
            {
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
        if super::field_present(&appearance, "background") {
            // 显式给 null 表示清掉背景图（不是「不改」）
            let value = field_opt_str(&appearance, "background").unwrap_or_default();
            if let Err(err) = state
                .settings
                .set(keys::APPEARANCE_BACKGROUND, &value)
                .await
            {
                return crate::http::error_response(err);
            }
            touched.push(keys::APPEARANCE_BACKGROUND.to_string());
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

    let to = field_opt_str(&body, "to")
        .unwrap_or_else(|| auth.user.email.clone());
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

/// `POST /api/v1/admin/thumbnails/rebuild` — 服务端兜底缩略图重建。
///
/// 兜底关着时回 `{enabled: false, queued: 0}`：契约要求如实说明「没做」，
/// 而不是假装排队了 128 张。真正的生成交给后台任务按轮次跑。
pub async fn thumbnails_rebuild(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let (rectx, auth) = admin_guard!(state, headers, method, peer, "/admin/thumbnails/rebuild");

    match pixora_queue::thumbs::rebuild_all(&state).await {
        Ok(outcome) => {
            let actor = auth.user.id;
            state
                .audit_detail(
                    actions::SETTINGS_CHANGE,
                    Some(actor),
                    "thumbnails.rebuild",
                    serde_json::json!({ "enabled": outcome.enabled, "queued": outcome.pending }),
                    rectx.ip.as_deref(),
                )
                .await;
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "queued": outcome.pending,
                "enabled": outcome.enabled,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        Err(err) => crate::http::error_response(err),
    }
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
        Ok(rows) => finish::<Vec<serde_json::Value>>(Ok(
            rows.iter().map(terms_row_json).collect(),
        )),
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
    let summary = field_opt_str(body, "summary")
        .or_else(|| existing.and_then(|item| item.summary.clone()));
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

    let created = match state.repo.create_terms(&input).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };

    if status == "active" {
        let previous = state.repo.active_terms().await.ok().flatten();
        if let Err(err) = state.repo.publish_terms(created.id, now()).await {
            return crate::http::error_response(err.into());
        }
        let actor = auth.user.id;
        state
            .audit_detail(
                actions::TERMS_PUBLISH,
                Some(actor),
                created.version.clone(),
                serde_json::json!({ "archived": previous.map(|item| item.version) }),
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
    if existing.status != pixora_repo::model::terms_status::DRAFT {
        return bad_request("只有草稿可以修改；已生效的版本请发布新版本");
    }

    let input = match terms_input(&body, Some(&existing), auth.user.id) {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if let Err(err) = state.repo.update_terms(terms_id, &input).await {
        return crate::http::error_response(err.into());
    }

    let actor = auth.user.id;
    state
        .audit_detail(
            actions::SETTINGS_CHANGE,
            Some(actor),
            existing.version.clone(),
            serde_json::json!({ "action": "update_draft" }),
            rectx.ip.as_deref(),
        )
        .await;

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

    match state.repo.delete_terms(terms_id).await {
        Ok(()) => {
            let actor = auth.user.id;
            state
                .audit_detail(
                    actions::SETTINGS_CHANGE,
                    Some(actor),
                    existing.version.clone(),
                    serde_json::json!({ "action": "delete" }),
                    rectx.ip.as_deref(),
                )
                .await;
            finish::<serde_json::Value>(Ok(serde_json::json!({
                "ok": true,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            })))
        }
        // 有同意记录的版本删不掉（外键 RESTRICT）：错误信息里说清原因
        Err(err) => crate::http::error_response(AppError::bad_request(format!(
            "该版本存在用户同意记录，不能删除（{}）",
            err
        ))),
    }
}
