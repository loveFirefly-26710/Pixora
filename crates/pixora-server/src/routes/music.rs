//! 音乐播放的公开端点。
//!
//! 歌单、歌词与本地音频都由服务端转发/提供：
//!   - 浏览器不直接请求 Meting API，避开 CORS，也不把第三方地址散到前端；
//!   - 本地文件带 Range 支持，播放器可以拖动进度；
//!   - 歌词代理只允许配置的 API 域名，避免变成任意 URL 读取器。

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use pixora_core::AppError;
use pixora_core::AppState;

use crate::http::{error_response, finish};

use super::PeerAddr;

/// `GET /api/v1/music/playlist` — 公开。
pub async fn playlist(State(state): State<Arc<AppState>>, _peer: PeerAddr) -> Response {
    finish(state.music_playlist().await)
}

/// `GET /api/v1/music/lyrics?url=…` 或 `?path=…` — 公开。
pub async fn lyrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let url = params.get("url").map(String::as_str);
    let path = params.get("path").map(String::as_str);
    match state.music_lyrics(url, path).await {
        Ok(text) => {
            let mut response = (axum::http::StatusCode::OK, text).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=300"),
            );
            response.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(err) => error_response(err),
    }
}

/// `GET /api/v1/music/file?path=…` — 公开。
///
/// `ServeFile` 自带 Range、If-Modified-Since 与 Content-Type 处理，
/// 这里只负责把相对路径解析成配置目录内的真实路径，并补上缓存头。
pub async fn file(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    request: Request,
) -> Response {
    let Some(path) = params.get("path").map(String::as_str) else {
        return error_response(AppError::bad_request("缺少音乐文件路径"));
    };
    let resolved = match state.resolve_local_music_file(path).await {
        Ok(path) => path,
        Err(err) => return error_response(err),
    };
    let service = ServeFile::new(resolved);
    match service.oneshot(request).await {
        Ok(response) => {
            let (parts, body) = response.into_parts();
            let mut response = Response::from_parts(parts, axum::body::Body::new(body));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            );
            response.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(err) => error_response(AppError::internal(format!("音乐文件读取失败：{err}"))),
    }
}
