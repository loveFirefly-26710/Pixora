//! 图片服务：列表、详情、修改、删除、批量操作、广场、目录树。
//!
//! 这里是「谁能看到什么」的收口处。三条不变量：
//!
//!   1. **未登录用户拿不到原始文件名**。`original_name` 会置空——它可能包含
//!      用户的真名或设备路径，属于内部信息。
//!   2. **管理员跨用户访问要写审计日志**。这是设计稿点名的合规要求，且必须在
//!      服务层做，不能靠前端隐藏。
//!   3. **删除只走引用计数**。任何绕过计数直接删物理文件的代码路径都是隐患，
//!      因为同一份文件可能被多个用户引用。

use std::collections::HashMap;

use chrono::Utc;
use pixora_repo::model::{visibility, Image, ImageView, User};
use pixora_repo::repo::images::{ImageDeletion, ImageFilter, ImagePatch, ImageSort};
use pixora_repo::{Page, PageRequest};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::AppState;

/// 批量操作的目标范围。`album_id` 为 `None` 表示不限相册（该账号的全部图片）。
#[derive(Debug, Clone, Copy)]
pub struct BatchScope {
    pub album_id: Option<i64>,
}

/// 按范围批量操作时一次最多涉及多少张：比 id 列表的上限宽松，
/// 但也不能让一次请求把整库拖进内存。
const BATCH_SCOPE_LIMIT: i64 = 10_000;

/// 访问者。页面权限判断集中用这个，别在各处 `if user.role == "admin"`。
#[derive(Debug, Clone)]
pub enum Viewer {
    Anonymous,
    User(User),
    /// 管理员正在查看某个用户的空间（用户视图）
    AdminViewing(User),
}

impl Viewer {
    pub fn user(&self) -> Option<&User> {
        match self {
            Viewer::Anonymous => None,
            Viewer::User(user) | Viewer::AdminViewing(user) => Some(user),
        }
    }

    pub fn id(&self) -> Option<i64> {
        self.user().map(|user| user.id)
    }

    pub fn is_admin(&self) -> bool {
        self.user().map(User::is_admin).unwrap_or(false)
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self, Viewer::Anonymous)
    }

    /// 目标空间的归属者：自己看自己，或管理员指定了别人。
    pub fn target_owner_id(&self, requested_uid: Option<&str>) -> Result<Option<String>> {
        match (self, requested_uid) {
            (_, None) => Ok(self.user().map(|user| user.uid.clone())),
            (Viewer::AdminViewing(user), Some(uid)) if user.is_admin() => Ok(Some(uid.to_string())),
            (viewer, Some(_)) if viewer.is_admin() => Ok(requested_uid.map(str::to_string)),
            (_, Some(_)) => Err(AppError::forbidden("只有管理员可以查看其他用户的空间")),
        }
    }
}

/// 契约 4.1 的 `ImageItem`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageItemPayload {
    pub id: i64,
    pub title: Option<String>,
    pub original_name: Option<String>,
    pub sha256: String,
    pub short_hash: String,
    pub ext: String,
    pub mime: String,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub album_id: Option<i64>,
    pub album_name: Option<String>,
    pub owner_uid: String,
    pub owner_id: i64,
    pub visibility: String,
    pub views: i64,
    pub upload_time: String,
    pub tags: Vec<String>,
    pub has_thumb: bool,
    pub url: String,
    pub thumb_url: Option<String>,
}

/// 详情在上面基础上多几个字段。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageDetailPayload {
    #[serde(flatten)]
    pub item: ImageItemPayload,
    pub storage_driver: String,
    pub created_at: String,
    pub original_filename_visible: bool,
    pub album: Option<AlbumRef>,
    pub siblings: Vec<SiblingRef>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AlbumRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SiblingRef {
    pub id: i64,
    pub thumb_url: Option<String>,
}

/// 批量操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    Delete,
    Move,
    Tag,
    Visibility,
}

