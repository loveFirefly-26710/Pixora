//! 认证路由：注册、登录、登出、找回密码。
//!
//! 这一组全部是**未登录也要能调**的公开端点，所以没有 `require_user`，
//! 但**每一处都要过 `request_context`**：注册与登录是风控的主战场
//! （同 IP 失败次数、临时封禁、发码频率都按真实 IP 统计）。
//!
//! 两条来自领域层的规矩，这里只是照搬：
//!   - 登录成功必须同时下发**两个** Cookie（会话 + CSRF）。CSRF Cookie 不设
//!     HttpOnly，因为双提交模式要求前端能读到它放进请求头。
//!   - 「账号不存在」与「密码错误」给同一个错误码（`INVALID_CREDENTIALS`），
//!     而「暂停/封禁」必须给各自的错误码与原因——客户端要显示不同的提示
//!     与申诉入口。这两件事都在 `AppState::login` 里做完了。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use pixora_core::{AppError, AppState};

use crate::http::{
    api_token, clear_cookies, clear_impersonator_cookie, error_response, finish, merge_cookies,
    request_context, session_cookie, session_cookies, with_cookies,
};

use super::{bad_request, field_opt_str, field_str, PeerAddr};

/// `POST /api/v1/auth/register/code` — 公开。
///
/// 反垃圾（一次性邮箱、MX 校验）与频率限制（同邮箱 5 次/小时、同 IP 20 次/小时、
/// 60 秒冷却）都在 `send_register_code` 里，重复实现只会漏掉某一层。
pub async fn register_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match state.send_register_code(&email, &rectx.device()).await {
        Ok(sent) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "sent": true,
            "expires_in": sent.expires_in,
            "resend_after": sent.resend_after,
            // 没配 SMTP 时把验证码带出来（仅本地开发；配了 SMTP 恒为 null）
            "debug_code": sent.debug_code,
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/register` — 公开。
///
/// 注册成功**不发会话**：设计稿要求用户注册后回到登录页重新登录，
/// 这样「记住我」「设备标记」这些设置能在登录那一步统一生效。
pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let code = match field_str(&body, "code") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let password = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match state
        .register(&email, &code, &password, &rectx.device())
        .await
    {
        Ok(outcome) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "uid": outcome.user.uid,
            "email": outcome.user.email,
            "quota_bytes": outcome.user.quota_bytes,
            "default_album_id": outcome.default_album_id,
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/login` — 公开。
///
/// `totp` 与 `recovery_code` 二选一：开了二次验证且都没带时，领域层回
/// `TOTP_REQUIRED`（400 + `totp_required: true`），前端据此翻到第二步——
/// 所以这里不能把它当成错误吞掉，原样透传。
pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let password = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let totp = field_opt_str(&body, "totp");
    // 恢复码也接受 `recovery_code` 这个更直白的字段名
    let recovery = field_opt_str(&body, "recovery_code").or_else(|| field_opt_str(&body, "recovery"));
    let remember = super::field_opt_bool(&body, "remember").unwrap_or(false);
    // 二次验证用哪种码由客户端说明：动态码、邮箱验证码都是 6 位数字，
    // 服务端分不清；不说就按动态码验
    let method = field_opt_str(&body, "code_method").unwrap_or_default();

    match state
        .login_with_method(
            &email,
            &password,
            &method,
            totp.as_deref(),
            recovery.as_deref(),
            &rectx.device(),
            remember,
        )
        .await
    {
        Ok(issued) => {
            let cookies = session_cookies(
                &state,
                &issued.session_token,
                &issued.csrf_token,
                remember,
            )
            .await;
            let payload = match crate::routes::me_payload(
                &state,
                &issued.user,
                Some(issued.csrf_token.clone()),
                None,
            )
            .await
            {
                Ok(payload) => payload,
                Err(err) => return error_response(err),
            };
            with_cookies(finish::<serde_json::Value>(Ok(payload)), cookies)
        }
        Err(AppError::TotpRequired) => {
            // 附上 `totp_required` 让前端知道该弹补输框，而不是把消息当成普通错误。
            // 这里手工拼体是因为额外字段要落在 `error` 对象里，而 `error_response`
            // 只透传 `AppError::extra()` 里已定义的字段。
            let body = serde_json::json!({
                "data": serde_json::Value::Null,
                "error": {
                    "code": "TOTP_REQUIRED",
                    "message": "该账号已开启二次验证，请输入动态验证码",
                    "totp_required": true,
                }
            });
            (axum::http::StatusCode::BAD_REQUEST, Json(body)).into_response()
        }
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/login/email-code` ← `{ "email": "…" }` — 公开。
///
/// 登录第二步的「改用邮箱验证码」。不管账号存不存在、开没开二次验证，都回同一句
/// 「已发送」：这条路径最容易被拿来试探账号与二次验证状态，回答必须一致。
pub async fn login_email_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    match state.send_login_code(&email, &rectx.device()).await {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "sent": true,
            "message": "如果该账号开启了二次验证，验证码已发到邮箱",
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/logout`。
///
/// 没有会话也返回成功：登出一定是幂等的——前端在 Cookie 过期后调它，
/// 报 401 只会让用户看到「登出失败」这种没有意义的提示。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    if let Some(token) = session_cookie(&headers) {
        if let Err(err) = state.logout(&token, &rectx.device()).await {
            tracing::warn!(error = %err, "登出时吊销会话失败");
        }
    }
    // Bearer / X-API-Key 也可以调登出：语义是「撤销这个令牌」，
    // 客户端（PicGo 之类）想主动作废自己手上的令牌时用得上。
    if let Some(token) = api_token(&headers) {
        if let Ok((user, record)) = state.authenticate_token(&token).await {
            if let Err(err) = state.repo.revoke_api_token(record.id, user.id).await {
                tracing::warn!(error = %err, "登出时撤销令牌失败");
            }
        }
    }
    // 「切回管理员」的 Cookie 也要一起清掉：它本身就是一个会话凭据，
    // 登出后留着等于留着别人的登录状态。合并用 merge_cookies——
    // HeaderMap::extend 会把同名的 Set-Cookie 替换掉
    let cookies = merge_cookies(clear_cookies(&state), clear_impersonator_cookie(&state));
    with_cookies(
        finish::<serde_json::Value>(Ok(serde_json::json!({ "ok": true }))),
        cookies,
    )
}

