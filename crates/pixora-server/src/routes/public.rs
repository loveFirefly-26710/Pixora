//! 公开端点：站点信息、广场、条款、健康检查。
//!
//! 这一组的共同点是**未登录也要能用**：导航栏、登录页、注册页、直链前缀
//! 都靠 `/site` 渲染，广场与条款页是未注册用户的入口。所以这里一律走
//! `authenticate_optional`——带着有效会话时能给出「当前用户视角」的字段
//! （比如 `terms.required`），不带会话时给游客视角，但**绝不因为没登录就报错**。
//!
//! 唯一的例外是 `POST /terms/consent`：同意条款是「记录在案」的动作，
//! 没有账号就无处可记，所以它必须登录。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use pixora_core::AppState;
use pixora_core::AppError;
use pixora_repo::repo::images::ImageFilter;
use pixora_repo::model::TermsVersion;

use crate::http::{
    error_response, finish, finish_page, parse_i64, parse_str,
    request_context, rfc3339, Authenticated,
};

use super::{bad_request, field_i64, PeerAddr};

/// `GET /api/v1/site` — 公开。
///
/// 一次给全「渲染首屏需要知道的一切」：站点名、直链前缀、注册开关、
/// 上传限制、缩略图策略、条款状态。把这些拆成多个端点会让首屏多几个往返，
/// 而且站点名这类字段会在两次请求之间被管理员改掉，出现半新半旧的页面。
pub async fn site(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    // 未登录是正常路径，不是错误；只有「带着坏凭据」才报错
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };

    let site = match state
        .settings
        .site(state.config.base_url.as_deref())
        .await
    {
        Ok(site) => site,
        Err(err) => return crate::http::error_response(err),
    };
    let upload = match state.settings.upload().await {
        Ok(upload) => upload,
        Err(err) => return crate::http::error_response(err),
    };
    let mail = match state.settings.mail().await {
        Ok(mail) => mail,
        Err(err) => return crate::http::error_response(err),
    };
    let (registration_open, registration_closed_reason) = match state.register_open().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let installed = state.settings.is_installed().await.unwrap_or(false);
    // 背景图可能是上传进库的那张：这里统一翻译成可访问的地址
    let background = match state.settings.background_public_url().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let terms = match terms_summary(&state, auth.as_ref()).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "name": site.name,
        "base_url": state.base_url().await,
        "upload_prefix": site.upload_prefix,
        "plaza_grid_columns": site.plaza_grid_columns,
        "recycle_retention_days": state
            .settings
            .retention_days()
            .await
            .unwrap_or(0),
        "registration_open": registration_open,
        "registration_closed_reason": registration_closed_reason,
        "plaza_public": site.plaza_public,
        "formats": upload.formats,
        "max_file_bytes": upload.max_file_bytes,
        "max_batch_size": upload.max_batch,
        "max_pixels": upload.max_pixels,
        "max_width": upload.max_width,
        "max_height": upload.max_height,
        // 队列深度：客户端队列据此限长（管理员也一样受它约束）
        "queue_depth": upload.queue_depth,
        "thumb_client": upload.thumb_client,
        "thumb_server_fallback": upload.thumb_server_fallback,
        "appeal_email": mail.appeal_email,
        "background": background,
        // 会话有效期：界面上要写清楚「多久不动会掉线」，这不是秘密
        "session": {
            "idle_minutes": state.settings.session().await.map(|s| s.idle_minutes).unwrap_or(0),
            "absolute_days": state.settings.session().await.map(|s| s.absolute_days).unwrap_or(0),
        },
        "installed": installed,
        "terms": terms,
    })))
}

/// 条款摘要：`required` 只对「带着有效会话」的请求有意义。
///
/// 判定口径：**同意记录以版本为单位**。生效版本存在但没有该用户的同意行，
/// 就是「必须先同意」。未登录恒为 `false`——游客还没法同意任何东西，
/// 报 `true` 只会让登录页显示一个点了没用的弹窗。
async fn terms_summary(
    state: &Arc<AppState>,
    auth: Option<&Authenticated>,
) -> pixora_core::Result<serde_json::Value> {
    let active = state.repo.active_terms().await?;
    let Some(active) = active.as_ref() else {
        // 实例还没发布过条款：不该拦人，版本号与条数都给空值
        return Ok(serde_json::json!({
            "required": false,
            "version": serde_json::Value::Null,
            "articles": 0,
        }));
    };

    let required = match auth {
        None => false,
        Some(auth) => state
            .repo
            .find_consent(auth.user.id, active.id)
            .await?
            .is_none(),
    };

    Ok(serde_json::json!({
        "required": required,
        "version": active.version,
        "articles": active.article_count(),
    }))
}

