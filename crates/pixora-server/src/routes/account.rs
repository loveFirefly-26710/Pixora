//! 当前用户路由（`/api/v1/me`）：资料、口令、会话、恢复码、二次验证、API 令牌。
//!
//! 全部要求登录。三类「自我处置」的动作有各自的坑：
//!
//!   - **改密码**：改完踢掉其它设备但**保留当前会话**，否则用户当场被自己踢下线，
//!     会以为「改密码把账号弄坏了」。当前会话 id 只有会话模式才有（Token 模式没有
//!     会话这一层），所以要按 `Option<i64>` 透传。
//!   - **删会话**：`DELETE /me/sessions` 有两种语义，靠 `keep_current=true` 区分
//!     「吊销其它设备」与「全部下线」。
//!   - **二次验证**：`setup` 会先把密钥写进库（但保持未启用），所以 `enable` /
//!      `disable` **必须重新读一遍用户行**——鉴权时加载的那份 `auth.user` 里还没有
//!      `totp_secret`，直接拿它去校验会永远报「请先生成密钥」。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::AppState;
use pixora_crypto::token as crypto_token;

/** Token 过期天数上限：十年。再长不如留空用「永不过期」。 */
const MAX_TOKEN_DAYS: i64 = 3650;

use crate::http::{
    clear_cookies, cookie_value, finish, request_context, rfc3339, with_cookies, CSRF_COOKIE,
};

use super::{
    bad_request, field_opt_bool, field_opt_i64, field_opt_str, field_str, not_found,
    pref_key_login_alert, pref_key_new_device_guard, require_user, user_view, PeerAddr,
};

/// 令牌权限范围的白名单。写死在这里而不是从库里读：它是契约的一部分，
/// 出现契约外的范围说明要么是前端传错，要么是有人在探接口。
const ALLOWED_SCOPES: [&str; 4] = ["upload", "read", "write", "admin"];

/// `GET /api/v1/me`。
///
/// 顺带回显 `csrf_token`：会话 Cookie 是 HttpOnly 的，前端刷新页面后要凭
/// `pixora_csrf` Cookie 里的值还原请求头；把它放在响应体里，前端也可以
/// 只依赖内存中的值（更安全的做法是两者取一）。
pub async fn me_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }

    // 会话模式回显请求里的 CSRF Cookie；Token 模式没有这条线，给 null
    let csrf = if auth.via_token {
        None
    } else {
        cookie_value(&headers, CSRF_COOKIE)
    };
    match super::me_payload(&state, &auth.user, csrf).await {
        Ok(payload) => finish::<serde_json::Value>(Ok(payload)),
        Err(err) => crate::http::error_response(err),
    }
}

/// `PATCH /api/v1/me`：只改两个界面偏好。
///
/// 邮箱、密码这类敏感字段刻意不在这里改——邮箱要重新验证、密码要走
/// 「旧密码 + 踢下线」，把它们混进一个通用补丁接口会让「改了什么」说不清。
pub async fn me_patch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let mut changed = false;
    if let Some(value) = field_opt_bool(&body, "login_alert") {
        if let Err(err) = state
            .settings
            .set_bool(&pref_key_login_alert(&auth.user.uid), value)
            .await
        {
            return crate::http::error_response(err);
        }
        changed = true;
    }
    if let Some(value) = field_opt_bool(&body, "new_device_guard") {
        if let Err(err) = state
            .settings
            .set_bool(&pref_key_new_device_guard(&auth.user.uid), value)
            .await
        {
            return crate::http::error_response(err);
        }
        changed = true;
    }
    if !changed {
        return bad_request("没有可修改的字段（login_alert / new_device_guard）");
    }

    // 重新读一遍用户行：偏好在 `settings` 表里，而 `user_view` 要的是最新状态
    let fresh = match state.repo.find_user_by_id(auth.user.id).await {
        Ok(Some(user)) => user,
        Ok(None) => return not_found("用户"),
        Err(err) => return crate::http::error_response(err.into()),
    };
    match user_view(&state, &fresh).await {
        Ok(payload) => finish::<serde_json::Value>(Ok(payload)),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/me/password`：改密码，踢掉其它设备但保留当前会话。
pub async fn password_change(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let current = match field_str(&body, "current") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let next = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match state
        .change_password(
            &auth.user,
            &current,
            &next,
            auth.session_id,
            &rectx.device(),
        )
        .await
    {
        Ok(revoked) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "revoked_sessions": revoked,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/me/sessions`。
///
/// `current` 靠会话 id 比对：Token 模式没有会话 id，全部标 false
/// （前端据此不显示「当前设备」徽标）。
pub async fn sessions_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }

    let current_id = auth.session_id.unwrap_or(-1);
    match state.list_sessions(auth.user.id, current_id).await {
        Ok(sessions) => finish::<Vec<serde_json::Value>>(Ok(sessions
            .into_iter()
            .map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "device": session.device,
                    "ip": session.ip,
                    "created_at": rfc3339(session.created_at),
                    "last_seen_at": rfc3339(session.last_seen_at),
                    "current": session.current,
                })
            })
            .collect())),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/me/sessions/{id}`：踢掉某一台设备。
pub async fn session_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(session_id): Path<i64>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    // 仓储的吊销语句带 `user_id` 条件，别人的会话 id 进来只会影响 0 行——
    // 不额外报「不属于你」，避免变成「某个 id 存不存在」的探针
    match state
        .revoke_session(auth.user.id, session_id, &rectx.device())
        .await
    {
        Ok(revoked) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "revoked": revoked,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/me/sessions`：一套接口两种语义。
