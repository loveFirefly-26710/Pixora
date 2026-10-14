//! 静态资源：前端构建产物。
//!
//! 默认把 `web/dist` 用 `rust-embed` 打进二进制——部署时只有一个文件，
//! 不需要挂载静态目录（设计稿的明确要求）。开发时可以用 `PIXORA_WEB_DIR`
//! 指向源码目录直接读盘，前端改一行刷新即可，不必重新编译 Rust。
//!
//! SPA 路由回落：任何不认识的路径都返回 `index.html`，由前端路由接管。
//! 只在「没有扩展名」时才回落，避免把一个不存在的 `.js` 也当成页面返回，
//! 那样前端会拿到一段 HTML 去解析成脚本，报出难懂的语法错误。

use std::path::PathBuf;

use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::AppState;

#[cfg(feature = "embed-web")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// 提供一个静态文件。
pub async fn serve_asset(state: &AppState, uri: &Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(response) = try_file(state, path).await {
        return response;
    }

    // 只有「看起来像页面」的路径才回落到 index.html
    if !path.contains('.') {
        if let Some(response) = try_file(state, "index.html").await {
            return response;
        }
    }

    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "404 Not Found",
    )
        .into_response()
}

async fn try_file(state: &AppState, path: &str) -> Option<Response> {
    // 开发模式：直接读盘
    if let Some(dir) = state.config.web_dir.as_ref() {
        return read_from_disk(dir, path).await;
    }
    #[cfg(feature = "embed-web")]
    {
        let asset = WebAssets::get(path)?;
        let mime = mime_of(path);
        let mut response = Response::new(axum::body::Body::from(asset.data.into_owned()));
        apply_headers(response.headers_mut(), mime, path);
        Some(response)
    }
    #[cfg(not(feature = "embed-web"))]
    {
        None
    }
}

async fn read_from_disk(dir: &PathBuf, path: &str) -> Option<Response> {
    // 路径穿越防线：相对路径里不许出现 `..`
    if path.split('/').any(|part| part == "..") {
        return None;
    }
    let target = dir.join(path);
    let bytes = tokio::fs::read(&target).await.ok()?;
    let mime = mime_of(path);
    let mut response = Response::new(axum::body::Body::from(bytes));
    apply_headers(response.headers_mut(), mime, path);
    Some(response)
}

fn apply_headers(headers: &mut axum::http::HeaderMap, mime: &'static str, path: &str) {
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // 带哈希的产物可以长缓存；index.html 每次都校验，否则前端更新后用户看不到新版本
    let cache = if path == "index.html" || path.ends_with(".webmanifest") {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

/// 按扩展名给 MIME。前端产物就这几种，够用且不引依赖。
fn mime_of(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("webmanifest") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("map") => "application/json; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// 前端产物是否嵌进了二进制。启动日志里会说明当前用的是哪种方式。
pub fn embedded_available() -> bool {
    #[cfg(feature = "embed-web")]
    {
        WebAssets::get("index.html").is_some()
    }
    #[cfg(not(feature = "embed-web"))]
    {
        false
    }
}