/// `GET /api/v1/plaza` — 公开。
///
/// 关闭广场公开浏览后未登录调用返回 `FORBIDDEN`：这不是「内容为空」，
/// 前端要区分这两种状态（一个是「暂无图片」的空状态，一个是「登录后才能看」）。
pub async fn plaza(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };
    let site = match state
        .settings
        .site(state.config.base_url.as_deref())
        .await
    {
        Ok(site) => site,
        Err(err) => return crate::http::error_response(err),
    };
    if !site.plaza_public && auth.is_none() {
        return crate::http::error_response(AppError::Forbidden(
            "站点已关闭广场公开浏览，登录后可以查看".into(),
        ));
    }

    finish_page(
        state
            .list_plaza(
                parse_str(&query, "q"),
                parse_str(&query, "tag"),
                parse_str(&query, "sort"),
                parse_i64(&query, "seed").unwrap_or(0).max(0) as u64,
                parse_i64(&query, "page"),
                parse_i64(&query, "per_page"),
            )
            .await,
    )
}

/// `GET /api/v1/site/background` — 公开。站点背景图本体（上传进库的那张）。
///
/// 地址里带 `?v=<更新时间>`，所以可以放心长缓存：换图之后 URL 就变了。
/// 没有上传过背景图时返回 404，前端会退回无背景图的样子。
pub async fn site_background(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let asset = match state
        .repo
        .get_asset(pixora_core::settings::keys::BACKGROUND_ASSET_KEY)
        .await
    {
        Ok(Some(asset)) => asset,
        Ok(None) => return crate::http::error_response(AppError::not_found("背景图")),
        Err(err) => return crate::http::error_response(err.into()),
    };
    let (bytes, version) = asset;

    let etag = format!("\"{}\"", pixora_crypto::hash::sha256_hex(&bytes));
    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == etag)
        .unwrap_or(false)
    {
        let mut response = axum::http::StatusCode::NOT_MODIFIED.into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&etag) {
            response.headers_mut().insert(axum::http::header::ETAG, value);
        }
        return response;
    }

    let mime = pixora_core::media::sniff(&bytes)
        .map(|item| item.mime)
        .unwrap_or("application/octet-stream");
    let mut response = (axum::http::StatusCode::OK, bytes).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(mime) {
        response.headers_mut().insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(axum::http::header::ETAG, value);
    }
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=86400"),
    );
    response.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    let _ = version;
    response
}

/// `GET /api/v1/plaza/tags` — 公开。广场顶部的标签 chips。
pub async fn plaza_tags(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };
    let site = match state
        .settings
        .site(state.config.base_url.as_deref())
        .await
    {
        Ok(site) => site,
        Err(err) => return crate::http::error_response(err),
    };
    if !site.plaza_public && auth.is_none() {
        return crate::http::error_response(AppError::Forbidden(
            "站点已关闭广场公开浏览，登录后可以查看".into(),
        ));
    }

    let limit = parse_i64(&query, "limit").unwrap_or(50).clamp(1, 200);
    match state.repo.plaza_tags(limit).await {
        Ok(tags) => finish(Ok(tag_list(tags))),
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// 标签的统一形状：`{ name, usage }`。
fn tag_list(tags: Vec<pixora_repo::model::Tag>) -> Vec<serde_json::Value> {
    tags.into_iter()
        .map(|tag| serde_json::json!({ "name": tag.name, "usage": tag.usage }))
        .collect()
}

/// `GET /api/v1/terms` — 公开（带会话时多回「我同意了没」）。
pub async fn terms(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };

    let active = match state.repo.active_terms().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };

    // 同意记录按版本取；未登录就只有 current 与历史
    let consents = match auth.as_ref() {
        Some(auth) => match state.repo.list_consents(auth.user.id).await {
            Ok(value) => value,
            Err(err) => return crate::http::error_response(err.into()),
        },
        None => Vec::new(),
    };
    let agreed_ids: std::collections::HashSet<i64> =
        consents.iter().map(|item| item.version_id).collect();

    let consent = consents
        .iter()
        .find(|item| active.as_ref().map(|active| active.id) == Some(item.version_id))
        .map(|item| {
            serde_json::json!({
                "agreed_at": rfc3339(item.agreed_at),
                "ip": item.ip,
                "version": item.version,
            })
        });

    let history = match state.repo.list_terms().await {
        Ok(rows) => rows,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let history: Vec<serde_json::Value> = history
        .into_iter()
        // 草稿不该出现在前台的「历史版本」里：它还没对外过一次
        .filter(|row| row.version.status != pixora_repo::model::terms_status::DRAFT)
        .map(|row| {
            serde_json::json!({
                "id": row.version.id,
                "version": row.version.version,
                "summary": row.version.summary,
                "effective_at": row.version.effective_at.map(rfc3339),
                "status": row.version.status,
                "agreed": agreed_ids.contains(&row.version.id),
            })
        })
        .collect();

    // 没有生效版本就等于「没有可同意的对象」；有生效版本才看同意记录
    let required = match (auth.as_ref(), active.as_ref()) {
        (None, _) | (_, None) => false,
        (Some(_), Some(active)) => !agreed_ids.contains(&active.id),
    };

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "current": active.as_ref().map(terms_body),
        "consent": consent,
        "required": required,
        "history": history,
    })))
}

