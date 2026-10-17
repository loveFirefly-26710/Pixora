//! 直链路由与静态资源。
//!
//! 直链的形式是 `/{prefix}/{sha256}.{ext}`：只有一级目录加哈希文件名，
//! 不含用户 UID、相册名和原始文件名。前缀可在后台改，改完旧链依然有效——
//! 映射关系在数据库里，不在 URL 结构里。
//!
//! 几个响应头是刻意的：
//!   - `Cache-Control: public, max-age=31536000, immutable`：内容是哈希寻址的，
//!     同一路径的内容永不变化，缓存一年没有任何风险。
//!   - `ETag`: 就是 sha256，让客户端能走条件请求。
//!   - `X-Content-Type-Options: nosniff`：防止浏览器把 SVG 当脚本解析。
//!   - SVG 额外加 `Content-Security-Policy: sandbox`：入库时已经剥离了脚本与
//!     外部引用，这是第二道防线（万一净化有漏网的，沙箱也会拦住）。
//!
//! 本地驱动优先让 Caddy/Nginx 用 `X-Accel-Redirect` 发文件，Rust 进程完全不参与
//! 数据传输；没有配加速前缀时才自己流式回传。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pixora_core::{AppError, AppState};
use pixora_repo::model::storage_kind;

use crate::http::error_response;
use crate::routes::PeerAddr;

/// 处理 `/{prefix}/{file}`。
///
/// `prefix` 由调用方在路由层比对，这里只认文件名——前缀可以改，
/// 旧链要仍然有效，所以不能把前缀当成唯一入口。
pub async fn serve_direct(
    State(state): State<Arc<AppState>>,
    Path((prefix, file)): Path<(String, String)>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    peer: PeerAddr,
    headers: HeaderMap,
) -> Response {
    match direct_inner(state, prefix, file, params, headers, peer.0).await {
        Ok(response) => response,
        Err(err) => error_response(err),
    }
}

