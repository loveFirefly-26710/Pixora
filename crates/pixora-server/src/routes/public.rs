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
use pixora_core::AppError;
use pixora_core::AppState;
use pixora_repo::model::TermsVersion;
use pixora_repo::repo::images::RandomFilter;

use crate::http::{
    error_response, finish, finish_page, parse_i64, parse_str, request_context, rfc3339,
    Authenticated,
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
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await
    {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };

    // 游客上传要带 CSRF 双提交头，而访客没有会话（也就没有令牌）：这里补发一个
    // ——它本身不授权任何东西，只是「同一个浏览器」的凭据
    let guest_limits = match (auth.is_none(), state.guest_uploader().await) {
        (true, Ok(Some(user))) => state.effective_upload_limits(&user).await.ok(),
        _ => None,
    };
    let csrf_cookie = match (&auth, &guest_limits) {
        (None, Some(_))
            if crate::http::cookie_value(&headers, crate::http::CSRF_COOKIE).is_none() =>
        {
            pixora_crypto::token::new_csrf_token()
                .ok()
                .map(|token| crate::http::csrf_cookie(&state, &token))
        }
        _ => None,
    };

    let site = match state.settings.site(state.config.base_url.as_deref()).await {
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
    // 站点默认头像：没设过头像的账号（含新注册的）用它，没配过就是 null
    let default_avatar = match state.settings.default_avatar_url().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let terms = match terms_summary(&state, auth.as_ref()).await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let music = match state.settings.music().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };

    let response = finish::<serde_json::Value>(Ok(serde_json::json!({
        "name": site.name,
        "base_url": state.base_url().await,
        "upload_prefix": site.upload_prefix,
        "plaza_grid_columns": site.plaza_grid_columns,
        "plaza_auto_refresh_seconds": site.plaza_auto_refresh_seconds,
        // 前端据此决定要不要显示顶部播放按钮；歌单内容走 /music/playlist
        "music": {
            "enabled": music.enabled,
            "source": music.source,
            "autoplay": music.autoplay,
        },
        "recycle_retention_days": state
            .settings
            .retention_days()
            .await
            .unwrap_or(0),
        "registration_open": registration_open,
        "registration_closed_reason": registration_closed_reason,
        // 注册邮箱白名单：注册页据此提前说明「只收这些域名」，
        // 免得用户填完邮箱、点了发码才知道不行
        "register_email_domains": site.email_domains.domains(),
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
        // 站点默认头像：账号自己没设头像时前端回落到它，再没有就回落到内置翼标
        "default_avatar": default_avatar,
        // 本站支持哪些登录方式：登录页/注册页照着它渲染按钮，
        // 没启用的（或凭据没配全的）不出现——摆一个点了报错的按钮最糟
        "login_methods": state.login_methods().await.unwrap_or_default(),
        // 会话有效期：界面上要写清楚「多久不动会掉线」，这不是秘密
        "session": {
            "idle_minutes": state.settings.session().await.map(|s| s.idle_minutes).unwrap_or(0),
            "absolute_days": state.settings.session().await.map(|s| s.absolute_days).unwrap_or(0),
        },
        "installed": installed,
        "terms": terms,
        // 未登录也能上传时，前端照着访客账号那套额度限流：队列深度、单批张数、
        // 并发都按它来，不然本地排了一堆、服务端一件件拒
        "guest_upload": guest_limits.as_ref().map(|limits| {
            serde_json::json!({
                "enabled": true,
                "max_file_bytes": limits.max_file_bytes,
                "max_batch_size": limits.max_batch,
                "max_concurrent_uploads": limits.max_concurrent_uploads,
                "queue_depth": upload.queue_depth,
                "formats": limits.formats,
            })
        }),
    })));

    // 上面那份 body 是 finish() 拼好的，这里只需要再挂一个 Set-Cookie
    match csrf_cookie {
        Some(cookie) => crate::http::with_cookies(response, crate::http::cookie_headers(cookie)),
        None => response,
    }
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
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await
    {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };
    let site = match state.settings.site(state.config.base_url.as_deref()).await {
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
                &site.plaza_album_ids,
            )
            .await,
    )
}

/// `GET /api/v1/site/avatar` — 公开。站点默认头像本体（上传进库的那张）。
///
/// 与背景图同一条路：地址里带 `?v=<更新时间>`，可以放心长缓存。没上传过时
/// 返回 404，前端回落到内置翼标头像。
pub async fn site_avatar(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    serve_site_asset(
        &state,
        &headers,
        pixora_core::settings::keys::AVATAR_ASSET_KEY,
        "默认头像",
    )
    .await
}