impl BatchAction {
    pub fn parse(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "delete" => Ok(Self::Delete),
            "move" => Ok(Self::Move),
            "tag" => Ok(Self::Tag),
            "visibility" => Ok(Self::Visibility),
            other => Err(AppError::bad_request(format!("不支持的批量操作：{other}"))),
        }
    }
}

/// 列表的查询参数（与契约 4.1 的表格对应）。
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub uid: Option<String>,
    pub album_id: Option<i64>,
    pub query: Option<String>,
    pub tag: Option<String>,
    pub sort: Option<String>,
    pub ids: Option<Vec<i64>>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

impl AppState {
    /// 图片列表。
    pub async fn list_images(&self, viewer: &Viewer, q: &ListQuery) -> Result<Page<ImageItemPayload>> {
        let owner_id = self.resolve_owner(viewer, q.uid.as_deref()).await?;
        let request = PageRequest::new(q.page, q.per_page, 24, 100);

        let filter = ImageFilter {
            owner_id,
            album_id: q.album_id,
            public_only: viewer.is_anonymous(),
            query: q.query.clone(),
            tag: q.tag.clone(),
            ids: q.ids.clone(),
            include_deleted: false,
            sort: ImageSort::parse(q.sort.as_deref().unwrap_or("new")),
        };

        let result = self.repo.list_images(&filter, request).await?;
        let total = result.total;
        let items = self.shape_items(viewer, result.items).await?;
        Ok(Page::new(items, total, request))
    }

    /// 广场：只捞公开图片。
    pub async fn list_plaza(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        sort: Option<&str>,
        page: Option<i64>,
        per_page: Option<i64>,
    ) -> Result<Page<ImageItemPayload>> {
        let request = PageRequest::new(page, per_page, 24, 100);
        let filter = ImageFilter {
            public_only: true,
            query: query.map(str::to_string),
            tag: tag.map(str::to_string),
            sort: ImageSort::parse(sort.unwrap_or("new")),
            ..Default::default()
        };
        let result = self.repo.list_images(&filter, request).await?;
        let total = result.total;
        let items = self.shape_items(&Viewer::Anonymous, result.items).await?;
        Ok(Page::new(items, total, request))
    }

    /// 单张详情。非本人的公开图片会累加浏览量。
    pub async fn image_detail(&self, viewer: &Viewer, id: i64) -> Result<ImageDetailPayload> {
        let view = self
            .repo
            .find_image_view(id)
            .await?
            .ok_or_else(|| AppError::not_found("图片"))?;

        // 能看的前提：自己的、管理员的、或它是公开的
        let is_owner = viewer.id() == Some(view.owner_id);
        if !is_owner && !viewer.is_admin() && !view.is_public() {
            return Err(AppError::not_found("图片"));
        }

        if !is_owner && view.is_public() {
            // 浏览量只对「别人看我的公开图」有意义，失败不影响返回
            if let Err(err) = self.repo.increment_views(id).await {
                tracing::debug!(error = %err, "浏览量累加失败");
            }
        }

        let item = self.shape_item(viewer, view.clone()).await?;
        let album = match view.album_id {
            Some(album_id) => self
                .repo
                .find_album_by_id(album_id)
                .await?
                .map(|album| AlbumRef {
                    id: album.id,
                    name: album.name,
                }),
            None => None,
        };

        // 同相册的相邻图片，预览浮层的缩略图条要用
        let siblings_filter = ImageFilter {
            owner_id: Some(view.owner_id),
            album_id: view.album_id,
            public_only: viewer.is_anonymous(),
            ..Default::default()
        };
        let siblings = self
            .repo
            .list_images(&siblings_filter, PageRequest::new(Some(1), Some(20), 20, 20))
            .await?;
        let mut sibling_views: Vec<SiblingRef> = Vec::new();
        for sibling in siblings.items {
            sibling_views.push(SiblingRef {
                id: sibling.id,
                thumb_url: if sibling.has_thumb() {
                    Some(self.direct_url(&sibling.sha256, &sibling.ext, true).await)
                } else {
                    None
                },
            });
        }

        Ok(ImageDetailPayload {
            item,
            storage_driver: view.storage_driver.clone(),
            created_at: view.upload_time.to_rfc3339(),
            original_filename_visible: !viewer.is_anonymous(),
            album,
            siblings: sibling_views,
        })
    }