async fn direct_inner(
    state: Arc<AppState>,
    prefix: String,
    file: String,
    params: std::collections::HashMap<String, String>,
    headers: HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> pixora_core::Result<Response> {
    // 一级目录要匹配当前配置；`i`、`images` 这类都行，改完旧链仍能命中数据库映射
    let site = state
        .settings
        .site(state.config.base_url.as_deref())
        .await?;
    let expected = site.upload_prefix.trim_matches('/');
    let actual = prefix.trim_matches('/');
    let prefix_ok = actual == expected || actual == "images" || actual == "i" || actual == "p";
    if !prefix_ok {
        return Err(AppError::not_found("图片"));
    }

    // 防盗链：直链能被任何站点引用，热度和流量却全落在自己身上。
    // 开关关着时（默认）这一步什么都不做
    ensure_hotlink_allowed(&state, &headers).await?;

    // 文件名必须是 `{64位十六进制}.{ext}` 的形状，其它一律拒绝，
    // 避免有人拿它去探测任意键
    let (hash_part, ext) = file
        .rsplit_once('.')
        .ok_or_else(|| AppError::not_found("图片"))?;
    if hash_part.len() != 64 || !hash_part.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(AppError::not_found("图片"));
    }
    let hash = hash_part.to_lowercase();
    let want_thumb = params.contains_key("thumb");
    let want_download = params.contains_key("download");

    // 原图那一份实体：URL 里的哈希与扩展名讲的就是它
    let original = state
        .repo
        .find_blob_by_sha(&hash, storage_kind::ORIGINAL)
        .await?;
    // 缩略图是另一份实体（自己算哈希）。`?thumb=1` 的意思是「这张图片的缩略图」，
    // 所以要顺着图片行去找它——用 URL 里那个原图哈希去 thumb 里查是查不到的。
    // 没有缩略图就回落到原图：列表页靠这个不做额外判断
    let thumb = if want_thumb {
        state.repo.find_thumb_blob_for_original(&hash).await?
    } else {
        None
    };
    let is_thumb = thumb.is_some();
    let Some(blob) = thumb.or(original.clone()) else {
        return Err(AppError::not_found("图片"));
    };

    // 扩展名要和「URL 指向的那份文件」一致，防止有人用伪造扩展名探测任意键。
    // 比对的是原图的扩展名：缩略图存成 webp 是内部编码，不该出现在 URL 里
    if !ext.is_empty() {
        let expected = original
            .as_ref()
            .map(|item| item.ext.as_str())
            .unwrap_or(blob.ext.as_str());
        if pixora_core::media::normalize_ext(ext) != pixora_core::media::normalize_ext(expected) {
            return Err(AppError::not_found("图片"));
        }
    }

    // 会话只解析一次：隐私判定、下载限流、下载文件名都要用它
    let viewer = session_user(&state, &headers).await;

    // 私有图片：直链本身不可枚举，但拿到链接的人不该因此看到私有内容。
    // 这里按「谁在请求」放行——相册页里那些 <img> 用的正是这条直链，
    // 一律拒绝会让私有相册的缩略图全变成碎图标（实测就是这样）。
    // 放行条件：这个实体还挂着公开引用，或者请求者本人（含管理员）就引用着它。
    let private = !is_publicly_referenced(&state, blob.id).await?;
    if private && !references_blob(&state, viewer.as_ref(), blob.id).await? {
        return Err(AppError::not_found("图片"));
    }

    // 下载限流：只算「下载」（带 `?download=1`）。页面里的 <img> 走同一条直链
    // 但不带这个参数——把它也计进去的话，打开一次广场就把额度耗光了
    let download_actor = if want_download {
        let actor = download_actor(&state, &headers, peer, viewer.as_ref()).await;
        let limits = download_limits(&state, viewer.as_ref()).await?;
        state.limits.check_download(&actor, limits).await?;
        Some(actor)
    } else {
        None
    };

    let is_svg = blob.mime_type == "image/svg+xml";
    // 下载时给的文件名：`{短哈希}.jpg` 这样的真图片文件，不再是 `.img`。
    // 本人与管理员能拿到上传时的原始文件名，其他人只给哈希——原始名常带
    // 「IMG_20240312_身份证」这类信息，不该顺手递给广场上的人
    let download_name = if want_download {
        download_file_name(&state, &blob, &original, viewer.as_ref()).await
    } else {
        None
    };

    // 条件请求：内容寻址下 ETag 永远稳定
    if let Some(etag) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        if etag.trim_matches('"') == blob.sha256 {
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            response
                .headers_mut()
                .insert(header::ETAG, etag_header(&blob.sha256));
            return Ok(response);
        }
    }

    // 本地驱动 + 配了加速前缀：交给反向代理发文件
    if let Some(accel) = state.config.accel_prefix.as_deref() {
        if blob.storage_driver == pixora_storage::names::LOCAL {
            if let Some(path) = state
                .storage
                .local_path(&blob.storage_driver, blob.backend_id, &blob.storage_key)
                .await
            {
                let mut response = StatusCode::OK.into_response();
                let target = format!(
                    "{}/{}",
                    accel.trim_end_matches('/'),
                    path.to_string_lossy().trim_start_matches('/')
                );
                if let Ok(value) = HeaderValue::from_str(&target) {
                    response.headers_mut().insert("X-Accel-Redirect", value);
                }
                apply_headers(
                    &mut response,
                    &blob.mime_type,
                    &blob.sha256,
                    is_svg,
                    want_download,
                    blob.size,
                    !private,
                    download_name.as_deref(),
                );
                return Ok(response);
            }
        }
    }

    let reader = state
        .storage
        .open_object(&blob.storage_driver, blob.backend_id, &blob.storage_key)
        .await?;
    // 计数放在真的要把文件发出去之前：限的是「被取走多少」，不是「被请求多少」。
    // 命中条件请求的 304 早在上一步就返回了，那次没传出字节，自然不计数
    if let Some(actor) = download_actor.as_ref() {
        if let Err(err) = state.limits.record_download(actor).await {
            tracing::debug!(error = %err, "下载计数失败");
        }
    }
    let stream = pixora_storage::local::into_stream(reader);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(stream))
        .map_err(|err| AppError::internal(err.to_string()))?;
    apply_headers(
        &mut response,
        &blob.mime_type,
        &blob.sha256,
        is_svg,
        want_download,
        blob.size,
        !private,
        download_name.as_deref(),
    );
    let _ = is_thumb;
    Ok(response)
}