/// 站点资产（背景图 / 默认头像）共用的发法：ETag + 一天缓存 + nosniff。
async fn serve_site_asset(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    asset_key: &str,
    label: &str,
) -> Response {
    let asset = match state.repo.get_asset(asset_key).await {
        Ok(Some(asset)) => asset,
        Ok(None) => return crate::http::error_response(AppError::not_found(label)),
        Err(err) => return crate::http::error_response(err.into()),
    };
    let (bytes, version) = asset;
    let _ = version;

    let etag = format!("\"{}\"", pixora_crypto::hash::sha256_hex(&bytes));
    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == etag)
        .unwrap_or(false)
    {
        let mut response = axum::http::StatusCode::NOT_MODIFIED.into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&etag) {
            response
                .headers_mut()
                .insert(axum::http::header::ETAG, value);
        }
        return response;
    }

    let mime = pixora_core::media::sniff(&bytes)
        .map(|item| item.mime)
        .unwrap_or("application/octet-stream");
    let mut response = (axum::http::StatusCode::OK, bytes).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(mime) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&etag) {
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, value);
    }
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=86400"),
    );
    response.headers_mut().insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    if mime == "image/svg+xml" {
        // 站点资产上传时已经净化过，这里再补一层沙箱：万一净化有漏网，
        // 直接打开 `/site/background` 或 `/site/avatar` 也不会执行脚本。
        response.headers_mut().insert(
            "Content-Security-Policy",
            axum::http::HeaderValue::from_static(
                "sandbox; default-src 'none'; script-src 'none'; object-src 'none'; \
                 img-src data:; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        );
    }
    response
}

/// `GET /api/v1/site/background` — 公开。站点背景图本体（上传进库的那张）。
///
/// 地址里带 `?v=<更新时间>`，所以可以放心长缓存：换图之后 URL 就变了。
/// 没有上传过背景图时返回 404，前端会退回无背景图的样子。
pub async fn site_background(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    serve_site_asset(
        &state,
        &headers,
        pixora_core::settings::keys::BACKGROUND_ASSET_KEY,
        "背景图",
    )
    .await
}

/// `GET /api/v1/terms` — 公开（带会话时多回「我同意了没」）。
pub async fn terms(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await
    {
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
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await
    {
        Ok(auth) => auth,
        Err(err) => return error_response(err),
    };
    let viewer = auth.as_ref().map(|auth| &auth.user);
    // 广场关闭公开浏览时，随机图也一并关掉，免得成为绕过开关的后门
    let site = match state.settings.site(state.config.base_url.as_deref()).await {
        Ok(site) => site,
        Err(err) => return error_response(err),
    };
    if !site.plaza_public && viewer.is_none() {
        return error_response(AppError::Forbidden(
            "站点已关闭广场公开浏览，登录后可以使用随机图".into(),
        ));
    }

    let param = |key: &str| {
        params
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    };
    let scope = param("scope").unwrap_or("public").to_ascii_lowercase();
    if !matches!(scope.as_str(), "public" | "plaza" | "mine" | "user") {
        return error_response(AppError::bad_request(
            "scope 只接受 public / plaza / mine / user",
        ));
    }

    let mut filter = RandomFilter::default();
    let mut public_only = true;

    // 先用 uid 定下「看谁的图」；scope=mine / user 会进一步覆盖它。
    if let Some(uid) = param("uid") {
        let target = match state.repo.find_user_by_uid(uid).await {
            Ok(Some(user)) => user,
            Ok(None) => return error_response(AppError::not_found("用户")),
            Err(err) => return error_response(err.into()),
        };
        filter.owner_id = Some(target.id);
    }

    match scope.as_str() {
        "plaza" => {
            if !site.plaza_album_ids.is_empty() {
                let allowed = match state.repo.list_public_albums().await {
                    Ok(albums) => albums.into_iter().map(|album| album.id).collect::<std::collections::HashSet<_>>(),
                    Err(err) => return error_response(err.into()),
                };
                filter.album_ids = site
                    .plaza_album_ids
                    .iter()
                    .copied()
                    .filter(|id| allowed.contains(id))
                    .collect();
                if filter.album_ids.is_empty() {
                    return error_response(AppError::not_found("图片"));
                }
            }
        }
        "mine" => {
            let Some(user) = viewer else {
                return error_response(AppError::Unauthorized);
            };
            filter.owner_id = Some(user.id);
            public_only = false;
        }
        "user" => {
            let Some(user) = viewer else {
                return error_response(AppError::Unauthorized);
            };
            let Some(uid) = param("uid") else {
                return error_response(AppError::bad_request("scope=user 需要 uid"));
            };
            let target = match state.repo.find_user_by_uid(uid).await {
                Ok(Some(target)) => target,
                Ok(None) => return error_response(AppError::not_found("用户")),
                Err(err) => return error_response(err.into()),
            };
            if target.id != user.id && !user.is_admin() {
                return error_response(AppError::forbidden("只能查看自己的随机图"));
            }
            filter.owner_id = Some(target.id);
            public_only = false;
        }
        _ => {}
    }

    if let Some(value) = param("visibility") {
        match value.to_ascii_lowercase().as_str() {
            "public" => filter.visibility = Some("public".into()),
            "private" => {
                let Some(owner) = filter.owner_id else {
                    return error_response(AppError::forbidden(
                        "私有图需要带 Token，并用 scope=mine 或指定相册",
                    ));
                };
                if !viewer.map(|user| user.id == owner || user.is_admin()).unwrap_or(false) {
                    return error_response(AppError::forbidden("不能读取别人的私有图"));
                }
                filter.visibility = Some("private".into());
                public_only = false;
            }
            "any" => {
                let authorized = filter.owner_id.map(|owner| {
                    viewer.map(|user| user.id == owner || user.is_admin()).unwrap_or(false)
                }).unwrap_or(false);
                if !authorized {
                    return error_response(AppError::forbidden(
                        "读取私有图需要 Token，并用 scope=mine 或指定自己的相册",
                    ));
                }
                public_only = false;
            }
            _ => return error_response(AppError::bad_request("visibility 只接受 public / private / any")),
        }
    }

    // 指定相册：album_id 直接查；album 是当前 owner 名下的 slug。
    let album_id = parse_i64(&params, "album_id").filter(|id| *id > 0);
    let album_slug = param("album");
    if album_id.is_some() || album_slug.is_some() {
        let album = if let Some(id) = album_id {
            match state.repo.find_album_by_id(id).await {
                Ok(Some(album)) => album,
                Ok(None) => return error_response(AppError::not_found("相册")),
                Err(err) => return error_response(err.into()),
            }
        } else {
            let Some(owner_id) = filter
                .owner_id
                .or_else(|| viewer.map(|user| user.id))
            else {
                return error_response(AppError::bad_request(
                    "用 album slug 指定相册时，需要同时给 uid，或先登录",
                ));
            };
            match state
                .repo
                .find_album_by_slug(owner_id, album_slug.unwrap_or_default())
                .await
            {
                Ok(Some(album)) => album,
                Ok(None) => return error_response(AppError::not_found("相册")),
                Err(err) => return error_response(err.into()),
            }
        };
        if let Some(owner_id) = filter.owner_id {
            if owner_id != album.owner_id {
                return error_response(AppError::forbidden("相册不属于指定的用户"));
            }
        }
        let can_private = viewer
            .map(|user| user.id == album.owner_id || user.is_admin())
            .unwrap_or(false);
        if album.visibility != "public" && !can_private {
            return error_response(AppError::not_found("相册"));
        }
        public_only = !can_private;
        filter.album_ids = vec![album.id];
        filter.owner_id = Some(album.owner_id);
    }

    // 管理员命名的集合：先当公开展示集合处理；与相册过滤同时给时取交集。
    if let Some(name) = param("collection") {
        let collections = match state.settings.plaza_collections().await {
            Ok(value) => value,
            Err(err) => return error_response(err),
        };
        let Some(ids) = collections.get(name) else {
            return error_response(AppError::not_found("集合"));
        };
        let allowed = match state.repo.list_public_albums().await {
            Ok(albums) => albums.into_iter().map(|album| album.id).collect::<std::collections::HashSet<_>>(),
            Err(err) => return error_response(err.into()),
        };
        let ids: Vec<i64> = ids.iter().copied().filter(|id| allowed.contains(id)).collect();
        if ids.is_empty() {
            return error_response(AppError::not_found("图片"));
        }
        if filter.album_ids.is_empty() {
            filter.album_ids = ids.clone();
        } else {
            filter.album_ids.retain(|id| ids.contains(id));
        }
        if filter.album_ids.is_empty() {
            return error_response(AppError::not_found("图片"));
        }
        public_only = true;
        filter.visibility = None;
    }

    filter.query = param("q").map(str::to_string);

    // 标签：tag 是旧的单标签参数，tags 支持逗号分隔与 all / any。
    let mut tags: Vec<String> = Vec::new();
    if let Some(tag) = param("tag") {
        tags.push(tag.to_string());
    }
    if let Some(list) = param("tags") {
        for tag in list.split([',', '，']) {
            let tag = tag.trim();
            if !tag.is_empty() {
                tags.push(tag.to_string());
            }
        }
    }
    tags.sort();
    tags.dedup();
    let tag_mode = param("tag_mode").unwrap_or("all").to_ascii_lowercase();
    if tag_mode == "any" {
        filter.tags_any = tags;
    } else if tag_mode == "all" {
        filter.tags_all = tags;
    } else {
        return error_response(AppError::bad_request("tag_mode 只接受 all / any"));
    }
    filter.exclude_tag = param("exclude_tag").map(str::to_string);

    if let Some(raw) = param("exclude_ids") {
        filter.exclude_ids = raw
            .split(',')
            .filter_map(|id| id.trim().parse::<i64>().ok())
            .filter(|id| *id > 0)
            .take(100)
            .collect();
    }

    filter.min_width = parse_i64(&params, "min_width").filter(|value| *value > 0);
    filter.min_height = parse_i64(&params, "min_height").filter(|value| *value > 0);
    if let Some(orientation) = param("orientation") {
        let orientation = orientation.to_ascii_lowercase();
        if !matches!(orientation.as_str(), "landscape" | "portrait" | "square") {
            return error_response(AppError::bad_request(
                "orientation 只接受 landscape / portrait / square",
            ));
        }
        filter.orientation = Some(orientation);
    }
    if let Some(raw) = param("ratio") {
        let ratio = if let Some((w, h)) = raw.split_once(':') {
            w.trim().parse::<f64>().ok().zip(h.trim().parse::<f64>().ok())
        } else {
            raw.parse::<f64>().ok().map(|value| (value, 1.0))
        };
        let Some((w, h)) = ratio.filter(|(w, h)| *w > 0.0 && *h > 0.0) else {
            return error_response(AppError::bad_request("ratio 的格式应是 16:9 或 1.78"));
        };
        filter.aspect_ratio = Some(w / h);
    }
    if let Some(raw) = param("since") {
        filter.uploaded_after = match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(value) => Some(value.with_timezone(&chrono::Utc)),
            Err(_) => return error_response(AppError::bad_request("since 要是 RFC3339 时间")),
        };
    }
    if let Some(raw) = param("until") {
        filter.uploaded_before = match chrono::DateTime::parse_from_rfc3339(raw) {
            Ok(value) => Some(value.with_timezone(&chrono::Utc)),
            Err(_) => return error_response(AppError::bad_request("until 要是 RFC3339 时间")),
        };
    }
    if let Some(raw) = param("window") {
        let hours = raw
            .trim()
            .trim_end_matches(['h', 'H'])
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0)
            .map(|value| value.min(24 * 365));
        let Some(hours) = hours else {
            return error_response(AppError::bad_request("window 要是小时数，例如 24h"));
        };
        let after = chrono::Utc::now() - chrono::Duration::hours(hours);
        filter.uploaded_after = Some(match filter.uploaded_after {
            Some(existing) if existing > after => existing,
            _ => after,
        });
    }
    filter.weight_recent = match param("weight") {
        None => false,
        Some("recent") => true,
        Some(_) => {
            return error_response(AppError::bad_request("weight 目前只支持 recent"))
        }
    };
    filter.seed = parse_i64(&params, "seed")
        .unwrap_or(0)
        .max(0) as u64;
    filter.public_only = public_only;

    let count = match parse_i64(&params, "count") {
        None => 1,
        Some(value) if value >= 1 => value.min(50),
        Some(_) => return error_response(AppError::bad_request("count 至少是 1，最多 50")),
    };
    let picked = match state.repo.random_images(&filter, count).await {
        Ok(items) => items,
        Err(err) => return error_response(err.into()),
    };
    if picked.is_empty() {
        return error_response(AppError::not_found("图片"));
    }

    let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(picked.len());
    for view in &picked {
        let url = state.direct_url(&view.sha256, &view.ext, false).await;
        let thumb = state.direct_url(&view.sha256, &view.ext, true).await;
        payloads.push(serde_json::json!({
            "url": url,
            "thumb_url": thumb,
            "id": view.id,
            "width": view.width,
            "height": view.height,
            "size": view.size,
            "ext": view.ext,
            "tags": view.tags,
            "owner_uid": view.owner_uid,
        }));
    }

    if payloads.len() > 1 || param("format").map(|value| value == "json").unwrap_or(false) {
        let body = if payloads.len() == 1 {
            payloads.pop().unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Array(payloads)
        };
        return finish(Ok(body));
    }

    // 302 而不是 301：随机结果不该被永久缓存下来。
    let first = payloads.pop().unwrap_or(serde_json::Value::Null);
    let url = first
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let mut response = (StatusCode::FOUND, [(header::LOCATION, url)]).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