    /// 修改单张：标题、可见性、所属相册、标签。
    pub async fn update_image(
        &self,
        viewer: &Viewer,
        id: i64,
        title: Option<Option<String>>,
        visibility_value: Option<String>,
        album_id: Option<Option<i64>>,
        tags: Option<Vec<String>>,
    ) -> Result<()> {
        let image = self.require_touchable(viewer, id).await?;

        if let Some(album_id) = album_id {
            if let Some(album_id) = album_id {
                let album = self
                    .repo
                    .find_album_by_id(album_id)
                    .await?
                    .ok_or_else(|| AppError::not_found("相册"))?;
                if album.owner_id != image.owner_id {
                    return Err(AppError::bad_request("只能移动到自己的相册"));
                }
            }
            // 改所属相册要同步两个相册的计数
            if image.album_id != album_id {
                self.repo
                    .move_images(&[id], image.owner_id, album_id)
                    .await?;
            }
        }

        let patch = ImagePatch {
            title,
            visibility: visibility_value.map(|value| {
                if value.eq_ignore_ascii_case(visibility::PUBLIC) {
                    visibility::PUBLIC.to_string()
                } else {
                    visibility::PRIVATE.to_string()
                }
            }),
            album_id: None, // 上面已经单独处理
            tags,
        };
        self.repo.update_image(id, &patch).await?;
        Ok(())
    }

    /// 删除单张：软删引用 + 递减计数 + 拆镜像链接。
    pub async fn delete_image(&self, viewer: &Viewer, id: i64) -> Result<()> {
        let image = self.require_touchable(viewer, id).await?;
        self.delete_images_inner(viewer, &[image]).await?;
        Ok(())
    }

    /// 批量操作。
    pub async fn batch_images(
        &self,
        viewer: &Viewer,
        action: BatchAction,
        ids: &[i64],
        scope: Option<BatchScope>,
        album_id: Option<i64>,
        tags: Option<Vec<String>>,
        visibility: Option<String>,
    ) -> Result<u64> {
        // 目标要么是前端选中的 id 列表，要么是一个范围（整个相册 / 全部图片）。
        // 范围这条路是为了「整相册一键操作」：几千张图不必先拉到浏览器再发回来，
        // 也绕开了 id 列表的长度上限。
        let ids: Vec<i64> = match scope {
            Some(scope) => {
                let owner = viewer.user().ok_or(AppError::Unauthorized)?;
                let resolved = self
                    .repo
                    .image_ids_in_scope(owner.id, scope.album_id, BATCH_SCOPE_LIMIT)
                    .await?;
                if resolved.is_empty() {
                    return Ok(0);
                }
                resolved
            }
            None => {
                if ids.is_empty() {
                    return Err(AppError::bad_request("没有选中任何图片"));
                }
                if ids.len() > 1000 {
                    return Err(AppError::bad_request("单次最多操作 1000 张"));
                }
                ids.to_vec()
            }
        };


        // 先把可操作的图片筛出来，越权的直接跳过而不是整批失败
        let mut owned: Vec<Image> = Vec::new();
        for id in &ids {
            if let Some(image) = self.require_touchable_optional(viewer, *id).await? {
                owned.push(image);
            }
        }
        if owned.is_empty() {
            return Err(AppError::forbidden("选中的图片都不属于当前账号"));
        }
        let owned_ids: Vec<i64> = owned.iter().map(|image| image.id).collect();
        let owner_id = owned[0].owner_id;

        match action {
            BatchAction::Delete => Ok(self.delete_images_inner(viewer, &owned).await?),
            BatchAction::Move => {
                let moved = self.repo.move_images(&owned_ids, owner_id, album_id).await?;
                Ok(moved)
            }
            BatchAction::Tag => {
                let tags = tags.ok_or_else(|| AppError::bad_request("缺少标签"))?;
                let patch = ImagePatch {
                    tags: Some(tags),
                    ..Default::default()
                };
                Ok(self.repo.update_images(&owned_ids, owner_id, &patch).await?)
            }
            BatchAction::Visibility => {
                let value = visibility.ok_or_else(|| AppError::bad_request("缺少可见性"))?;
                let patch = ImagePatch {
                    visibility: Some(value),
                    ..Default::default()
                };
                Ok(self.repo.update_images(&owned_ids, owner_id, &patch).await?)
            }
        }
    }

