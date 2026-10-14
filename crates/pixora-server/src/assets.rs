//! 静态资源：前端构建产物。
//!
//! 默认把 `web/dist` 用 `rust-embed` 打进二进制——部署时只有一个文件，
//! 不需要挂载静态目录（设计稿的明确要求）。开发时可以用 `PIXORA_WEB_DIR`
//! 指向源码目录直接读盘，前端改一行刷新即可，不必重新编译 Rust。
//!
//! SPA 路由回落：任何不认识的路径都返回 `index.html`，由前端路由接管。
//! 只在「没有扩展名」时才回落，避免把一个不存在的 `.js` 也当成页面返回，
//! 那样前端会拿到一段 HTML 去解析成脚本，报出难懂的语法错误。
//!
//! ## 缓存：产物是固定文件名，所以一律 no-cache
//!
//! 构建出来的就是 `main.js` / `main.css` / `workers/*.js`，文件名里没有内容哈希。
//! 给它们长缓存是个陷阱：换一次二进制之后，用户浏览器里的旧 bundle 最长会赖
//! 到缓存过期，现象是「服务端明明改了，界面还是老的」，只有强制刷新才出来。
//! 所以这里对全部产物发 `Cache-Control: no-cache`（每次校验）+ 内容 ETag：
//! 没变就是一次 304、没有响应体，代价只是多一个请求头。

use std::path::PathBuf;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::AppState;

#[cfg(feature = "embed-web")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// 提供一个静态文件。`request_headers` 只用来看 `If-None-Match`。
pub async fn serve_asset(state: &AppState, request_headers: &HeaderMap, uri: &Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(response) = try_file(state, request_headers, path).await {
        return response;
    }

    // 只有「看起来像页面」的路径才回落到 index.html
    if !path.contains('.') {
        if let Some(response) = try_file(state, request_headers, "index.html").await {
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

async fn try_file(state: &AppState, request_headers: &HeaderMap, path: &str) -> Option<Response> {
    // 开发模式：直接读盘
    if let Some(dir) = state.config.web_dir.as_ref() {
        return read_from_disk(dir, request_headers, path).await;
    }
    #[cfg(feature = "embed-web")]
    {
        let asset = WebAssets::get(path)?;
        let mime = mime_of(path);
        Some(build_response(request_headers, asset.data.into_owned(), mime))
    }
    #[cfg(not(feature = "embed-web"))]
    {
        None
    }
}

async fn read_from_disk(dir: &PathBuf, request_headers: &HeaderMap, path: &str) -> Option<Response> {
    // 路径穿越防线：相对路径里不许出现 `..`
    if path.split('/').any(|part| part == "..") {
        return None;
    }
    let target = dir.join(path);
    let bytes = tokio::fs::read(&target).await.ok()?;
    let mime = mime_of(path);
    Some(build_response(request_headers, bytes, mime))
}

/// 组响应：算 ETag、看 `If-None-Match` 决定 200 还是 304。
///
/// ETag 用内容的 sha256：进程重启、二进制重新构建，只要字节一样，
/// ETag 就一样——开发时反复重启也不会把用户的缓存全部打掉。
fn build_response(request_headers: &HeaderMap, bytes: Vec<u8>, mime: &'static str) -> Response {
    let etag = format!("\"{}\"", pixora_crypto::hash::sha256_hex(&bytes));
    let not_modified = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim())
                // `W/"…"` 与 `"…"` 在这里等价：我们不做弱校验的区分
                .any(|item| item == "*" || item.trim_start_matches("W/") == etag)
        })
        .unwrap_or(false);

    let mut response = if not_modified {
        // 304 不带实体，也就不用带 Content-Type
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Response::new(axum::body::Body::from(bytes));
        if let Ok(value) = HeaderValue::from_str(mime) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        response
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_if_none_match(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn etag_与内容绑定_且支持条件请求() {
        let body = b"console.log('pixora')".to_vec();
        let fresh = build_response(&HeaderMap::new(), body.clone(), "text/javascript");
        assert_eq!(fresh.status(), StatusCode::OK);
        let etag = fresh.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();
        assert_eq!(fresh.headers().get(header::CACHE_CONTROL).unwrap(), "no-cache");

        // 带对 ETag 的请求 → 304，且没有实体
        let cached = build_response(&headers_with_if_none_match(&etag), body.clone(), "text/javascript");
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert!(cached.headers().get(header::CONTENT_TYPE).is_none());
        assert_eq!(cached.headers().get(header::ETAG).unwrap().to_str().unwrap(), etag);

        // 弱校验前缀与 `*` 也认
        let weak = build_response(
            &headers_with_if_none_match(&format!("W/{etag}")),
            body.clone(),
            "text/javascript",
        );
        assert_eq!(weak.status(), StatusCode::NOT_MODIFIED);
        let star = build_response(&headers_with_if_none_match("*"), body.clone(), "text/javascript");
        assert_eq!(star.status(), StatusCode::NOT_MODIFIED);

        // 内容变了 ETag 就变，不能命中 304
        let changed = build_response(
            &headers_with_if_none_match(&etag),
            b"console.log('changed')".to_vec(),
            "text/javascript",
        );
        assert_eq!(changed.status(), StatusCode::OK);
        assert_ne!(changed.headers().get(header::ETAG).unwrap().to_str().unwrap(), etag);
    }
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
