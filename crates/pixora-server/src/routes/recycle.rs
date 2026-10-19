//! 回收站：已删除的图片与相册。
//!
//! 删除是「先软删、可恢复」：图片进回收站后引用计数已经归零（不占配额），
//! 但物理文件按保留天数留着——超过保留期由后台清理任务彻底删掉。
//! 相册进回收站时带着里面的图片一起进，恢复时也只带回那一批
//! （用户自己在相册里单独删掉的图，不在这批里）。
//!
//! 归属判定在领域层：不属于当前账号的 id 会被跳过，而不是让整批失败。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::AppState;
use pixora_repo::PageRequest;

use crate::http::{
    cookie_value, finish, finish_page, parse_i64, parse_str, request_context, CSRF_COOKIE,
};

use super::{bad_request, require_user, viewer_for, PeerAddr};

/// `GET /api/v1/recycle/images` — 回收站里的图片（分页，形状与普通列表一致）。
pub async fn images_list(
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
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }

    let page = parse_i64(&query, "page").unwrap_or(1).max(1);
    let per_page = parse_i64(&query, "per_page").unwrap_or(24).clamp(1, 100);
    // 管理员从用户视图删图后，回收站也要按目标 uid 查（否则看到的是自己的空回收站）
    let uid = parse_str(&query, "uid").map(str::to_string);
    let viewer = viewer_for(Some(&auth), uid.is_some());

    finish_page(
        state
            .deleted_images(
                &viewer,
                uid.as_deref(),
                PageRequest::new(Some(page), Some(per_page), 24, 100),
            )
            .await
            .map(|(items, total, _retention)| pixora_repo::Page {
                items,
                total,
                page,
                per_page,
            }),
    )
}

/// `POST /api/v1/recycle/images/restore` — 从回收站恢复（`{ids: [...]}`）。
pub async fn images_restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let auth = match write_guard(&state, &headers, &method, peer.addr()).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let ids = id_list(&body);
    if ids.is_empty() {
        return bad_request("没有选中任何图片");
    }
    let viewer = viewer_for(Some(&auth), query.contains_key("uid"));
    match state.restore_images(&viewer, &ids).await {
        Ok(restored) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "restored": restored,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/recycle/images/purge` — 彻底删除选中的（`{ids: [...]}`）。
pub async fn images_purge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let auth = match write_guard(&state, &headers, &method, peer.addr()).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let ids = id_list(&body);
    if ids.is_empty() {
        return bad_request("没有选中任何图片");
    }
    let viewer = viewer_for(Some(&auth), query.contains_key("uid"));
    match state.purge_images(&viewer, &ids).await {
        Ok(purged) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "purged": purged,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/recycle/images/empty` — 清空回收站。
pub async fn images_empty(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let auth = match write_guard(&state, &headers, &method, peer.addr()).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let uid = parse_str(&query, "uid").map(str::to_string);
    let viewer = viewer_for(Some(&auth), uid.is_some());
    match state.empty_bin(&viewer, uid.as_deref()).await {
        Ok(purged) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "purged": purged,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/recycle/albums` — 回收站里的相册。
pub async fn albums_list(
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
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }
    let uid = parse_str(&query, "uid").map(str::to_string);
    let viewer = viewer_for(Some(&auth), uid.is_some());
    match state.deleted_albums(&viewer, uid.as_deref()).await {
        Ok(albums) => finish::<serde_json::Value>(Ok(serde_json::json!({ "albums": albums }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/recycle/albums/{id}/restore` — 恢复相册（连同随它删掉的图片）。
pub async fn album_restore(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(album_id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let auth = match write_guard(&state, &headers, &method, peer.addr()).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let viewer = viewer_for(Some(&auth), query.contains_key("uid"));
    match state.restore_album(&viewer, album_id).await {
        Ok(restored) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "restored_images": restored,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/recycle/albums/{id}` — 彻底删除相册（含其中的图片）。
pub async fn album_purge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(album_id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let auth = match write_guard(&state, &headers, &method, peer.addr()).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    let viewer = viewer_for(Some(&auth), query.contains_key("uid"));
    match state.purge_album(&viewer, album_id).await {
        Ok(purged) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "purged_images": purged,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// 写操作的公共前置：登录 + 写权限（CSRF 在 `require_user` 里一道过）。
async fn write_guard(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    method: &Method,
    addr: Option<std::net::SocketAddr>,
) -> Result<crate::http::Authenticated, Response> {
    let rectx = request_context(state, headers, addr).await;
    let auth = require_user(state, headers, rectx.ip.as_deref(), method).await?;
    super::ensure_scope(&auth, "write")?;
    Ok(auth)
}

fn id_list(body: &serde_json::Value) -> Vec<i64> {
    body.get("ids")
        .and_then(|value| value.as_array())
        .map(|array| array.iter().filter_map(|item| item.as_i64()).collect())
        .unwrap_or_default()
}