    /// 目录递归查询（`GET /api/v1/fs/tree`）。
    pub async fn fs_tree(
        &self,
        viewer: &Viewer,
        uid: Option<&str>,
        album_slug: Option<&str>,
        depth: i64,
        cursor: Option<&str>,
    ) -> Result<FsTreePayload> {
        let owner_uid = self
            .resolve_owner_uid(viewer, uid)
            .await?
            .ok_or_else(|| AppError::bad_request("需要指定用户"))?;
        let depth = depth.clamp(1, 5);

        let owner = self
            .repo
            .find_user_by_uid(&owner_uid)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;
        // 普通用户只能查自己；管理员查别人要留审计
        if viewer.id() != Some(owner.id) {
            if !viewer.is_admin() {
                return Err(AppError::forbidden("只能查询自己的数据目录"));
            }
            let admin_id = viewer.id().unwrap_or_default();
            self.audit_cross_user(admin_id, &owner_uid, "fs_tree", None)
                .await;
        }

        let albums = self.repo.list_albums_all(owner.id).await?;
        let aggregates: HashMap<Option<i64>, (i64, i64)> = self
            .repo
            .album_aggregates(owner.id)
            .await?
            .into_iter()
            .map(|row| (row.album_id, (row.files, row.bytes)))
            .collect();

        let mut nodes: Vec<FsNodePayload> = Vec::new();
        let filter_slug = album_slug.map(|value| value.to_string());
        for album in &albums {
            if let Some(slug) = filter_slug.as_deref() {
                if album.slug != slug {
                    continue;
                }
            }
            let (files, size) = aggregates.get(&Some(album.id)).copied().unwrap_or((0, 0));
            let mut images: Vec<FsFilePayload> = Vec::new();
            if depth >= 2 {
                let page = self
                    .repo
                    .list_images(
                        &ImageFilter {
                            owner_id: Some(owner.id),
                            album_id: Some(album.id),
                            public_only: viewer.is_anonymous(),
                            include_deleted: false,
                            sort: ImageSort::NameAsc,
                            ..Default::default()
                        },
                        PageRequest::new(Some(1), Some(200), 200, 500),
                    )
                    .await?;
                for image in page.items {
                    images.push(FsFilePayload {
                        id: image.id,
                        name: format!("{}.{}", image.sha256, image.ext),
                        size: image.size,
                        upload_time: image.upload_time.to_rfc3339(),
                    });
                }
            }
            nodes.push(FsNodePayload {
                node_type: "album".into(),
                id: Some(album.id),
                name: album.name.clone(),
                slug: album.slug.clone(),
                files,
                size,
                images,
            });
        }

        // 未归档的图片也算一个节点，否则「全部图片」的数量对不上
        let (loose_files, loose_bytes) = aggregates.get(&None).copied().unwrap_or((0, 0));
        if filter_slug.is_none() || filter_slug.as_deref() == Some("") {
            nodes.insert(
                0,
                FsNodePayload {
                    node_type: "root".into(),
                    id: None,
                    name: "未分类".into(),
                    slug: String::new(),
                    files: loose_files,
                    size: loose_bytes,
                    images: Vec::new(),
                },
            );
        }

        let files: i64 = nodes.iter().map(|node| node.files).sum();
        let size: i64 = nodes.iter().map(|node| node.size).sum();
        let path = match album_slug {
            Some(slug) => format!("/{owner_uid}/{slug}"),
            None => format!("/{owner_uid}"),
        };

        Ok(FsTreePayload {
            uid: owner_uid,
            path,
            files,
            size,
            nodes,
            next_cursor: cursor.and(None),
        })
    }

    // ============================================================
    // 内部工具
    // ============================================================