/// 条款正文的对外形状。
fn terms_body(version: &TermsVersion) -> serde_json::Value {
    serde_json::json!({
        "id": version.id,
        "version": version.version,
        "title": version.title,
        "body": version.body,
        "summary": version.summary,
        "articles": version.article_count(),
        "effective_at": version.effective_at.map(rfc3339),
        "published_at": version.published_at.map(rfc3339),
    })
}

/// `POST /api/v1/terms/consent` — 需要登录。
///
/// 只增不改：同意记录里冗余存一份版本号，条款日后被删也查得到「当初同意的是哪一版」。
/// 提交一个还没有发布的版本号是无效操作（用户不可能看到草稿），直接拒绝。
pub async fn terms_consent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: axum::http::Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match super::require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let version_id = match field_i64(&body, "version_id") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let version = match state.repo.find_terms(version_id).await {
        Ok(Some(version)) => version,
        Ok(None) => return super::not_found("条款版本"),
        Err(err) => return crate::http::error_response(err.into()),
    };
    if !version.is_active() {
        return bad_request("该条款版本当前未生效");
    }

    let now = pixora_repo::model::now();
    if let Err(err) = state
        .repo
        .record_consent(
            auth.user.id,
            &version,
            rectx.ip.as_deref(),
            rectx.user_agent.as_deref(),
            now,
        )
        .await
    {
        return crate::http::error_response(err.into());
    }

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "agreed_at": rfc3339(now),
        "csrf_token": crate::http::cookie_value(&headers, crate::http::CSRF_COOKIE),
    })))
}

/// `GET /healthz` — 探活。
///
/// 刻意不套信封：探针（Caddy、K8s、uptime 机器人）只想知道进程活着、库连得上，
/// 给它一层 `{data, error}` 反而要每个探针各写一遍解包逻辑。
/// 库不通时也回 200 但把 `db` 写成错误——探针按字段判断，比按状态码更清楚。
///
/// 注意：`app.rs` 目前把 `/healthz` 接到了 `direct::healthz`（它把这份载荷
/// 包进了信封，与契约第 11 章的「裸对象」形状不一致）。这里保留契约形状的
/// 实现，两处需要合成一处时以本函数的返回体为准。
#[allow(dead_code)]
pub async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let db_ok = state.repo.db().ping().await.is_ok();
    let uptime = (chrono::Utc::now() - state.started_at).num_seconds().max(0);
    let payload = serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "db": if db_ok { "ok" } else { "error" },
        "uptime_seconds": uptime,
    });
    (axum::http::StatusCode::OK, axum::Json(payload)).into_response()
}

/// `GET /api/v1/random` — 随机取一张公开图片。
///
/// 默认 302 跳到直链，这样 `<img src="/api/v1/random">` 直接能用：浏览器自己去
/// 取图，服务端不参与传字节。加 `?format=json` 则回一个 JSON（含直链），
/// 方便脚本里先拿地址再决定怎么用。
///
/// 支持的筛选与广场一致：`tag`（精确标签）、`q`（关键词）、`uid`（作者）。
/// 没有符合条件的图片时回 404，而不是随机糊一张不相关的图给用户。
pub async fn random_image(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    // 广场关闭公开浏览时，随机图也一并关掉，免得成为绕过开关的后门
    let site = match state.settings.site(state.config.base_url.as_deref()).await {
        Ok(site) => site,
        Err(err) => return error_response(err),
    };
    if !site.plaza_public {
        if let Err(err) = crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
            return error_response(err);
        }
    }

    let filter = ImageFilter {
        public_only: true,
        tag: params.get("tag").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
        query: params.get("q").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()),
        owner_id: match params.get("uid") {
            Some(uid) => match state.repo.find_user_by_uid(uid.trim()).await {
                Ok(Some(user)) => Some(user.id),
                Ok(None) => return error_response(AppError::not_found("用户")),
                Err(err) => return error_response(err.into()),
            },
            None => None,
        },
        ..Default::default()
    };

    let picked = match state.repo.random_image(&filter).await {
        Ok(Some(view)) => view,
        Ok(None) => return error_response(AppError::not_found("图片")),
        Err(err) => return error_response(err.into()),
    };
    let url = state.direct_url(&picked.sha256, &picked.ext, false).await;
    let thumb = state.direct_url(&picked.sha256, &picked.ext, true).await;

    if params.get("format").map(|v| v == "json").unwrap_or(false) {
        return finish(Ok(serde_json::json!({
            "url": url,
            "thumb_url": thumb,
            "id": picked.id,
            "width": picked.width,
            "height": picked.height,
            "size": picked.size,
            "ext": picked.ext,
            "tags": picked.tags,
            "owner_uid": picked.owner_uid,
        })));
    }

    // 302 而不是 301：随机结果不该被永久缓存下来。
    // 不用 `Redirect::temporary`——它是 307，虽然这里只有 GET、语义上都对，
    // 但契约文档与客户端都按「302 跳转」理解，让实现和文档一致更省事。
    let mut response = (
        StatusCode::FOUND,
        [(header::LOCATION, url)],
    )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
