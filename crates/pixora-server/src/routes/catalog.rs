//! 相册、图片、标签与目录树。
//!
//! 「谁能看谁的东西」不在这里判：它是 `Viewer` 的职责，路由只负责把
//! 「这次请求带着谁的身份、想看谁的空间」翻译成一个 `Viewer`。
//! 传错一个参数就会变成越权，所以构造规则只写一次（[`super::viewer_for`]）：
//! **只有管理员 + 显式带了 `uid` 才是跨用户视角**，其余一律是自己。
//!
//! 读接口允许匿名（未登录只能看到公开图片，裁剪在 `pixora_core::images` 里做）；
//! 写接口一律 `require_user`，并且对 API Token 做范围校验（契约里
//! `read` / `write` 的分工就是为 PicGo 这类只有上传权限的客户端准备的）。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::images::{BatchAction, ListQuery};
use pixora_core::AppState;

use crate::http::{
    cookie_value, finish, finish_page, parse_i64, parse_str, request_context, CSRF_COOKIE,
};

use super::{
    bad_request, field_opt_i64, field_opt_str, field_string_list, not_found, require_user,
    viewer_for, PeerAddr,
};

/// `GET /api/v1/albums` — 侧边栏要全量，所以不分页。
pub async fn albums_list(
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
    if let Some(auth) = auth.as_ref() {
        if let Err(resp) = super::ensure_scope(auth, "read") {
            return resp;
        }
    }

    let uid = parse_str(&query, "uid");
    // 只有管理员显式指定 uid 才是「看别人的空间」，其余都按自己的处理
    let viewer = viewer_for(auth.as_ref(), uid.is_some());

    match state.albums_payload(&viewer, uid).await {
        Ok(payload) => finish::<pixora_core::albums::AlbumListPayload>(Ok(payload)),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/albums` — 建相册。
///
/// `slug` 省略时按名称净化（`sanitize_slug`），它决定物理目录名，
/// 所以净化必须走领域层那份实现，不能在路由里自己写一遍。
pub async fn album_create(
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

    let name = match super::field_str(&body, "name") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let slug = field_opt_str(&body, "slug");
    let description = field_opt_str(&body, "description");
    let visibility = field_opt_str(&body, "visibility");

    let viewer = viewer_for(Some(&auth), false);
    match state
        .create_album(&viewer, &name, slug.as_deref(), description.as_deref(), visibility.as_deref())
        .await
    {
        Ok(album) => finish::<pixora_core::albums::AlbumPayload>(Ok(album)),
        Err(err) => crate::http::error_response(err),
    }
}

/// `PATCH /api/v1/albums/{id}`。
///
/// 三态字段（改 / 清空 / 不动）靠 `field_present` 区分：
/// `description: null` 是「清空描述」，不带这个键才是「不改」。
pub async fn album_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(album_id): Path<i64>,
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

    let name = field_opt_str(&body, "name");
    // 三态：键不存在 = 不改；值是 null = 清空；值是字符串 = 改成它
    let description: Option<Option<String>> = if super::field_present(&body, "description") {
        Some(field_opt_str(&body, "description"))
    } else {
        None
    };
    let description = description.as_ref().map(|value| value.as_deref());
    let visibility = field_opt_str(&body, "visibility");
    let sort_order = field_opt_i64(&body, "sort_order");
    let cover_blob_id = if super::field_present(&body, "cover_blob_id") {
        Some(field_opt_i64(&body, "cover_blob_id"))
    } else {
        None
    };

    let viewer = viewer_for(Some(&auth), false);
    match state
        .update_album(
            &viewer,
            album_id,
            name.as_deref(),
            description,
            visibility.as_deref(),
            sort_order,
            cover_blob_id,
        )
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/albums/{id}`。
///
/// 删相册**不删图片**（领域层只解除关联），返回解除了多少张，
/// 前端可以用它做「已把 236 张图片移到未分类」的提示。
pub async fn album_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(album_id): Path<i64>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let viewer = viewer_for(Some(&auth), false);
    match state.delete_album(&viewer, album_id).await {
        Ok(detached) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "detached_images": detached,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/images`。
///
/// `ids` 是逗号分隔的一串 id：批量操作前回读用，语义与「分页列表」不同
/// （前端拿它保证「选中的那些现在还在、状态是什么」），所以原样透传给领域层。
pub async fn images_list(
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
    if let Some(auth) = auth.as_ref() {
        if let Err(resp) = super::ensure_scope(auth, "read") {
            return resp;
        }
    }

    let uid = parse_str(&query, "uid").map(str::to_string);
    let ids: Option<Vec<i64>> = parse_str(&query, "ids").map(|raw| {
        raw.split(',')
            .filter_map(|item| item.trim().parse::<i64>().ok())
            .collect()
    });

    let viewer = viewer_for(auth.as_ref(), uid.is_some());
    let q = ListQuery {
        uid,
        album_id: parse_i64(&query, "album_id"),
        query: parse_str(&query, "q").map(str::to_string),
        tag: parse_str(&query, "tag").map(str::to_string),
        sort: parse_str(&query, "sort").map(str::to_string),
        ids,
        page: parse_i64(&query, "page"),
        per_page: parse_i64(&query, "per_page"),
    };

    finish_page(state.list_images(&viewer, &q).await)
}

/// `GET /api/v1/images/{id}`。
///
/// 匿名也能调：公开图片的分享页不需要登录。能不能看由领域层判
/// （非本人且非公开一律按 `NOT_FOUND`，不泄露「这张图存在但你不能看」）。
pub async fn image_detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Path(image_id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match crate::http::authenticate_optional(&state, &headers, rectx.ip.as_deref()).await {
        Ok(auth) => auth,
        Err(err) => return crate::http::error_response(err),
    };
    if let Some(auth) = auth.as_ref() {
        if let Err(resp) = super::ensure_scope(auth, "read") {
            return resp;
        }
    }

    // 管理员带 uid 看别人的图时属于跨用户访问（领域层会写审计），
    // 这里只需把身份翻成 Viewer
    let viewer = viewer_for(auth.as_ref(), query.contains_key("uid"));
    finish::<pixora_core::images::ImageDetailPayload>(state.image_detail(&viewer, image_id).await)
}

/// `PATCH /api/v1/images/{id}`。
///
/// `album_id` 的三态是这里最容易做错的地方：
///   - 键不存在 → 不改所属相册；
///   - `null`   → 移出相册（进「未分类」）；
///   - 数字     → 移动到该相册。
/// `field_opt_i64` 对前两种都返回 `None`，所以必须先用 `field_present` 分流。
pub async fn image_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(image_id): Path<i64>,
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

    let title = if super::field_present(&body, "title") {
        Some(field_opt_str(&body, "title"))
    } else {
        None
    };
    let visibility = field_opt_str(&body, "visibility");
    let album_id = if super::field_present(&body, "album_id") {
        Some(field_opt_i64(&body, "album_id"))
    } else {
        None
    };
    let tags = field_string_list(&body, "tags");

    let viewer = viewer_for(Some(&auth), false);
    match state
        .update_image(&viewer, image_id, title, visibility, album_id, tags)
        .await
    {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `DELETE /api/v1/images/{id}`：软删引用，物理文件等 GC。
pub async fn image_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(image_id): Path<i64>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "write") {
        return resp;
    }

    let viewer = viewer_for(Some(&auth), false);
    match state.delete_image(&viewer, image_id).await {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `POST /api/v1/images/batch`：一个请求只做一件事。
pub async fn images_batch(
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

    let action = match field_opt_str(&body, "action")
        .map(|value| BatchAction::parse(&value))
        .transpose()
    {
        Ok(Some(action)) => action,
        Ok(None) => return bad_request("缺少参数 action"),
        Err(err) => return crate::http::error_response(err),
    };
    let ids: Vec<i64> = body
        .get("ids")
        .and_then(|value| value.as_array())
        .map(|array| array.iter().filter_map(|item| item.as_i64()).collect())
        .unwrap_or_default();
    let album_id = field_opt_i64(&body, "album_id");
    let tags = field_string_list(&body, "tags");
    let visibility = field_opt_str(&body, "visibility");

    let viewer = viewer_for(Some(&auth), false);
    match state
        .batch_images(&viewer, action, &ids, album_id, tags, visibility)
        .await
    {
        Ok(affected) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "affected": affected,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// `GET /api/v1/tags`：`?scope=plaza` 走全站公开标签，其余走「我的标签」。
///
/// 两条路径的数据来源完全不同（一个跨用户聚合、一个按 owner 查），
/// 放在一个端点上是因为前端只有一个标签面板，参数决定它的数据源。
pub async fn tags_list(
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
    if let Some(auth) = auth.as_ref() {
        if let Err(resp) = super::ensure_scope(auth, "read") {
            return resp;
        }
    }

    let limit = parse_i64(&query, "limit").unwrap_or(100).clamp(1, 500);
    if parse_str(&query, "scope") == Some("plaza") {
        return match state.repo.plaza_tags(limit).await {
            Ok(tags) => finish::<Vec<serde_json::Value>>(Ok(tag_rows(tags))),
            Err(err) => crate::http::error_response(err.into()),
        };
    }

    let Some(auth) = auth.as_ref() else {
        return crate::http::error_response(pixora_core::AppError::Unauthorized);
    };
    // 管理员带 uid 时看别人的标签空间（会写审计），普通用户只能看自己
    let uid = parse_str(&query, "uid");
    let owner_id = match uid {
        Some(uid) if auth.user.is_admin() => match state.repo.find_user_by_uid(uid).await {
            Ok(Some(user)) => {
                if user.id != auth.user.id {
                    super::audit_admin_view(&state, &auth.user, uid, "tags", rectx.ip.as_deref())
                        .await;
                }
                user.id
            }
            Ok(None) => return not_found("用户"),
            Err(err) => return crate::http::error_response(err.into()),
        },
        Some(uid) if uid == auth.user.uid => auth.user.id,
        Some(_) => {
            return crate::http::error_response(pixora_core::AppError::forbidden(
                "只能查看自己的标签",
            ))
        }
        None => auth.user.id,
    };

    match state
        .repo
        .list_tags(owner_id, parse_str(&query, "q"), limit)
        .await
    {
        Ok(tags) => finish::<Vec<serde_json::Value>>(Ok(tag_rows(tags))),
        Err(err) => crate::http::error_response(err.into()),
    }
}

fn tag_rows(tags: Vec<pixora_repo::model::Tag>) -> Vec<serde_json::Value> {
    tags.into_iter()
        .map(|tag| serde_json::json!({ "name": tag.name, "usage": tag.usage }))
        .collect()
}

/// `PATCH /api/v1/tags`：改名 / 删除。
///
/// 改名是「把这一条标签在图片上的引用整体换成新名字」，不是新建一条——
/// 所以走仓储的 `rename_tag`（它在一个事务里删旧名并合并到新名）。
pub async fn tags_patch(
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

    let action = match field_opt_str(&body, "action") {
        Some(value) => value,
        None => return bad_request("缺少参数 action"),
    };
    let from = match super::field_str(&body, "from") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    let outcome = match action.as_str() {
        "rename" => match super::field_str(&body, "to") {
            Ok(to) => state.repo.rename_tag(auth.user.id, &from, &to).await,
            Err(resp) => return resp,
        },
        "delete" => state.repo.delete_tag(auth.user.id, &from).await,
        other => return bad_request(format!("不支持的标签操作：{other}")),
    };

    match outcome {
        Ok(()) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "csrf_token": cookie_value(&headers, CSRF_COOKIE),
        }))),
        Err(err) => crate::http::error_response(err.into()),
    }
}

/// `GET /api/v1/fs/tree`：目录树的虚拟视角（不读文件系统）。
///
/// `depth` 默认 2、上限 5 由领域层夹取；路由只负责把 `album` 参数
/// 翻译成 slug，并保留 `cursor` 以便将来分页。
pub async fn fs_tree(
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
    if let Some(auth) = auth.as_ref() {
        if let Err(resp) = super::ensure_scope(auth, "read") {
            return resp;
        }
    }

    let uid = parse_str(&query, "uid");
    let viewer = viewer_for(auth.as_ref(), uid.is_some());
    let depth = parse_i64(&query, "depth").unwrap_or(2);

    finish::<pixora_core::images::FsTreePayload>(
        state
            .fs_tree(
                &viewer,
                uid,
                parse_str(&query, "album"),
                depth,
                parse_str(&query, "cursor"),
            )
            .await,
    )
}