/// `POST /api/v1/auth/password/code` — 公开。
///
/// 账号不存在也返回成功（`send_password_reset_code` 里的静默处理）：
/// 否则这个端点就成了「某邮箱注册过没有」的探针。
pub async fn password_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match state
        .send_password_reset_code(&email, &rectx.device())
        .await
    {
        Ok(sent) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "sent": true,
            "expires_in": sent.expires_in,
            "resend_after": sent.resend_after,
            "debug_code": sent.debug_code,
        }))),
        Err(err) => error_response(err),
    }
}

/// `GET /api/v1/auth/password/link?token=…` — 公开。
///
/// 重置页先用它换一句「给哪个账号设密码」。邮箱是打了码的：这一页可能被旁人
/// 瞄到，而链接本身就是凭据，不必把完整地址摊在屏幕上。
pub async fn password_link_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    // 链接令牌是可以被猜错的，按「登录」这一档限流：猜的人多了也要挡住
    if let Err(err) = state
        .ensure_ip_allowed(rectx.ip.as_deref(), pixora_core::ipguard::scope::LOGIN)
        .await
    {
        return error_response(err);
    }
    let token = query.get("token").map(String::as_str).unwrap_or_default();
    match state.reset_link_info(token).await {
        Ok(info) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "email": info.email,
            "expires_at": info.expires_at,
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/password/link` ← `{ "token": "pxr_…", "password": "…" }` — 公开。
///
/// 点邮件里的链接设新密码。令牌用一次就废；同样会踢掉全部会话并清 Cookie。
pub async fn password_link_reset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let token = match field_str(&body, "token") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let password = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if password.len() > 200 {
        return bad_request("密码过长");
    }

    match state
        .reset_password_by_token(&token, &password, &rectx.device())
        .await
    {
        Ok(email) => with_cookies(
            finish::<serde_json::Value>(Ok(serde_json::json!({ "ok": true, "email": email }))),
            clear_cookies(&state),
        ),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/password/reset` — 公开。
///
/// 成功后领域层会吊销该用户全部会话；这里顺手清掉当前浏览器上的 Cookie，
/// 免得前端拿着一个已经失效的会话继续发请求。
pub async fn password_reset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let code = match field_str(&body, "code") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let password = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if password.len() > 200 {
        return bad_request("密码过长");
    }

    match state
        .reset_password(&email, &code, &password, &rectx.device())
        .await
    {
        Ok(()) => with_cookies(
            finish::<serde_json::Value>(Ok(serde_json::json!({ "ok": true }))),
            clear_cookies(&state),
        ),
        Err(err) => error_response(err),
    }
}
