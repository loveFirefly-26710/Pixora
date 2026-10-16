//! 第三方登录与登录方式管理的路由。
//!
//! 三条路：
//!   - `GET /auth/oauth/{provider}/start`：带 `state` 跳去对方站点（登录或绑定）
//!   - `GET /auth/oauth/{provider}/callback`：对方带着授权码回来，换会话或补邮箱
//!   - `/me/identities`：已登录的人看自己挂了哪些方式、绑定与解绑
//!
//! 回调是浏览器整页跳转过来的，所以这里直接 302 回前端路由，而不是回 JSON；
//! 出错也跳回前端并把原因带上（`?oauth_error=`），让登录页把它显示出来——
//! 把用户丢在一个 JSON 报错页上是最没用的做法。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use pixora_core::identities::OAuthOutcome;
use pixora_core::{AppError, AppState};
use pixora_repo::repo::identities::provider;

use crate::http::{
    cookie_value, error_response, finish, request_context, session_cookies, with_cookies,
    CSRF_COOKIE,
};

use super::{bad_request, field_str, require_user, PeerAddr};

/// 回调地址：站点自己的地址 + 固定路径。必须与在对方平台登记的完全一致。
pub(crate) fn redirect_uri(state: &AppState, provider_name: &str) -> String {
    let base = state
        .config
        .base_url
        .clone()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    format!("{base}/api/v1/auth/oauth/{provider_name}/callback")
}

/// `GET /api/v1/auth/oauth/{provider}/start` — 跳去对方站点。
///
/// `mode=bind` 时必须已经登录：绑定是「把新方式挂到现有账号上」，没有账号就无从挂起。
pub async fn oauth_start(
    State(state): State<Arc<AppState>>,
    Path(provider_name): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    peer: PeerAddr,
    headers: HeaderMap,
) -> Response {
    let provider_name = provider_name.to_ascii_lowercase();
    let mode = query
        .get("mode")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "login".to_string());
    let next = query.get("next").cloned().unwrap_or_else(|| "/".to_string());

    let Some(client) = (match state.get_oauth_client(&provider_name).await {
        Ok(client) => client,
        Err(err) => return error_response(err),
    }) else {
        return error_response(AppError::Config(format!(
            "{} 登录还没配置好：管理员要在「系统设置 → 登录方式」里填上客户端凭据并启用",
            provider::label(&provider_name)
        )));
    };
    if !state
        .settings
        .bool(&format!("login.{provider_name}.enabled"), false)
        .await
        .unwrap_or(false)
    {
        return error_response(AppError::Config(format!(
            "{} 登录没有启用",
            provider::label(&provider_name)
        )));
    }

    // 绑定模式下先认人：拿不到会话就直接劝回登录页，别让对方白跑一趟授权
    let mut bind_user_id = None;
    if mode == "bind" {
        let rectx = request_context(&state, &headers, peer.addr()).await;
        let auth = match crate::http::authenticate(&state, &headers, rectx.ip.as_deref()).await {
            Ok(auth) => auth,
            Err(err) => return error_response(err),
        };
        bind_user_id = Some(auth.user.id);
    }

    let oauth_state = match state.new_state(&provider_name, &mode, &next, bind_user_id) {
        Ok(value) => value,
        Err(err) => return error_response(err),
    };
    let signed = match state.sign_state(&oauth_state) {
        Ok(value) => value,
        Err(err) => return error_response(err),
    };
    let url = state.authorize_url(&client, &redirect_uri(&state, &provider_name), &signed);
    if url.is_empty() {
        return error_response(AppError::bad_request(format!(
            "不支持的登录方式：{provider_name}"
        )));
    }
    redirect(&url)
}

