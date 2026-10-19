//! 相册服务。
//!
//! 相册是「用户 → 相册 → 图片」中间的那一层，`slug` 决定物理目录名，
//! 所以新建时要净化：目录名要能跨平台安全落盘，也不能带出路径穿越。
//!
//! 删除相册**不删图片**：相册与其中的图片一起进回收站，保留期内可整份恢复。
//! 「删相册顺手删图」是很多图床的默认行为，也是最容易造成数据损失的按钮，
//! 这里刻意不这么做。

use chrono::Utc;
use pixora_repo::model::{visibility, Album, User};
use pixora_repo::repo::albums::{AlbumPatch, NewAlbum};
use pixora_storage::cas::sanitize_slug;

use crate::error::{AppError, Result};
use crate::images::Viewer;
use crate::AppState;

/// 契约 4.1 的 `GET /api/v1/albums` 响应。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AlbumListPayload {
    pub all: AlbumTotals,
    pub albums: Vec<AlbumPayload>,
    pub usage: AlbumUsage,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AlbumTotals {
    pub id: Option<i64>,
    pub name: String,
    pub image_count: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AlbumPayload {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub visibility: String,
    pub image_count: i64,
    pub bytes: i64,
    pub cover_url: Option<String>,
    pub sort_order: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AlbumUsage {
    pub used_bytes: i64,
    pub quota_bytes: i64,
}

impl AppState {
    /// 相册列表：侧边栏要全量，所以不分页。
    pub async fn albums_payload(&self, viewer: &Viewer, uid: Option<&str>) -> Result<AlbumListPayload> {
        let owner = self.albums_owner(viewer, uid).await?;
        let albums = self.repo.list_albums_all(owner.id).await?;
        let aggregates: std::collections::HashMap<Option<i64>, (i64, i64)> = self
            .repo
            .album_aggregates(owner.id)
            .await?
            .into_iter()
            .map(|row| (row.album_id, (row.files, row.bytes)))
            .collect();

        let mut payloads = Vec::with_capacity(albums.len());
        for album in albums {
            let (count, bytes) = aggregates.get(&Some(album.id)).copied().unwrap_or((0, 0));
            payloads.push(self.album_payload(&album, count, bytes).await);
        }

        let (total_count, total_bytes) = self.album_totals(&aggregates);

        Ok(AlbumListPayload {
            all: AlbumTotals {
                id: None,
                name: "全部图片".into(),
                image_count: total_count,
                bytes: total_bytes,
            },
            albums: payloads,
            usage: AlbumUsage {
                used_bytes: owner.used_bytes.max(total_bytes),
                quota_bytes: owner.quota_bytes,
            },
        })
    }

    pub async fn create_album(
        &self,
        viewer: &Viewer,
        name: &str,
        slug: Option<&str>,
        description: Option<&str>,
        visibility_value: Option<&str>,
    ) -> Result<AlbumPayload> {
        let owner = viewer
            .user()
            .ok_or_else(|| AppError::Unauthorized)?
            .clone();

        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::bad_request("相册名称不能为空"));
        }
        if name.chars().count() > 64 {
            return Err(AppError::bad_request("相册名称不能超过 64 个字"));
        }

        let base_slug = slug.map(str::to_string).unwrap_or_else(|| name.to_string());
        let slug = sanitize_slug(&base_slug);
        if self.repo.slug_exists(owner.id, &slug).await? {
            return Err(AppError::Conflict(format!("已存在同名目录 {slug}")));
        }

        let album = self
            .repo
            .create_album(&NewAlbum {
                owner_id: owner.id,
                name: name.to_string(),
                slug,
                description: description.map(str::to_string),
                visibility: visibility_value
                    .filter(|value| value.eq_ignore_ascii_case(visibility::PUBLIC))
                    .map(|_| visibility::PUBLIC.to_string())
                    .unwrap_or_else(|| visibility::PRIVATE.to_string()),
                sort_order: 0,
            })
            .await?;

        Ok(self.album_payload(&album, 0, 0).await)
    }

    pub async fn update_album(
        &self,
        viewer: &Viewer,
        album_id: i64,
        name: Option<&str>,
        description: Option<Option<&str>>,
        visibility_value: Option<&str>,
        sort_order: Option<i64>,
        cover_blob_id: Option<Option<i64>>,
    ) -> Result<()> {
        let album = self.require_owned_album(viewer, album_id).await?;

        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(AppError::bad_request("相册名称不能为空"));
            }
        }
        // 封面只能从相册主人自己的图片里挑。否则随便填一个 blob id 就能通过
        // 返回的缩略图地址判断「别处是否存在这张图」。
        if let Some(Some(blob_id)) = cover_blob_id {
            if !self.repo.owner_has_blob(album.owner_id, blob_id).await? {
                return Err(AppError::bad_request("封面必须来自相册主人自己的图片"));
            }
        }

        let patch = AlbumPatch {
            name: name.map(|value| value.trim().to_string()),
            description: description.map(|value| value.map(str::to_string)),
            visibility: visibility_value.map(|value| {
                if value.eq_ignore_ascii_case(visibility::PUBLIC) {
                    visibility::PUBLIC.to_string()
                } else {
                    visibility::PRIVATE.to_string()
                }
            }),
            sort_order,
            cover_blob_id,
        };
        self.repo.update_album(album_id, &patch).await?;
        Ok(())
    }

    /// 删除相册：图片解除关联，不跟着删。
    /// 把相册移进回收站：相册与它里面的图片一起进去，随时能整份恢复。
    ///
    /// 两处与「删除单张图」保持一致：每张图都走 `delete_image_reference`
    /// （引用计数、账号用量、相册张数按同一套规则走），镜像链接拆掉、空目录收掉。
    /// 只是图片标记成「随相册删除」，恢复相册时只带回这一批。
    ///
    /// 回收站关着（保留 0 天）时直接彻底删：相册行与其中的图片一起消失。
    /// 把没有归属的图片收进各自的默认相册。返回收编的张数。
    ///
    /// 「未分类」这个状态已经取消（移动对话框里没有「移出相册」了），但早期
    /// 版本留下过 `album_id` 为空的行。默认相册不存在就顺手建一个——它本来就是
    /// 「上传不指定相册」的落脚点。
    pub async fn attach_orphan_images(&self) -> Result<u64> {
        let owners = self.repo.owners_with_unattached_images().await?;
        let mut total = 0u64;
        for owner_id in owners {
            let album = self.repo.insert_default_album(owner_id, "默认相册").await?;
            let moved = self.repo.attach_images_to_album(owner_id, album.id).await?;
            if moved > 0 {
                tracing::info!(owner_id, moved, "把没有归属的图片收进默认相册");
            }
            total += moved;
        }
        Ok(total)
    }

    pub async fn delete_album(&self, viewer: &Viewer, album_id: i64) -> Result<i64> {
        let album = self.require_owned_album(viewer, album_id).await?;
        // 默认相册是「上传不指定相册」的落脚点，删掉之后没有重建它的入口，
        // 所以在这里拦住——前端隐藏按钮只是方便，真正的规则在服务端
        if album.slug == pixora_repo::repo::albums::DEFAULT_ALBUM_SLUG {
            return Err(AppError::bad_request(
                "默认相册不能删除：上传不指定相册时就落在它这里",
            ));
        }
        let actor_id = viewer.id();
        let owner = self.repo.find_user_by_id(album.owner_id).await?;
        let now = Utc::now();

        // 图片先进回收站：一张张按单张删除那套走，计数与链接才不会飘
        let images = self.repo.list_album_live_images(album_id).await?;
        let mut moved = 0i64;
        for image in &images {
            let Ok(Some(blob)) = self.repo.find_blob_by_id(image.blob_id).await else {
                continue;
            };
            if let Some(owner) = owner.as_ref() {
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
            self.repo
                .delete_image_reference(
                    &pixora_repo::repo::images::ImageDeletion {
                        image_id: image.id,
                        owner_id: image.owner_id,
                        album_id: image.album_id,
                        blob_id: image.blob_id,
                        thumb_blob_id: image.thumb_blob_id,
                        size: blob.size,
                        by_album: true,
                    },
                    now,
                )
                .await?;
            moved += 1;
        }
        if let Some(owner) = owner.as_ref() {
            // 目录只删空的：图片链接都拆完了，这里通常就空了
            self.prune_view_dir(
                pixora_storage::names::LOCAL,
                None,
                &owner.uid,
                &album.slug,
            )
            .await;
        }

        // 相册行本身软删：列表里消失，但恢复时还要靠它把图片带回来
        self.repo.soft_delete_album(album_id, now).await?;

        // 回收站关着就是彻底删：相册行与其中的墓碑一起清掉
        if self.settings.retention_days().await? == 0 {
            for image_id in self.repo.deleted_image_ids_in_album(album_id, 20_000).await? {
                if let Ok(Some(targets)) = self.repo.purge_image_row(image_id).await {
                    self.reclaim_purged(vec![targets]).await;
                }
            }
            self.repo.delete_album(album_id).await?;
        }

        if moved > 0 {
            self.audit_detail(
                crate::audit::actions::DELETE_IMAGE,
                actor_id,
                format!("album:{}", album.name),
                serde_json::json!({ "moved_to_bin": moved, "album_id": album_id }),
                None,
            )
            .await;
        }
        self.kick_gc();
        Ok(moved.max(album.image_count))
    }

    /// 从回收站恢复相册：相册行与「随它一起删掉」的图片一起回来。
    ///
    /// 只带回 `deleted_by_album` 的那一批——用户在这个相册里单独删掉的图，
    /// 是他自己删的，不该被这次恢复顺手捞回来。
    pub async fn restore_album(&self, viewer: &Viewer, album_id: i64) -> Result<i64> {
        let album = self.require_owned_album_any(viewer, album_id).await?;
        if album.deleted_at.is_none() {
            return Err(AppError::bad_request("这个相册不在回收站里"));
        }
        let actor_id = viewer.id();
        self.repo.restore_album(album_id).await?;

        let owner = self.repo.find_user_by_id(album.owner_id).await?;
        let mut restored = 0i64;
        for image in self.repo.list_album_binned_images(album_id).await? {
            let Ok(Some(blob)) = self.repo.find_blob_by_id(image.blob_id).await else {
                continue;
            };
            // 张数在软删时没减过，这里也不加（count_album = false）
            self.repo
                .restore_image_reference(&image, blob.size, false)
                .await?;
            if let Some(owner) = owner.as_ref() {
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
            restored += 1;
        }

        self.audit_detail(
            crate::audit::actions::RECYCLE_RESTORE,
            actor_id,
            format!("album:{}", album.name),
            serde_json::json!({ "restored_images": restored, "album_id": album_id }),
            None,
        )
        .await;
        Ok(restored)
    }

    /// 彻底删除回收站里的相册：相册行与其中的图片一起清掉，物理文件交给 GC。
    pub async fn purge_album(&self, viewer: &Viewer, album_id: i64) -> Result<i64> {
        let album = self.require_owned_album_any(viewer, album_id).await?;
        if album.deleted_at.is_none() {
            return Err(AppError::bad_request("相册还在使用中，先把它移到回收站"));
        }
        let actor_id = viewer.id();

        // 先清照片：镜像链接拆掉、墓碑行删掉，物理文件由 GC 收
        let ids = self.repo.deleted_image_ids_in_album(album_id, 20_000).await?;
        let count = ids.len() as i64;
        let owner = self.repo.find_user_by_id(album.owner_id).await?;
        for id in &ids {
            if let (Ok(Some(owner)), Ok(Some(image))) = (
                self.repo.find_user_by_id(album.owner_id).await,
                self.repo.find_image(*id).await,
            ) {
                if let Ok(Some(blob)) = self.repo.find_blob_by_id(image.blob_id).await {
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
            if let Ok(Some(targets)) = self.repo.purge_image_row(*id).await {
                self.reclaim_purged(vec![targets]).await;
            }
        }
        if let Some(owner) = owner.as_ref() {
            self.prune_view_dir(
                pixora_storage::names::LOCAL,
                None,
                &owner.uid,
                &album.slug,
            )
            .await;
        }

        self.repo.delete_album(album_id).await?;
        self.kick_gc();
        self.audit_detail(
            crate::audit::actions::RECYCLE_PURGE,
            actor_id,
            format!("album:{}", album.name),
            serde_json::json!({ "purged_images": count, "album_id": album_id }),
            None,
        )
        .await;
        Ok(count)
    }

    /// 回收站里的相册列表（含各自带回来多少张图）。
    pub async fn deleted_albums(
        &self,
        viewer: &Viewer,
        target_uid: Option<&str>,
    ) -> Result<Vec<AlbumPayload>> {
        // 用户视图下看的是目标用户的回收站：albums_owner 负责 uid 校验与审计
        let owner = self.albums_owner(viewer, target_uid).await?;
        let albums = self.repo.list_deleted_albums(owner.id).await?;
        let mut out = Vec::with_capacity(albums.len());
        for album in albums {
            let (count, bytes) = self
                .repo
                .album_binned_totals(album.id)
                .await
                .unwrap_or((0, 0));
            out.push(self.album_payload(&album, count, bytes).await);
        }
        Ok(out)
    }

    /// 带回收站一起找相册（恢复、彻底删除要看见它），并校验归属。
    async fn require_owned_album_any(&self, viewer: &Viewer, album_id: i64) -> Result<Album> {
        let actor = viewer.user().ok_or_else(|| AppError::Unauthorized)?;
        let album = self
            .repo
            .find_album_any(album_id)
            .await?
            .ok_or_else(|| AppError::not_found("相册"))?;
        self.ensure_can_touch(actor.id, actor.is_admin(), album.owner_id, "相册")?;
        Ok(album)
    }

    // ------------------------------------------------------------

    async fn albums_owner(&self, viewer: &Viewer, uid: Option<&str>) -> Result<User> {
        match uid {
            Some(uid) if viewer.is_admin() => {
                let owner = self
                    .repo
                    .find_user_by_uid(uid)
                    .await?
                    .ok_or_else(|| AppError::not_found("用户"))?;
                if viewer.id() != Some(owner.id) {
                    if let Some(actor) = viewer.user() {
                        self.audit_cross_user(actor.id, uid, "albums", None).await;
                    }
                }
                Ok(owner)
            }
            Some(uid) if viewer.user().map(|user| user.uid.as_str()) == Some(uid) => self
                .repo
                .find_user_by_uid(uid)
                .await?
                .ok_or_else(|| AppError::not_found("用户")),
            Some(_) => Err(AppError::forbidden("只能查看自己的相册")),
            None => viewer
                .user()
                .cloned()
                .ok_or_else(|| AppError::Unauthorized),
        }
    }

    async fn require_owned_album(&self, viewer: &Viewer, album_id: i64) -> Result<Album> {
        let actor = viewer.user().ok_or_else(|| AppError::Unauthorized)?;
        let album = self
            .repo
            .find_album_by_id(album_id)
            .await?
            .ok_or_else(|| AppError::not_found("相册"))?;
        self.ensure_can_touch(actor.id, actor.is_admin(), album.owner_id, "相册")?;
        Ok(album)
    }

    async fn album_payload(&self, album: &Album, image_count: i64, bytes: i64) -> AlbumPayload {
        let cover_url = match album.cover_blob_id {
            Some(blob_id) if self.repo.owner_has_blob(album.owner_id, blob_id).await.unwrap_or(false) => {
                match self.repo.find_blob_by_id(blob_id).await {
                    Ok(Some(blob)) => Some(self.direct_url(&blob.sha256, &blob.ext, true).await),
                    _ => None,
                }
            }
            _ => None,
        };
        AlbumPayload {
            id: album.id,
            name: album.name.clone(),
            slug: album.slug.clone(),
            description: album.description.clone(),
            visibility: album.visibility.clone(),
            image_count,
            bytes,
            cover_url,
            sort_order: album.sort_order,
            updated_at: album.updated_at.to_rfc3339(),
        }
    }

    fn album_totals(
        &self,
        aggregates: &std::collections::HashMap<Option<i64>, (i64, i64)>,
    ) -> (i64, i64) {
        let mut count = 0;
        let mut bytes = 0;
        for (files, size) in aggregates.values() {
            count += files;
            bytes += size;
        }
        (count, bytes)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizing_protects_filesystem() {
        assert_eq!(sanitize_slug("旅行"), "旅行");
        assert_eq!(sanitize_slug("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_slug("工作 2024"), "工作_2024");
    }

    #[test]
    fn viewer_guard_rejects_cross_user_for_normal_account() {
        let user = User {
            id: 1,
            uid: "1001".into(),
            email: "a@b.com".into(),
            email_verified: true,
            password_hash: String::new(),
            role: "user".into(),
            status: "active".into(),
            status_reason: None,
            status_changed_at: None,
            status_changed_by: None,
            totp_secret: None,
            totp_enabled: false,
            quota_bytes: 1,
            used_bytes: 0,
            login_fail_count: 0,
            locked_until: None,
            auto_unsuspend_at: None,
            must_change_password: false,
            avatar: None,
            created_at: pixora_repo::model::now(),
            last_login_at: None,
            last_login_ip: None,
        };
        let viewer = Viewer::User(user);
        assert_eq!(viewer.target_owner_id(None).unwrap().as_deref(), Some("1001"));
        assert!(viewer.target_owner_id(Some("2044")).is_err());
    }
}