    /// 解析「要看谁的空间」。
    async fn resolve_owner(&self, viewer: &Viewer, requested_uid: Option<&str>) -> Result<Option<i64>> {
        let uid = self.resolve_owner_uid(viewer, requested_uid).await?;
        match uid {
            None => Ok(None),
            Some(uid) => {
                let user = self
                    .repo
                    .find_user_by_uid(&uid)
                    .await?
                    .ok_or_else(|| AppError::not_found("用户"))?;
                Ok(Some(user.id))
            }
        }
    }

    async fn resolve_owner_uid(&self, viewer: &Viewer, requested_uid: Option<&str>) -> Result<Option<String>> {
        match (viewer, requested_uid) {
            (_, None) => Ok(viewer.user().map(|user| user.uid.clone())),
            (viewer, Some(uid)) if viewer.is_admin() => {
                if let Some(actor) = viewer.user() {
                    let is_other = actor.uid != uid;
                    if is_other {
                        self.audit_cross_user(actor.id, uid, "images", None).await;
                    }
                }
                Ok(Some(uid.to_string()))
            }
            (viewer, Some(uid)) if viewer.user().map(|user| user.uid.as_str()) == Some(uid) => {
                Ok(Some(uid.to_string()))
            }
            (_, Some(_)) => Err(AppError::forbidden("只能查看自己的图片")),
        }
    }

    /// 回读并校验归属。越权按「不存在」处理，不泄露资源是否存在。
    async fn require_touchable(&self, viewer: &Viewer, id: i64) -> Result<Image> {
        self.require_touchable_optional(viewer, id)
            .await?
            .ok_or_else(|| AppError::not_found("图片"))
    }

    async fn require_touchable_optional(&self, viewer: &Viewer, id: i64) -> Result<Option<Image>> {
        let Some(image) = self.repo.find_image(id).await? else {
            return Ok(None);
        };
        if image.deleted_at.is_some() {
            return Ok(None);
        }
        let actor = viewer
            .user()
            .ok_or_else(|| AppError::Unauthorized)?;
        if image.owner_id != actor.id && !actor.is_admin() {
            return Ok(None);
        }
        Ok(Some(image))
    }

    /// 逐张删除：软删引用 + 递减计数 + 拆镜像链接 + 写审计。
    async fn delete_images_inner(&self, viewer: &Viewer, images: &[Image]) -> Result<u64> {
        let actor_id = viewer.id();
        let mut removed = 0u64;

        for image in images {
            let Some(blob) = self.repo.find_blob_by_id(image.blob_id).await? else {
                continue;
            };
            // 镜像链接要拆掉：目录树是给人看的视图，图片删了目录里不该还留着
            let owner = self.repo.find_user_by_id(image.owner_id).await?;
            if let (Some(owner), Some(album_id)) = (owner.as_ref(), image.album_id) {
                if let Some(album) = self.repo.find_album_by_id(album_id).await? {
                    self.unlink_view_for(
                        &blob.storage_driver,
                        blob.backend_id,
                        &owner.uid,
                        &album.slug,
                        &blob.sha256,
                        &blob.ext,
                    )
                    .await;
                }
            }

            self.repo
                .delete_image_reference(
                    &ImageDeletion {
                        image_id: image.id,
                        owner_id: image.owner_id,
                        album_id: image.album_id,
                        blob_id: image.blob_id,
                        thumb_blob_id: image.thumb_blob_id,
                        size: blob.size,
                    },
                    Utc::now(),
                )
                .await?;

            // 引用计数归零时不在这里删物理文件：交给 GC 统一处理，
            // 免得把「刚上传、引用还没建好」的文件误删
            removed += 1;
        }

        if removed > 0 {
            // 最后一张引用可能刚刚消失：催一次回收，别让文件在磁盘上多躺半小时
            self.kick_gc();
            self.audit_detail(
                actions::DELETE_IMAGE,
                actor_id,
                format!("images:{}", images.len()),
                serde_json::json!({ "removed": removed, "ids": images.iter().map(|i| i.id).take(20).collect::<Vec<_>>() }),
                None,
            )
            .await;
        }
        Ok(removed)
    }

