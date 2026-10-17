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
    /// 打乱排序的种子（ 时用）。0 = 让服务端随机挑一个
    pub seed: u64,
    pub ids: Option<Vec<i64>>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

impl AppState {
    /// 图片列表。
    pub async fn list_images(
        &self,
        viewer: &Viewer,
        q: &ListQuery,
    ) -> Result<Page<ImageItemPayload>> {
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
            // 排序交给仓储层解析：默认是打乱，客户端给了 sort 就按它来
            sort: ImageSort::parse(q.sort.as_deref().unwrap_or("shuffle")),
            seed: q.seed,
        };

        let result = self.repo.list_images(&filter, request).await?;
        let total = result.total;
        let items = self.shape_items(viewer, result.items).await?;
        Ok(Page::new(items, total, request))
    }

    /// 广场：只捞公开图片。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_plaza(
        &self,
        query: Option<&str>,
        tag: Option<&str>,
        sort: Option<&str>,
        seed: u64,
        page: Option<i64>,
        per_page: Option<i64>,
    ) -> Result<Page<ImageItemPayload>> {
        let request = PageRequest::new(page, per_page, 24, 100);
        let filter = ImageFilter {
            public_only: true,
            query: query.map(str::to_string),
            tag: tag.map(str::to_string),
            // 广场默认是随机顺序：每次进来看到的都该是新的组合
            sort: ImageSort::parse(sort.unwrap_or("shuffle")),
            seed,
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
            .list_images(
                &siblings_filter,
                PageRequest::new(Some(1), Some(20), 20, 20),
            )
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

        if let Some(Some(value)) = &title {
            if value.chars().count() > 255 {
                return Err(AppError::bad_request("标题不能超过 255 个字符"));
            }
        }

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
                let moved = self
                    .repo
                    .move_images(&owned_ids, owner_id, album_id)
                    .await?;
                Ok(moved)
            }
            BatchAction::Tag => {
                let tags = tags.ok_or_else(|| AppError::bad_request("缺少标签"))?;
                let patch = ImagePatch {
                    tags: Some(tags),
                    ..Default::default()
                };
                Ok(self
                    .repo
                    .update_images(&owned_ids, owner_id, &patch)
                    .await?)
            }
            BatchAction::Visibility => {
                let value = visibility.ok_or_else(|| AppError::bad_request("缺少可见性"))?;
                let patch = ImagePatch {
                    visibility: Some(value),
                    ..Default::default()
                };
                Ok(self
                    .repo
                    .update_images(&owned_ids, owner_id, &patch)
                    .await?)
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

        // 没有归属的图片理论上不该再出现（移动一律落进默认相册，启动时还有
        // 一次收编），留这一格只是兜底：真有历史遗留也让它在视图里看得见
        let (loose_files, loose_bytes) = aggregates.get(&None).copied().unwrap_or((0, 0));
        if loose_files > 0 && (filter_slug.is_none() || filter_slug.as_deref() == Some("")) {
            nodes.insert(
                0,
                FsNodePayload {
                    node_type: "root".into(),
                    id: None,
                    name: "未归档".into(),
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
    async fn resolve_owner(
        &self,
        viewer: &Viewer,
        requested_uid: Option<&str>,
    ) -> Result<Option<i64>> {
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

    async fn resolve_owner_uid(
        &self,
        viewer: &Viewer,
        requested_uid: Option<&str>,
    ) -> Result<Option<String>> {
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
        let actor = viewer.user().ok_or_else(|| AppError::Unauthorized)?;
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
                        by_album: false,
                    },
                    Utc::now(),
                )
                .await?;

            // 引用计数归零时不在这里删物理文件：交给 GC 统一处理，
            // 免得把「刚上传、引用还没建好」的文件误删
            removed += 1;
        }

        if removed > 0 {
            // 回收站关着（保留 0 天）时删除就是彻底的：墓碑当场清掉，
            // 否则它们会以「引用计数为 0 但外键还在」的状态一直挂着
            if self.settings.retention_days().await? == 0 {
                for image in images {
                    if let Ok(Some(targets)) = self.repo.purge_image_row(image.id).await {
                        self.reclaim_purged(vec![targets]).await;
                    }
                }
            }
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

    // ---------- 回收站 ----------

    /// 回收站的图片列表（形状与普通列表一致，前端同一套渲染）。
    pub async fn deleted_images(
        &self,
        viewer: &Viewer,
        page: PageRequest,
    ) -> Result<(Vec<ImageItemPayload>, i64, i64)> {
        let owner = viewer.user().ok_or(AppError::Unauthorized)?;
        let result = self.repo.list_deleted_images(owner.id, page).await?;
        let retention = self.settings.retention_days().await?;
        let items = self.shape_items(viewer, result.items).await?;
        Ok((items, result.total, retention))
    }

    /// 从回收站恢复：引用计数、账号用量、相册张数、镜像链接一起回到原样。
    pub async fn restore_images(&self, viewer: &Viewer, ids: &[i64]) -> Result<u64> {
        if ids.is_empty() {
            return Err(AppError::bad_request("没有选中任何图片"));
        }
        if ids.len() > 1000 {
            return Err(AppError::bad_request("单次最多操作 1000 张"));
        }
        let actor_id = viewer.id();
        let mut restored = 0u64;
        for id in ids {
            let Some(image) = self.repo.find_image(*id).await? else {
                continue;
            };
            if image.deleted_at.is_none() {
                continue; // 本来就没删
            }
            let actor = viewer.user().ok_or(AppError::Unauthorized)?;
            if image.owner_id != actor.id && !actor.is_admin() {
                continue;
            }
            let Some(blob) = self.repo.find_blob_by_id(image.blob_id).await? else {
                continue;
            };
            self.repo
                .restore_image_reference(&image, blob.size, true)
                .await?;

            // 镜像链接：恢复之后这张图该重新出现在相册目录里
            if let (Ok(Some(owner)), Some(album_id)) = (
                self.repo.find_user_by_id(image.owner_id).await,
                image.album_id,
            ) {
                if let Ok(Some(album)) = self.repo.find_album_by_id(album_id).await {
                    self.link_view_for(
                        &blob.storage_driver,
                        blob.backend_id,
                        &blob.storage_key,
                        &owner.uid,
                        &album.slug,
                        &blob.sha256,
                        &blob.ext,
                    )
                    .await;
                }
            }
            restored += 1;
        }
        if restored > 0 {
            self.audit_detail(
                actions::RECYCLE_RESTORE,
                actor_id,
                format!("images:{restored}"),
                serde_json::json!({ "restored": restored, "ids": ids.iter().take(20).collect::<Vec<_>>() }),
                None,
            )
            .await;
        }
        Ok(restored)
    }

    /// 立即回收一个已经没有引用的实体（原图或缩略图）。
    ///
    /// 与后台 GC 共用同一套顺序：**先删物理文件、再删数据库行**。区别只在
    /// 不看宽限期——这个入口由「彻底删除」这类明确的销毁动作调用，那时引用
    /// 已经归零、墓碑也清干净了，宽限期要保护的那道「登记实体 → 建引用」
    /// 时间窗压根不适用。
    ///
    /// 两道保险仍然在：`ref_count > 0`（还有别的引用）与「还有任何图片行
    /// 引用它」（含回收站里的墓碑，恢复还要那份字节）时都不动它。
    pub async fn reclaim_blob_now(&self, blob_id: i64) -> Result<bool> {
        let Some(blob) = self.repo.find_blob_by_id(blob_id).await? else {
            return Ok(false);
        };
        // 与上传去重共用同一把内容哈希分片锁：删除期间不允许新引用挂上来。
        let _blob_guard = self.blob_lock(&blob.sha256).lock().await;
        let Some(blob) = self.repo.find_blob_by_id(blob_id).await? else {
            return Ok(false);
        };
        if blob.ref_count > 0 {
            return Ok(false);
        }
        if self.repo.count_image_refs_for_blob(blob.id).await? > 0 {
            return Ok(false);
        }

        match self
            .storage
            .delete_object(&blob.storage_driver, blob.backend_id, &blob.storage_key)
            .await
        {
            Ok(()) => {}
            Err(err) => {
                // 后端配置没了的孤儿（管理员删过后端）也照样收掉；
                // 远端对象够不到时留行并记日志，与 GC 同一套判断
                match self
                    .storage
                    .delete_orphan_object(&blob.storage_driver, &blob.storage_key)
                    .await
                {
                    Ok(true) => tracing::info!(
                        blob_id = blob.id,
                        driver = %blob.storage_driver,
                        "后端配置已删除，按孤儿对象回收"
                    ),
                    Ok(false) => {
                        tracing::warn!(
                            blob_id = blob.id,
                            driver = %blob.storage_driver,
                            error = %err,
                            "物理文件删除失败，留给后台回收重试"
                        );
                        return Ok(false);
                    }
                    Err(orphan_err) => {
                        tracing::warn!(
                            blob_id = blob.id,
                            driver = %blob.storage_driver,
                            error = %orphan_err,
                            "孤儿对象回收失败，留给后台回收重试"
                        );
                        return Ok(false);
                    }
                }
            }
        }

        Ok(self.repo.delete_blob_if_unreferenced(blob.id).await? > 0)
    }

    /// 一批墓碑删掉之后，把它们各自对应的实体当场收掉。
    ///
    /// 彻底删除之后不再需要管理员去「存储分布」页点「立即回收」：引用归零的
    /// 文件本来就该在删除的那一刻消失。收不掉的（远端失败）留着由后台重试。
    pub async fn reclaim_purged(
        &self,
        targets: Vec<pixora_repo::repo::images::PurgedBlobs>,
    ) -> u64 {
        let mut reclaimed = 0u64;
        for target in targets {
            if let Ok(true) = self.reclaim_blob_now(target.blob_id).await {
                reclaimed += 1;
            }
            if let Some(thumb_id) = target.thumb_blob_id {
                if let Ok(true) = self.reclaim_blob_now(thumb_id).await {
                    reclaimed += 1;
                }
            }
        }
        reclaimed
    }

    /// 彻底删除：墓碑行消失，物理文件当场回收（引用计数已归零）。
    pub async fn purge_images(&self, viewer: &Viewer, ids: &[i64]) -> Result<u64> {
        if ids.is_empty() {
            return Err(AppError::bad_request("没有选中任何图片"));
        }
        let actor = viewer.user().ok_or(AppError::Unauthorized)?.clone();
        let mut purged = 0u64;
        for id in ids {
            let Some(image) = self.repo.find_image(*id).await? else {
                continue;
            };
            if image.deleted_at.is_none() {
                continue; // 没进回收站的不在这里删
            }
            if image.owner_id != actor.id && !actor.is_admin() {
                continue;
            }
            // 镜像链接跟着走（相册还能找到就拆，找不到也没关系——目录是视图）
            if let (Ok(Some(owner)), Some(album_id)) = (
                self.repo.find_user_by_id(image.owner_id).await,
                image.album_id,
            ) {
                if let Ok(Some(blob)) = self.repo.find_blob_by_id(image.blob_id).await {
                    if let Ok(Some(album)) = self.repo.find_album_any(album_id).await {
                        self.unlink_view_for(
                            &blob.storage_driver,
                            blob.backend_id,
                            &owner.uid,
                            &album.slug,
                            &blob.sha256,
                            &blob.ext,
                        )
                        .await;
                        self.prune_view_dir(
                            &blob.storage_driver,
                            blob.backend_id,
                            &owner.uid,
                            &album.slug,
                        )
                        .await;
                    }
                }
            }
            if let Some(targets) = self.repo.purge_image_row(image.id).await? {
                purged += 1;
                self.reclaim_purged(vec![targets]).await;
            }
        }
        if purged > 0 {
            self.kick_gc();
            self.audit_detail(
                actions::RECYCLE_PURGE,
                Some(actor.id),
                format!("images:{purged}"),
                serde_json::json!({ "purged": purged, "ids": ids.iter().take(20).collect::<Vec<_>>() }),
                None,
            )
            .await;
        }
        Ok(purged)
    }

    /// 清空当前账号的回收站。
    pub async fn empty_bin(&self, viewer: &Viewer) -> Result<u64> {
        let actor = viewer.user().ok_or(AppError::Unauthorized)?.clone();
        // 逐张走彻底删除：镜像链接与审计都要留痕
        let page = PageRequest::new(Some(1), Some(500), 500, 500);
        let listed = self.repo.list_deleted_images(actor.id, page).await?;
        let ids: Vec<i64> = listed.items.iter().map(|item| item.id).collect();
        let purged = self.purge_images(viewer, &ids).await?;
        // 页面之外还有（超过 500 张）的时候再来一轮，直到清干净
        if listed.total > ids.len() as i64 {
            let owner = viewer.user().ok_or(AppError::Unauthorized)?;
            let more = self.repo.purge_deleted_images(owner.id, 20_000).await?;
            if !more.is_empty() {
                self.reclaim_purged(more.clone()).await;
            }
            return Ok(purged + more.len() as u64);
        }
        Ok(purged)
    }

    /// 把仓储的视图转成接口形状，并按访问者裁剪字段。
    async fn shape_items(
        &self,
        viewer: &Viewer,
        views: Vec<ImageView>,
    ) -> Result<Vec<ImageItemPayload>> {
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
        if !ext.is_empty()
            && crate::media::normalize_ext(ext) != crate::media::normalize_ext(&blob.ext)
        {
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