/// `GET /api/v1/auth/oauth/{provider}/callback` — 对方带着授权码回来。
pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Path(provider_name): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    peer: PeerAddr,
    headers: HeaderMap,
) -> Response {
    let provider_name = provider_name.to_ascii_lowercase();

    // 对方拒绝授权（用户点了取消）时带 `error` 回来，这不是故障，如实说一句
    if let Some(error) = query.get("error") {
        let detail = query
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return fail_redirect(&format!("{}授权被拒绝：{detail}", provider::label(&provider_name)));
    }

    let state_token = query.get("state").cloned().unwrap_or_default();
    let oauth_state = match state.verify_state(&state_token) {
        Ok(value) => value,
        Err(err) => return fail_redirect(err.message().as_str()),
    };
    if oauth_state.provider != provider_name {
        return fail_redirect("登录状态对不上，请重新发起");
    }
    let Some(code) = query.get("code").cloned() else {
        return fail_redirect("对方没有带回授权码，请重试");
    };

    let rectx = request_context(&state, &headers, peer.addr()).await;
    let device = rectx.device();

    let client = match state.get_oauth_client(&provider_name).await {
        Ok(Some(client)) => client,
        Ok(None) => return fail_redirect("这个登录方式没有配置好"),
        Err(err) => return fail_redirect(err.message().as_str()),
    };
    let bundle = match state
        .oauth_token_bundle(&client, &code, &redirect_uri(&state, &provider_name))
        .await
    {
        Ok(bundle) => bundle,
        Err(err) => return fail_redirect(err.message().as_str()),
    };
    let profile = match state.oauth_profile(&client, &bundle).await {
        Ok(profile) => profile,
        Err(err) => return fail_redirect(err.message().as_str()),
    };

    // 绑定：把身份挂到发起时那个账号上（会话可能已经过期，所以再核一次身份）
    if oauth_state.mode == "bind" {
        let auth = match crate::http::authenticate(&state, &headers, rectx.ip.as_deref()).await {
            Ok(auth) => auth,
            Err(_) => return fail_redirect("登录状态已失效，请重新登录后再绑定"),
        };
        if let Some(expected) = oauth_state.user_id {
            if expected != auth.user.id {
                return fail_redirect("绑定的账号与发起时不一致，请重试");
            }
        }
        return match state
            .bind_identity(&auth.user, &provider_name, &profile, rectx.ip.as_deref())
            .await
        {
            Ok(()) => redirect(&format!(
                "/#/account/security?bound={provider_name}"
            )),
            Err(err) => fail_redirect(err.message().as_str()),
        };
    }

    match state
        .oauth_login(&provider_name, profile, &device, false)
        .await
    {
        Ok(OAuthOutcome::LoggedIn(issued)) => {
            let cookies = session_cookies(&state, &issued.session_token, &issued.csrf_token, false).await;
            let target = if oauth_state.next.is_empty() {
                "/".to_string()
            } else {
                oauth_state.next
            };
            with_cookies(redirect(&target), cookies)
        }
        // 没拿到邮箱：回前端补一个，令牌背着「哪个平台的哪个身份」
        Ok(OAuthOutcome::NeedEmail(ticket)) => redirect(&format!(
            "/#/complete-signup?ticket={}&provider={provider_name}",
            urlencode(&ticket)
        )),
        Err(err) => fail_redirect(err.message().as_str()),
    }
}

/// `POST /api/v1/auth/oauth/complete` — OAuth 建号时补邮箱。
///
/// 对方没给邮箱（QQ、微信都不给，GitHub 也可能不给），只能让用户自己填一个，
/// 并且照样走注册验证码：邮箱是账号本身，不能凭一句话就当成他的。
pub async fn complete_signup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let ticket = match field_str(&body, "ticket") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let code = match field_str(&body, "code") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let _ = method;

    match state
        .complete_oauth_signup(&ticket, &email, &code, &rectx.device())
        .await
    {
        Ok(issued) => {
            let cookies = session_cookies(&state, &issued.session_token, &issued.csrf_token, false).await;
            let payload = match super::me_payload(&state, &issued.user, Some(issued.csrf_token), None).await {
                Ok(payload) => payload,
                Err(err) => return error_response(err),
            };
            with_cookies(finish::<serde_json::Value>(Ok(payload)), cookies)
        }
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/phone/code` — 发一条手机验证码（登录用）。
pub async fn phone_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let phone = match field_str(&body, "phone") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let _ = method;
    match state
        .send_phone_code(
            &phone,
            &rectx.device(),
            pixora_repo::repo::verifications::purpose::PHONE_LOGIN,
        )
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "message": "验证码已发送，10 分钟内有效",
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/auth/phone/login` — 手机号 + 验证码登录。
pub async fn phone_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let phone = match field_str(&body, "phone") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let code = match field_str(&body, "code") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let remember = body
        .get("remember")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let _ = method;

    match state.phone_login(&phone, &code, &rectx.device(), remember).await {
        Ok(issued) => {
            let cookies =
                session_cookies(&state, &issued.session_token, &issued.csrf_token, remember).await;
            let payload = match super::me_payload(&state, &issued.user, Some(issued.csrf_token), None).await {
                Ok(payload) => payload,
                Err(err) => return error_response(err),
            };
            with_cookies(finish::<serde_json::Value>(Ok(payload)), cookies)
        }
        Err(err) => error_response(err),
    }
}

