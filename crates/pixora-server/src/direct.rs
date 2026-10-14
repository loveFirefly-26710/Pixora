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

use crate::http::{error_response, AppResponse};

/// 处理 `/{prefix}/{file}`。
///
/// `prefix` 由调用方在路由层比对，这里只认文件名——前缀可以改，
/// 旧链要仍然有效，所以不能把前缀当成唯一入口。
pub async fn serve_direct(
    State(state): State<Arc<AppState>>,
    Path((prefix, file)): Path<(String, String)>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    match direct_inner(state, prefix, file, params, headers).await {
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
) -> pixora_core::Result<Response> {
    // 一级目录要匹配当前配置；`i`、`images` 这类都行，改完旧链仍能命中数据库映射
    let site = state.settings.site(state.config.base_url.as_deref()).await?;
    let expected = site.upload_prefix.trim_matches('/');
    let actual = prefix.trim_matches('/');
    let prefix_ok = actual == expected
        || actual == "images"
        || actual == "i"
        || actual == "p";
    if !prefix_ok {
        return Err(AppError::not_found("图片"));
    }

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

    // 缩略图是另一份实体；没有缩略图时回落到原图（列表页就是靠这个不做额外判断）
    let kind = if want_thumb {
        storage_kind::THUMB
    } else {
        storage_kind::ORIGINAL
    };
    let mut blob = state.repo.find_blob_by_sha(&hash, kind).await?;
    let mut is_thumb = want_thumb;
    if blob.is_none() && want_thumb {
        blob = state
            .repo
            .find_blob_by_sha(&hash, storage_kind::ORIGINAL)
            .await?;
        is_thumb = false;
    }
    let Some(blob) = blob else {
        return Err(AppError::not_found("图片"));
    };

    // 扩展名与库里记录不一致 → 当作不存在，防止用伪造扩展名探测
    if !ext.is_empty()
        && pixora_core::media::normalize_ext(ext) != pixora_core::media::normalize_ext(&blob.ext)
    {
        return Err(AppError::not_found("图片"));
    }

    // 私有图片不允许直接访问：直链是不可枚举的，但拿到链接的人不该因此看到私有内容。
    // 所有者与管理员的访问也不给直链——他们用带鉴权的详情接口。
    let private = is_private(&state, blob.id).await?;
    if private {
        return Err(AppError::not_found("图片"));
    }

    let is_svg = blob.mime_type == "image/svg+xml";

    // 条件请求：内容寻址下 ETag 永远稳定
    if let Some(etag) = headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok()) {
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
                    response
                        .headers_mut()
                        .insert("X-Accel-Redirect", value);
                }
                apply_headers(&mut response, &blob.mime_type, &blob.sha256, is_svg, want_download, blob.size);
                return Ok(response);
            }
        }
    }

    let reader = state
        .storage
        .open_object(&blob.storage_driver, blob.backend_id, &blob.storage_key)
        .await?;
    let stream = pixora_storage::local::into_stream(reader);
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from_stream(stream))
        .map_err(|err| AppError::internal(err.to_string()))?;
    apply_headers(&mut response, &blob.mime_type, &blob.sha256, is_svg, want_download, blob.size);
    let _ = is_thumb;
    Ok(response)
}

/// 判断这个实体是否只被私有图片引用。
///
/// 一张物理文件可能被多人引用：只要有一处是公开的，直链就应当放行；
/// 全都被私有引用时才算私有。这正是「删一份不误伤另一份」的另一面。
///
/// 缩略图要单独看：它自己的 id 不在 `images.blob_id` 上，而在 `thumb_blob_id` 上。
/// 只查 `blob_id` 的话缩略图的引用集永远为空，会被当成私有——所有缩略图都会 404。
async fn is_private(state: &AppState, blob_id: i64) -> pixora_core::Result<bool> {
    let rows = state
        .repo
        .db()
        .fetch_all(
            "SELECT visibility FROM images               WHERE (blob_id = ? OR thumb_blob_id = ?) AND deleted_at IS NULL",
            pixora_repo::Args::new().int(blob_id).int(blob_id),
        )
        .await?;
    if rows.is_empty() {
        // 没有任何引用：可能是刚上传还没建引用，按私有处理更安全
        return Ok(true);
    }
    let mut any_public = false;
    for row in rows {
        let visibility = pixora_repo::model::get_str(&row, "visibility")?;
        if visibility == pixora_repo::model::visibility::PUBLIC {
            any_public = true;
            break;
        }
    }
    Ok(!any_public)
}

fn apply_headers(
    response: &mut Response,
    mime: &str,
    sha256: &str,
    is_svg: bool,
    download: bool,
    size: i64,
) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
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
        // 沙箱化：即使净化有漏网，脚本也不会有执行上下文
        headers.insert(
            "Content-Security-Policy",
            HeaderValue::from_static("sandbox; script-src 'none'; object-src 'none'"),
        );
    }
    if download {
        if let Ok(value) = HeaderValue::from_str(&format!(
            "attachment; filename=\"{}.{}\"",
            &sha256[..16.min(sha256.len())],
            if is_svg { "svg" } else { "img" }
        )) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
    }
}

fn etag_header(sha256: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{sha256}\"")).unwrap_or(HeaderValue::from_static("\"0\""))
}

/// 健康检查。给容器编排与反向代理用。
pub async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let db_ok = state.repo.db().ping().await.is_ok();
    let uptime = (chrono::Utc::now() - state.started_at).num_seconds().max(0);
    let body = serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "db": if db_ok { "ok" } else { "error" },
        "uptime_seconds": uptime,
    });
    if db_ok {
        AppResponse::ok(body)
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "degraded", "db": "error" })),
        )
            .into_response()
    }
}
