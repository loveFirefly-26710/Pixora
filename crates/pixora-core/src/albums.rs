//! 相册服务。
//!
//! 相册是「用户 → 相册 → 图片」中间的那一层，`slug` 决定物理目录名，
//! 所以新建时要净化：目录名要能跨平台安全落盘，也不能带出路径穿越。
//!
//! 删除相册**不删图片**：图片只是解除关联（移入「未分类」）。删相册顺手删图
//! 是很多图床的默认行为，也是最容易造成数据损失的按钮，这里刻意不这么做。

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
        self.require_owned_album(viewer, album_id).await?;

        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(AppError::bad_request("相册名称不能为空"));
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
    pub async fn delete_album(&self, viewer: &Viewer, album_id: i64) -> Result<i64> {
        let album = self.require_owned_album(viewer, album_id).await?;
        let count = album.image_count;
        self.repo.delete_album(album_id).await?;
        Ok(count)
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
            Some(blob_id) => match self.repo.find_blob_by_id(blob_id).await {
                Ok(Some(blob)) => Some(self.direct_url(&blob.sha256, &blob.ext, true).await),
                _ => None,
            },
            None => None,
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
            created_at: pixora_repo::model::now(),
            last_login_at: None,
            last_login_ip: None,
        };
        let viewer = Viewer::User(user);
        assert_eq!(viewer.target_owner_id(None).unwrap().as_deref(), Some("1001"));
        assert!(viewer.target_owner_id(Some("2044")).is_err());
    }
}