///
/// - `?keep_current=true`：吊销除当前会话外的全部会话（「退出其它设备」）。
/// - 不带（或 false）：全部吊销，当前设备也下线（「退出所有设备」）——
///   这时要同时清 Cookie，否则浏览器还留着一个已经失效的会话 Cookie。
pub async fn sessions_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let keep_current = matches!(
        query.get("keep_current").map(|value| value.trim()),
        Some("true") | Some("1") | Some("yes")
    );

    // Token 模式没有「当前会话」可留，keep_current 只能退化成「全撤」
    let outcome = match (keep_current, auth.session_id) {
        (true, Some(session_id)) => {
            state
                .revoke_other_sessions(auth.user.id, session_id, &rectx.device())
                .await
        }
        _ => state
            .repo
            .revoke_all_sessions(auth.user.id, pixora_repo::model::now())
            .await
            .map_err(pixora_core::AppError::from),
    };

    match outcome {
        Ok(revoked) => {
            let response = finish::<serde_json::Value>(Ok(serde_json::json!({
                "revoked": revoked,
                "kept_current": keep_current && auth.session_id.is_some(),
            })));
            if !keep_current {
                with_cookies(response, clear_cookies(&state))
            } else {
                response
            }
        }
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/me/recovery-codes`：只看剩余数量。
///
/// 明文只在生成那一次返回，这里恒给空数组——「能看到就等于泄露」。
pub async fn recovery_codes_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };

    match state.recovery_code_count(auth.user.id).await {
        Ok(remaining) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "codes": Vec::<String>::new(),
            "remaining": remaining,
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/me/recovery-codes`：重新生成，明文只此一次。
///
/// 生成即作废旧码（仓储里是整体替换）：两套码同时有效等于把「一次性」
/// 这件事悄悄放宽了一倍。
pub async fn recovery_codes_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    // 没开二次验证时生成恢复码没有意义（登录流程根本不会问它），
    // 而且会给用户「我已经有二次验证了」的错觉——拒绝并说清原因
    if !auth.user.totp_enabled {
        return bad_request("请先开启二次验证，再生成恢复码");
    }

    match state.regenerate_recovery_codes(&auth.user).await {
        Ok(codes) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "codes": codes,
            "remaining": codes.len(),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/me/totp`：按 `action` 分派 setup / enable / disable。
pub async fn totp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let action = match field_str(&body, "action") {
        Ok(value) => value.to_ascii_lowercase(),
        Err(resp) => return resp,
    };

    match action.as_str() {
        "setup" => match state.totp_setup(&auth.user).await {
            Ok(setup) => finish::<serde_json::Value>(Ok(serde_json::json!({
                "secret": setup.secret,
                "otpauth_url": setup.otpauth_url,
                "csrf_token": cookie_value(&headers, CSRF_COOKIE),
            }))),
            Err(err) => crate::http::error_response(err),
        },
        "enable" | "disable" => {
            let code = match field_str(&body, "code") {
                Ok(value) => value,
                Err(resp) => return resp,
            };
            // setup 刚把密钥写进库里，而鉴权时加载的这份用户行还是旧的：
            // 必须重新读一次，否则永远是「请先生成密钥」
            let fresh = match state.repo.find_user_by_id(auth.user.id).await {
                Ok(Some(user)) => user,
                Ok(None) => return not_found("用户"),
                Err(err) => return crate::http::error_response(err.into()),
            };
            let outcome = if action == "enable" {
                state
                    .totp_enable(&fresh, &code, &rectx.device())
                    .await
                    .map(|_| true)
            } else {
                state
                    .totp_disable(&fresh, &code, &rectx.device())
                    .await
                    .map(|_| false)
            };
            match outcome {
                Ok(enabled) => finish::<serde_json::Value>(Ok(serde_json::json!({
                    "enabled": enabled,
                    "csrf_token": cookie_value(&headers, CSRF_COOKIE),
                }))),
                Err(err) => crate::http::error_response(err),
            }
        }
        other => bad_request(format!("不支持的二次验证操作：{other}")),
    }
}

/// `GET /api/v1/me/tokens`。
///
/// 列表里**绝不带 `token` 字段**：明文只在签发那一刻返回过一次，
/// 之后连服务端都只有哈希。已撤销的也列出来（安全页要回答「它什么时候被撤的」）。
pub async fn tokens_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }

    match state.repo.list_api_tokens(auth.user.id).await {
        Ok(tokens) => finish::<Vec<serde_json::Value>>(Ok(tokens
            .into_iter()
            .map(|token| {
                serde_json::json!({
                    "id": token.id,
                    "name": token.name,
                    "display": token.display,
                    "scopes": token.scopes,
                    "created_at": rfc3339(token.created_at),
                    "expires_at": token.expires_at.map(rfc3339),
                    "last_used_at": token.last_used_at.map(rfc3339),
                    "revoked": token.revoked_at.is_some(),
                })
            })
            .collect())),
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// `POST /api/v1/me/tokens`：签发。明文只此一次。
pub async fn tokens_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let name = field_opt_str(&body, "name").unwrap_or_else(|| "未命名令牌".to_string());
    if name.chars().count() > 64 {
        return bad_request("令牌名称不能超过 64 个字");
    }

    // 范围缺省给最小可用的一组：能读能上传，但不能写（改/删要显式申请）
    let mut scopes = super::field_string_list(&body, "scopes")
        .unwrap_or_else(|| vec!["upload".to_string(), "read".to_string()]);
    for scope in &scopes {
        if !ALLOWED_SCOPES.contains(&scope.as_str()) {
            return bad_request(format!("未知的权限范围：{scope}"));
        }
    }
    scopes.sort();
    scopes.dedup();

    // `expires_in_days` 省略或 null 表示永不过期。天数现在是前端手输的，
    // 手滑填个 0、负数或 99999 要当场报错，不能悄悄折成「永远不过期」——
    // 那等于把用户以为收窄了的令牌变成永久的，方向正好反了。
    let expires_at = match field_opt_i64(&body, "expires_in_days") {
        None => None,
        Some(days) if days >= 1 && days <= MAX_TOKEN_DAYS => {
            Some(pixora_repo::model::now() + chrono::Duration::days(days))
        }
        Some(days) => {
            return bad_request(format!(
                "过期天数要在 1 到 {MAX_TOKEN_DAYS} 之间（收到 {days}）；永不过期请留空"
            ))
        }
    };

    let plain = match crypto_token::new_api_token() {
        Ok(token) => token,
        Err(err) => return crate::http::error_response(err.into()),
    };
    // 展示片段取前缀那一段（`px_live_` + 8 位十六进制），界面上用来让用户认领令牌
    let display: String = plain.chars().take(16).collect();

    let created = match state
        .repo
        .create_api_token(&pixora_repo::repo::tokens::NewApiToken {
            user_id: auth.user.id,
            name: name.clone(),
            token_hash: crypto_token::token_hash(&plain),
            display: display.clone(),
            scopes: scopes.clone(),
            expires_at,
        })
        .await
    {
        Ok(record) => record,
        Err(err) => return crate::http::error_response(err.into()),
    };

    // 签发令牌是个值得留档的动作：它等于发了一把长期钥匙
    let actor = auth.user.id;
    state
        .audit_detail(
            pixora_core::audit::actions::SETTINGS_CHANGE,
            Some(actor),
            format!("token:{}", created.id),
            serde_json::json!({ "scopes": scopes, "expires_at": expires_at.map(rfc3339) }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "id": created.id,
        "name": created.name,
        "token": plain,
        "display": display,
        "scopes": created.scopes,
        "created_at": rfc3339(created.created_at),
        "expires_at": created.expires_at.map(rfc3339),
        "csrf_token": cookie_value(&headers, CSRF_COOKIE),
    })))
}

/// `DELETE /api/v1/me/tokens/{id}`：撤销（只写 `revoked_at`，不物理删）。
pub async fn token_revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(token_id): Path<i64>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    // 仓储的更新语句带 `user_id`，撤销别人的令牌只会影响 0 行
    match state
        .repo
        .revoke_api_token(token_id, auth.user.id)
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err.into()),
    }
}