/// 防盗链：判断这次请求的来源站是否被允许。
///
/// 开关关着（默认）时什么都不做——这功能一开，别人站里已经引用的图会立刻挂掉，
/// 得由管理员明确决定，不能让默认值替他决定。
///
/// 开着时的判定顺序：
///   1. 没有 `Referer`：由 `security.hotlink_allow_empty` 决定（默认允许）。
///      直接在地址栏打开、PicGo 这类客户端、部分隐私设置都会让 Referer 为空，
///      一律拒绝会误伤大量正常访问。
///   2. 来源主机与请求的 `Host` 一致（本站页面自己引用）→ 放行。
///   3. 来源主机与站点 `base_url` 的主机一致 → 放行（有 CDN/反向代理时常见）。
///   4. 来源主机命中白名单（含子域）→ 放行。
///   5. 其余一律 403。
async fn ensure_hotlink_allowed(state: &AppState, headers: &HeaderMap) -> pixora_core::Result<()> {
    let settings = state.settings.hotlink().await?;
    if !settings.enabled {
        return Ok(());
    }

    let referer = headers
        .get(axum::http::header::REFERER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(referer) = referer else {
        return if settings.allow_empty {
            Ok(())
        } else {
            Err(AppError::forbidden(
                "这个图片不允许直接访问：请求没有携带来源信息".to_string(),
            ))
        };
    };

    let Some(host) = host_of(referer) else {
        return Err(AppError::forbidden(
            "这个图片不允许从该来源访问".to_string(),
        ));
    };

    let request_host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if host_matches(&host, request_host) {
        return Ok(());
    }

    let site = state
        .settings
        .site(state.config.base_url.as_deref())
        .await?;
    if let Some(site_host) = host_of(&site.base_url) {
        if host_matches(&host, &site_host) {
            return Ok(());
        }
    }

    if settings
        .domains
        .iter()
        .any(|allowed| host_matches(&host, allowed))
    {
        return Ok(());
    }

    tracing::debug!(referer, "防盗链拦下了一次直链访问");
    Err(AppError::forbidden(
        "这个图片不允许从该来源访问（站点开启了防盗链）".to_string(),
    ))
}

/// 从 URL 里取主机（含端口，统一小写）。取不出主机就返回 `None`。
///
/// 只做字符串处理，不引入 URL 解析库：输入的形态就三种
/// （`https://host/...`、`//host/...`、`host/...`），而且这里只用来比对。
fn host_of(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("//"))
        .unwrap_or(trimmed);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@') // 去掉可能的 userinfo
        .next()
        .unwrap_or_default()
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// 两个主机是否算同一个：完全相等，或者只差端口。
///
/// 带不带端口都算同一站：`example.com` 与 `example.com:8080` 是同一台服务器，
/// 部署时域名解析和端口写法经常不一致。
fn host_matches(host: &str, allowed: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let Some(allowed) = host_of(allowed) else {
        return false;
    };
    if host == allowed {
        return true;
    }
    let bare = |value: &str| value.split(':').next().unwrap_or(value).to_string();
    let (host_bare, allowed_bare) = (bare(&host), bare(&allowed));
    if host_bare == allowed_bare {
        return true;
    }
    // 白名单里写 `example.com` 时，`img.example.com` 也算命中：
    // 子域常用来放静态资源，逐个列出来太啰嗦
    host_bare.ends_with(&format!(".{allowed_bare}"))
}

/// 这个实体有没有公开引用。
///
/// 一张物理文件可能被多人引用：只要有一处是公开的，直链就应当放行；
/// 全都被私有引用时才算私有。这正是「删一份不误伤另一份」的另一面。
///
/// 缩略图要单独看：它自己的 id 不在 `images.blob_id` 上，而在 `thumb_blob_id` 上。
/// 只查 `blob_id` 的话缩略图的引用集永远为空，会被当成私有——所有缩略图都会 404。
async fn is_publicly_referenced(state: &AppState, blob_id: i64) -> pixora_core::Result<bool> {
    let rows = state
        .repo
        .db()
        .fetch_all(
            "SELECT visibility FROM images               WHERE (blob_id = ? OR thumb_blob_id = ?) AND deleted_at IS NULL",
            pixora_repo::Args::new().int(blob_id).int(blob_id),
        )
        .await?;
    for row in rows {
        if pixora_repo::model::get_str(&row, "visibility")?
            == pixora_repo::model::visibility::PUBLIC
        {
            return Ok(true);
        }
    }
    // 没有任何公开引用（包括「一条引用都没有」）都算私有：直链不发。
    Ok(false)
}

/// 直链请求者是谁（只看会话 Cookie）。
///
/// `<img>` 标签带不了 Authorization 头，所以这里是浏览器会话的判断。Token 访问
/// 图片走的是带鉴权的详情接口，不经过直链。会话无效、没登录都返回 `None`。
async fn session_user(state: &AppState, headers: &HeaderMap) -> Option<pixora_repo::model::User> {
    let token = crate::http::session_cookie(headers)?;
    match state.authenticate_session(&token, None).await {
        Ok((user, _)) => Some(user),
        Err(_) => None,
    }
}

/// 这个请求者是不是「引用着这个实体的人」（管理员一并放行）。
///
/// 回收站里的引用也算数：那里的图片是软删的行，本人当然还「有权限」，
/// 而回收站页面上的预览图用的正是这条直链——把它们排除掉，本人打开回收站
/// 看到的就是一排碎图标。
async fn references_blob(
    state: &AppState,
    viewer: Option<&pixora_repo::model::User>,
    blob_id: i64,
) -> pixora_core::Result<bool> {
    let Some(user) = viewer else {
        return Ok(false);
    };
    if user.is_admin() {
        return Ok(true);
    }
    let hit = state
        .repo
        .db()
        .fetch_optional(
            "SELECT 1 AS hit FROM images WHERE owner_id = ? AND (blob_id = ? OR thumb_blob_id = ?)",
            pixora_repo::Args::new()
                .int(user.id)
                .int(blob_id)
                .int(blob_id),
        )
        .await?;
    Ok(hit.is_some())
}

/// 一次下载算在谁头上：登录了算账号，没登录算那个 IP。
///
/// 解析不出 IP（比如 Unix 域套接字上的请求）时统一记成一个占位主体：
/// 好过放行——额度是按主体分开的，占位主体只会让这些请求共用一份额度。
async fn download_actor(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    viewer: Option<&pixora_repo::model::User>,
) -> pixora_core::ratelimit::DownloadActor {
    if let Some(user) = viewer {
        return pixora_core::ratelimit::DownloadActor::User(user.id);
    }
    let ip = crate::http::request_context(state, headers, peer).await.ip;
    pixora_core::ratelimit::DownloadActor::Ip(ip.unwrap_or_else(|| "unknown".to_string()))
}

/// 这个请求者的下载额度：游客读系统设置，登录用户读他生效的那份限流策略。
async fn download_limits(
    state: &AppState,
    viewer: Option<&pixora_repo::model::User>,
) -> pixora_core::Result<pixora_core::ratelimit::DownloadLimits> {
    let Some(user) = viewer else {
        let settings = state.settings.download().await?;
        return Ok(pixora_core::ratelimit::DownloadLimits {
            per_minute: settings.anon_per_minute,
            per_day: settings.anon_per_day,
        });
    };
    let policy = state.repo.effective_policy(user).await?;
    // 管理员不受限：他们是站点的主人，配额度的人把自己锁在门外没有意义
    if policy.unlimited || user.is_admin() {
        return Ok(pixora_core::ratelimit::DownloadLimits {
            per_minute: 0,
            per_day: 0,
        });
    }
    Ok(pixora_core::ratelimit::DownloadLimits {
        per_minute: policy.download_per_minute,
        per_day: policy.download_per_day,
    })
}

/// 下载时的文件名。
///
/// 扩展名取原图那一份（URL 里的 ext 也是它的），保证落盘的是一个真正的图片文件：
/// 以前这里发的是 `{哈希}.img`，文件管理器不认、双击打不开。本人与管理员另外
/// 拿到上传时的原始文件名；原始名可能带个人信息，对外只给哈希。
async fn download_file_name(
    state: &AppState,
    blob: &pixora_repo::model::Blob,
    original: &Option<pixora_repo::model::Blob>,
    viewer: Option<&pixora_repo::model::User>,
) -> Option<String> {
    let ext = original
        .as_ref()
        .map(|item| item.ext.as_str())
        .unwrap_or(blob.ext.as_str());
    let ext = if ext.is_empty() { "img" } else { ext };
    let short = &blob.sha256[..16.min(blob.sha256.len())];

    if let Some(user) = viewer {
        let source = original.as_ref().unwrap_or(blob);
        let found = state
            .repo
            .find_live_image_for_download(source.id, Some(user.id))
            .await
            .ok()
            .flatten();
        if let Some(image) = found {
            if let Some(name) = sanitize_file_name(&image.original_name, ext) {
                return Some(name);
            }
        }
    }
    Some(format!("{short}.{ext}"))
}

/// 原始文件名要能安全地放进 `Content-Disposition`：去掉路径分隔符、控制字符
/// 与引号，只留文件名本身；结果不像个名字就返回 `None`（调用方回落到哈希）。
fn sanitize_file_name(raw: &str, ext: &str) -> Option<String> {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .chars()
        .filter(|ch| !ch.is_control() && !"\"\\".contains(*ch))
        .collect::<String>();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    // 名字太长截断、没带扩展名就补上（下载下来要能认出来是什么）
    let mut name: String = name.chars().take(120).collect();
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(&format!(".{}", ext.to_ascii_lowercase())) {
        name = format!("{name}.{ext}");
    }
    Some(name)
}

fn apply_headers(
    response: &mut Response,
    mime: &str,
    sha256: &str,
    is_svg: bool,
    download: bool,
    size: i64,
    public: bool,
    file_name: Option<&str>,
) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // 公开图片可以放心长缓存（内容寻址，同一路径内容永不变）；私有图片只能
    // private 缓存——写 public 会让共享代理把它缓存下来发给别人。
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if public {
            "public, max-age=31536000, immutable"
        } else {
            "private, max-age=300"
        }),
    );
    headers.insert(header::ETAG, etag_header(sha256));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if size > 0 {
        if let Ok(value) = HeaderValue::from_str(&size.to_string()) {
            headers.insert(header::CONTENT_LENGTH, value);
        }
    }

    if is_svg {
        // 沙箱化：即使净化有漏网，脚本也不会有执行上下文；默认连外部图片
        // 都不让加载，只允许 data: 内联。
        headers.insert(
            "Content-Security-Policy",
            HeaderValue::from_static(
                "sandbox; default-src 'none'; script-src 'none'; object-src 'none'; \
                 img-src data:; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
        );
    }
    if download {
        if let Some(value) = content_disposition(
            file_name,
            &format!("{}.svg", &sha256[..16.min(sha256.len())]),
        ) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
}

/// 下载响应头里的文件名。
///
/// 名字可能带中文或空格，所以两种写法都给：`filename=` 是给老客户端的兜底
/// （非 ASCII 会被剥掉），`filename*=UTF-8''…` 才是规范那份（RFC 5987）。
/// 少了后者，中文文件名下载下来会变成一串问号。
fn content_disposition(file_name: Option<&str>, fallback: &str) -> Option<HeaderValue> {
    let raw = file_name.unwrap_or(fallback).trim();
    if raw.is_empty() {
        return None;
    }
    let ascii: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii() && ch != '"' && ch != '\\' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .ok()
}

fn etag_header(sha256: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{sha256}\"")).unwrap_or(HeaderValue::from_static("\"0\""))
}