/// `GET /api/v1/me/identities` — 我挂了哪些登录方式。
///
/// 邮箱与密码也一并列出来：它们是「必需的那一个」和「随时能设的那一个」，
/// 界面上一整排方式要一起呈现，缺了它们反而看不清「还能不能进得来」。
pub async fn identities_get(
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
    let list = match state.identities_of(&auth.user).await {
        Ok(list) => list,
        Err(err) => return error_response(err),
    };
    let methods = state.login_methods().await.unwrap_or_default();
    let available = serde_json::json!({
        "github": methods.github,
        "google": methods.google,
        "qq": methods.qq,
        "wechat": methods.wechat,
        "phone": methods.phone,
    });
    finish::<serde_json::Value>(Ok(serde_json::json!({
        "email": auth.user.email,
        "password_set": auth.user.password_hash.trim() != "!",
        "identities": list,
        "available": available,
    })))
}

/// `POST /api/v1/me/identities` — 绑定手机号（要验证码）。
///
/// 第三方方式的绑定走 OAuth（`/auth/oauth/{provider}/start?mode=bind`）：
/// 那一步必须由用户本人到对方站点上确认，不能在这里凭一句话就挂上。
pub async fn identity_bind(
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
    let provider_name = match field_str(&body, "provider") {
        Ok(value) => value.to_ascii_lowercase(),
        Err(resp) => return resp,
    };
    if provider_name != provider::PHONE {
        return bad_request(format!(
            "{label} 的绑定要走授权页：点「绑定」按钮，到对方站点确认一次",
            label = provider::label(&provider_name)
        ));
    }
    let phone = match field_str(&body, "phone") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let code = match field_str(&body, "code") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    match state
        .bind_phone(&auth.user, &phone, &code, rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/me/identities/phone/code` — 绑手机号前先要一条验证码。
pub async fn phone_bind_code(
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
    let phone = match field_str(&body, "phone") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    match state
        .send_phone_code(
            &phone,
            &rectx.device(),
            pixora_repo::repo::verifications::purpose::PHONE_BIND,
        )
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "message": "验证码已发送，10 分钟内有效",
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => error_response(err),
    }
}

/// `DELETE /api/v1/me/identities/{provider}` — 解绑一个登录方式。
pub async fn identity_unbind(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(provider_name): Path<String>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }
    let provider_name = provider_name.to_ascii_lowercase();
    match state
        .unbind_identity(&auth.user, &provider_name, rectx.ip.as_deref())
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => error_response(err),
    }
}

/// 302 跳转。
fn redirect(target: &str) -> Response {
    match axum::response::Redirect::to(target).into_response().status() {
        StatusCode::SEE_OTHER | StatusCode::TEMPORARY_REDIRECT | StatusCode::FOUND => {
            axum::response::Redirect::to(target).into_response()
        }
        _ => {
            let mut response = StatusCode::FOUND.into_response();
            if let Ok(value) = axum::http::HeaderValue::from_str(target) {
                response.headers_mut().insert(header::LOCATION, value);
            }
            response
        }
    }
}

/// 回调出错：跳回登录页并带上原因，让人看得见发生了什么。
fn fail_redirect(message: &str) -> Response {
    redirect(&format!("/#/login?oauth_error={}", urlencode(message)))
}

/// 最小百分号编码（路径与查询值共用）。
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