    /// 把仓储的视图转成接口形状，并按访问者裁剪字段。
    async fn shape_items(&self, viewer: &Viewer, views: Vec<ImageView>) -> Result<Vec<ImageItemPayload>> {
        let mut items = Vec::with_capacity(views.len());
        for view in views {
            items.push(self.shape_item(viewer, view).await?);
        }
        Ok(items)
    }

    async fn shape_item(&self, viewer: &Viewer, view: ImageView) -> Result<ImageItemPayload> {
        let url = self.direct_url(&view.sha256, &view.ext, false).await;
        let thumb_url = if view.has_thumb() {
            Some(self.direct_url(&view.sha256, &view.ext, true).await)
        } else {
            None
        };
        Ok(ImageItemPayload {
            id: view.id,
            title: view.title.clone(),
            // 未登录用户拿不到原始文件名：它可能带真名或设备路径
            original_name: if viewer.is_anonymous() {
                None
            } else {
                Some(view.original_name.clone())
            },
            short_hash: view.sha256.chars().take(8).collect(),
            sha256: view.sha256.clone(),
            ext: view.ext.clone(),
            mime: view.mime_type.clone(),
            size: view.size,
            width: view.width,
            height: view.height,
            album_id: view.album_id,
            album_name: view.album_name.clone(),
            owner_uid: view.owner_uid.clone(),
            owner_id: view.owner_id,
            visibility: view.visibility.clone(),
            views: view.views,
            upload_time: view.upload_time.to_rfc3339(),
            tags: view.tags.clone(),
            has_thumb: view.has_thumb(),
            url,
            thumb_url,
        })
    }

    /// 直链下载用的元信息。
    pub async fn direct_link_info(&self, sha256: &str, ext: &str) -> Result<DirectLink> {
        let Some(blob) = self
            .repo
            .find_blob_by_sha(sha256, pixora_repo::model::storage_kind::ORIGINAL)
            .await?
        else {
            return Err(AppError::not_found("图片"));
        };
        // 扩展名与库里记录不一致 → 当作不存在，防止用伪造扩展名探测
        if !ext.is_empty() && crate::media::normalize_ext(ext) != crate::media::normalize_ext(&blob.ext) {
            return Err(AppError::not_found("图片"));
        }
        Ok(DirectLink {
            blob_id: blob.id,
            storage_driver: blob.storage_driver.clone(),
            storage_key: blob.storage_key.clone(),
            backend_id: blob.backend_id,
            mime_type: blob.mime_type.clone(),
            size: blob.size,
            sha256: blob.sha256.clone(),
            ext: blob.ext.clone(),
        })
    }
}

/// 直链所需的元信息。
#[derive(Debug, Clone)]
pub struct DirectLink {
    pub blob_id: i64,
    pub storage_driver: String,
    pub storage_key: String,
    pub backend_id: Option<i64>,
    pub mime_type: String,
    pub size: i64,
    pub sha256: String,
    pub ext: String,
}

/// 目录树节点。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FsNodePayload {
    #[serde(rename = "type")]
    pub node_type: String,
    pub id: Option<i64>,
    pub name: String,
    pub slug: String,
    pub files: i64,
    pub size: i64,
    pub images: Vec<FsFilePayload>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FsFilePayload {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub upload_time: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FsTreePayload {
    pub uid: String,
    pub path: String,
    pub files: i64,
    pub size: i64,
    pub nodes: Vec<FsNodePayload>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_action_parsing() {
        assert_eq!(BatchAction::parse("delete").unwrap(), BatchAction::Delete);
        assert_eq!(BatchAction::parse("MOVE").unwrap(), BatchAction::Move);
        assert!(BatchAction::parse("explode").is_err());
    }

    #[test]
    fn anonymous_viewer_cannot_resolve_others() {
        let viewer = Viewer::Anonymous;
        assert!(viewer.target_owner_id(Some("1001")).is_err());
        assert_eq!(viewer.target_owner_id(None).unwrap(), None);
    }
}
