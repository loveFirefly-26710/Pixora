//! 图片仓储：逻辑引用、列表与广场查询、批量操作、引用计数的增删。
//!
//! 引用计数有三处必须同时变化，缺一处就会出现「用量对不上」或「物理文件删不掉」：
//!   - `images` 行（逻辑引用本身）
//!   - `blobs.ref_count`（原图 + 缩略图两份实体）
//!   - `users.used_bytes` 与 `albums.image_count`
//!
//! 因此新增与删除都写成带事务的方法，事务体就在方法内部。

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::error::{RepoError, Result};
use crate::model::*;
use crate::repo::tags::{ensure_tag_on, normalize_tag_list};
use crate::repo::{like_pattern, Page, PageRequest, Repo};

/// 新建图片引用的入参。blob 相关字段指向已经登记好的实体。
#[derive(Debug, Clone)]
pub struct NewImage {
    pub owner_id: i64,
    pub album_id: Option<i64>,
    pub blob_id: i64,
    pub thumb_blob_id: Option<i64>,
    pub original_name: String,
    pub title: Option<String>,
    pub visibility: String,
}

/// 删除一张引用所需的全部信息。由服务层从已加载的图片与实体上填好。
#[derive(Debug, Clone)]
pub struct ImageDeletion {
    pub image_id: i64,
    pub owner_id: i64,
    pub album_id: Option<i64>,
    pub blob_id: i64,
    pub thumb_blob_id: Option<i64>,
    /// 原图字节数，用于回退用户用量
    pub size: i64,
}

/// 图片列表的排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageSort {
    #[default]
    Newest,
    Oldest,
    Largest,
    NameAsc,
    /// 按浏览量，广场的「最热」
    Hottest,
}

impl ImageSort {
    pub fn parse(input: &str) -> Self {
        match input.trim().to_ascii_lowercase().as_str() {
            "old" | "oldest" => Self::Oldest,
            "size" | "largest" => Self::Largest,
            "name" | "name_asc" => Self::NameAsc,
            "hot" | "hottest" => Self::Hottest,
            _ => Self::Newest,
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            Self::Newest => "i.upload_time DESC, i.id DESC",
            Self::Oldest => "i.upload_time ASC, i.id ASC",
            Self::Largest => "b.size DESC, i.id DESC",
            Self::NameAsc => "COALESCE(i.title, i.original_name) ASC, i.id ASC",
            Self::Hottest => "i.views DESC, i.upload_time DESC, i.id DESC",
        }
    }
}

/// 列表与广场共用的过滤条件。
#[derive(Debug, Clone, Default)]
pub struct ImageFilter {
    pub owner_id: Option<i64>,
    pub album_id: Option<i64>,
    /// 只要公开图片（广场、未登录浏览）
    pub public_only: bool,
    /// 标题或标签包含该关键词
    pub query: Option<String>,
    /// 精确命中某个标签
    pub tag: Option<String>,
    /// 只要指定的一批 id（批量操作前回读用）
    pub ids: Option<Vec<i64>>,
    /// 包含已软删的图片（回收站/审计才需要）
    pub include_deleted: bool,
    pub sort: ImageSort,
}

impl ImageFilter {
    pub fn owner(owner_id: i64) -> Self {
        Self {
            owner_id: Some(owner_id),
            ..Default::default()
        }
    }

    pub fn public_plaza() -> Self {
        Self {
            public_only: true,
            ..Default::default()
        }
    }
}

/// 图片的修改补丁。`None` 表示不动，`Some(None)` 表示清空。
#[derive(Debug, Clone, Default)]
pub struct ImagePatch {
    pub title: Option<Option<String>>,
    pub visibility: Option<String>,
    pub album_id: Option<Option<i64>>,
    /// 给了就整体替换标签集合
    pub tags: Option<Vec<String>>,
}

/// 相册维度的聚合，给 `GET /api/v1/fs/tree` 用。
#[derive(Debug, Clone, Default)]
pub struct AlbumAggregate {
    pub album_id: Option<i64>,
    pub files: i64,
    pub bytes: i64,
}

/// 广场的筛选参数。
#[derive(Debug, Clone, Default)]
pub struct PlazaFilter {
    pub query: Option<String>,
    pub tag: Option<String>,
    pub sort: ImageSort,
}

impl Repo {
    // ---------- 读取 ----------

    pub async fn find_image(&self, id: i64) -> Result<Option<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE id = ?",
            Image::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Image::from_row).transpose()
    }

    /// 单张图的完整视图（含物理属性、相册名与标签）。
    pub async fn find_image_view(&self, id: i64) -> Result<Option<ImageView>> {
        let row = self
            .db
            .fetch_optional(
                &format!("{IMAGE_VIEW_SQL} WHERE i.id = ?"),
                Args::new().int(id),
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut view = ImageView::from_row(&row)?;
        let mut tags = self.tags_for_images(&[view.id]).await?;
        view.tags = tags.remove(&view.id).unwrap_or_default();
        Ok(Some(view))
    }

    /// 按 id 批量回读视图，批量操作前用它拿到归属与大小。
    pub async fn list_image_views(&self, ids: &[i64]) -> Result<Vec<ImageView>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("{IMAGE_VIEW_SQL} WHERE i.id IN ({placeholders})");
        let mut args = Args::new();
        for id in ids {
            args = args.int(*id);
        }
        let rows = self.db.fetch_all(&sql, args).await?;
        let mut views: Vec<ImageView> = rows.iter().map(ImageView::from_row).collect::<Result<_>>()?;
        let id_list: Vec<i64> = views.iter().map(|view| view.id).collect();
        let mut tag_map = self.tags_for_images(&id_list).await?;
        for view in &mut views {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(views)
    }

    /// 图片列表（相册页、搜索页都走这里）。
    pub async fn list_images(
        &self,
        filter: &ImageFilter,
        page: PageRequest,
    ) -> Result<Page<ImageView>> {
        let (where_sql, args) = self.image_where(filter);
        let total = self
            .db
            .count(
                &format!(
                    "SELECT COUNT(*) AS total FROM images i JOIN blobs b ON b.id = i.blob_id{where_sql}"
                ),
                args.clone(),
            )
            .await?;

        let sql = format!(
            "{IMAGE_VIEW_SQL}{where_sql} ORDER BY {} LIMIT ? OFFSET ?",
            filter.sort.order_by()
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(page.per_page).int(page.offset()))
            .await?;
        let mut views: Vec<ImageView> = rows.iter().map(ImageView::from_row).collect::<Result<_>>()?;
        let ids: Vec<i64> = views.iter().map(|view| view.id).collect();
        let mut tag_map = self.tags_for_images(&ids).await?;
        for view in &mut views {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(Page::new(views, total, page))
    }

    /// 广场：只捞公开图片。
    pub async fn list_plaza(
        &self,
        filter: &PlazaFilter,
        page: PageRequest,
    ) -> Result<Page<ImageView>> {
        self.list_images(
            &ImageFilter {
                public_only: true,
                query: filter.query.clone(),
                tag: filter.tag.clone(),
                sort: filter.sort,
                ..Default::default()
            },
            page,
        )
        .await
    }

    pub async fn count_images(&self, filter: &ImageFilter) -> Result<i64> {
        let (where_sql, args) = self.image_where(filter);
        self.db
            .count(
                &format!(
                    "SELECT COUNT(*) AS total FROM images i JOIN blobs b ON b.id = i.blob_id{where_sql}"
                ),
                args,
            )
            .await
    }

    /// 最近上传，统计面板用。
    pub async fn recent_images(&self, limit: i64) -> Result<Vec<ImageView>> {
        let sql = format!(
            "{IMAGE_VIEW_SQL} WHERE i.deleted_at IS NULL ORDER BY i.upload_time DESC, i.id DESC LIMIT ?"
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(limit.clamp(1, 100)))
            .await?;
        let mut views: Vec<ImageView> = rows.iter().map(ImageView::from_row).collect::<Result<_>>()?;
        let ids: Vec<i64> = views.iter().map(|view| view.id).collect();
        let mut tag_map = self.tags_for_images(&ids).await?;
        for view in &mut views {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(views)
    }

    /// 相册维度的文件数与字节数，`fs/tree` 与容量展示都要。
    pub async fn album_aggregates(&self, owner_id: i64) -> Result<Vec<AlbumAggregate>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT i.album_id AS album_id, COUNT(*) AS files, COALESCE(SUM(b.size), 0) AS bytes \
                   FROM images i JOIN blobs b ON b.id = i.blob_id \
                  WHERE i.owner_id = ? AND i.deleted_at IS NULL \
                  GROUP BY i.album_id",
                Args::new().int(owner_id),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(AlbumAggregate {
                    album_id: get_opt_i64(row, "album_id")?,
                    files: get_i64(row, "files")?,
                    bytes: get_i64(row, "bytes")?,
                })
            })
            .collect()
    }

    // ---------- 写入 ----------

    /// 建立图片引用：一步完成「插行 + 加引用计数 + 记用量 + 相册计数 + 写标签」。
    ///
    /// 去重命中时同一条 blob 会被多个用户引用，`used_bytes` 各自累加，
    /// 物理空间只占一份——这是设计稿里「删一份不误伤另一份」的实现基础。
    pub async fn create_image_reference(
        &self,
        new: &NewImage,
        blob_size: i64,
        tags: &[String],
    ) -> Result<Image> {
        let mut tx = self.db.pool().begin().await?;
        let now = now_ts();

        let insert = self.db.sql_v(
            "INSERT INTO images (owner_id, album_id, blob_id, thumb_blob_id, original_name, title, \
                                visibility, views, upload_time, deleted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL) RETURNING id",
            "INSERT INTO images (owner_id, album_id, blob_id, thumb_blob_id, original_name, title, \
                                visibility, views, upload_time, deleted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL)",
            "INSERT INTO images (owner_id, album_id, blob_id, thumb_blob_id, original_name, title, \
                                visibility, views, upload_time, deleted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL) RETURNING id",
        );
        let image_id = if self.db.dialect() == crate::db::Dialect::MySql {
            sqlx::query(&insert)
                .bind(new.owner_id)
                .bind(new.album_id)
                .bind(new.blob_id)
                .bind(new.thumb_blob_id)
                .bind(&new.original_name)
                .bind(new.title.clone())
                .bind(&new.visibility)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            let row = sqlx::query(&self.db.sql("SELECT LAST_INSERT_ID() AS id"))
                .fetch_one(&mut *tx)
                .await?;
            get_i64(&row, "id")?
        } else {
            let row = sqlx::query(&insert)
                .bind(new.owner_id)
                .bind(new.album_id)
                .bind(new.blob_id)
                .bind(new.thumb_blob_id)
                .bind(&new.original_name)
                .bind(new.title.clone())
                .bind(&new.visibility)
                .bind(&now)
                .fetch_one(&mut *tx)
                .await?;
            get_i64(&row, "id")?
        };

        sqlx::query(&self.db.sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"))
            .bind(new.blob_id)
            .execute(&mut *tx)
            .await?;
        if let Some(thumb_id) = new.thumb_blob_id {
            sqlx::query(&self.db.sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"))
                .bind(thumb_id)
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query(&self.db.sql("UPDATE users SET used_bytes = used_bytes + ? WHERE id = ?"))
            .bind(blob_size)
            .bind(new.owner_id)
            .execute(&mut *tx)
            .await?;

        if let Some(album_id) = new.album_id {
            sqlx::query(
                &self
                    .db
                    .sql("UPDATE albums SET image_count = image_count + 1, updated_at = ? WHERE id = ?"),
            )
            .bind(&now)
            .bind(album_id)
            .execute(&mut *tx)
            .await?;
        }

        for name in normalize_tag_list(tags) {
            let tag_id = ensure_tag_on(&self.db, &mut tx, new.owner_id, &name).await?;
            let link = self.db.sql_v(
                "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
                "INSERT IGNORE INTO image_tags (image_id, tag_id) VALUES (?, ?)",
                "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
            );
            sqlx::query(&link)
                .bind(image_id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        self.find_image(image_id)
            .await?
            .ok_or_else(|| RepoError::NotFound(format!("新建图片 {image_id}")))
    }

    /// 软删除一张引用，并按引用计数回收空间记账。
    pub async fn delete_image_reference(&self, deletion: &ImageDeletion, at: DateTime<Utc>) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        let stamp = ts(at);

        sqlx::query(
            &self
                .db
                .sql("UPDATE images SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL"),
        )
        .bind(&stamp)
        .bind(deletion.image_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            &self
                .db
                .sql("UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?"),
        )
        .bind(deletion.blob_id)
        .execute(&mut *tx)
        .await?;
        if let Some(thumb_id) = deletion.thumb_blob_id {
            sqlx::query(
                &self
                    .db
                    .sql("UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?"),
            )
            .bind(thumb_id)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            &self
                .db
                .sql("UPDATE users SET used_bytes = MAX(used_bytes - ?, 0) WHERE id = ?"),
        )
        .bind(deletion.size)
        .bind(deletion.owner_id)
        .execute(&mut *tx)
        .await?;

        if let Some(album_id) = deletion.album_id {
            sqlx::query(
                &self
                    .db
                    .sql("UPDATE albums SET image_count = MAX(image_count - 1, 0), updated_at = ? WHERE id = ?"),
            )
            .bind(&stamp)
            .bind(album_id)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(&self.db.sql("DELETE FROM image_tags WHERE image_id = ?"))
            .bind(deletion.image_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// 批量软删。返回实际删除的张数。
    ///
    /// 逐张在同一个事务里删：跨表计数必须成对变化，一次性 `UPDATE ... WHERE id IN (...)`
    /// 做不到「按每张图的大小回退用量」。
    pub async fn delete_images(&self, deletions: &[ImageDeletion], at: DateTime<Utc>) -> Result<u64> {
        let mut removed = 0u64;
        for deletion in deletions {
            self.delete_image_reference(deletion, at).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// 批量移动到相册（传 `None` 即移出相册）。
    pub async fn move_images(
        &self,
        ids: &[i64],
        owner_id: i64,
        album_id: Option<i64>,
    ) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let now = now_ts();

        // 先回读旧归属，才能把两个相册的计数都改对
        let old_rows = self
            .db
            .fetch_all(
                &format!("SELECT id, album_id FROM images WHERE owner_id = ? AND id IN ({placeholders})"),
                {
                    let mut args = Args::new().int(owner_id);
                    for id in ids {
                        args = args.int(*id);
                    }
                    args
                },
            )
            .await?;

        let mut tx = self.db.pool().begin().await?;
        let mut moved = 0u64;
        for row in &old_rows {
            let image_id = get_i64(row, "id")?;
            let old_album = get_opt_i64(row, "album_id")?;
            if old_album == album_id {
                continue;
            }
            if let Some(old) = old_album {
                sqlx::query(
                    &self.db.sql(
                        "UPDATE albums SET image_count = MAX(image_count - 1, 0), updated_at = ? WHERE id = ?",
                    ),
                )
                .bind(&now)
                .bind(old)
                .execute(&mut *tx)
                .await?;
            }
            if let Some(new_album) = album_id {
                sqlx::query(
                    &self.db.sql(
                        "UPDATE albums SET image_count = image_count + 1, updated_at = ? WHERE id = ?",
                    ),
                )
                .bind(&now)
                .bind(new_album)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(&self.db.sql("UPDATE images SET album_id = ? WHERE id = ?"))
                .bind(album_id)
                .bind(image_id)
                .execute(&mut *tx)
                .await?;
            moved += 1;
        }
        tx.commit().await?;
        Ok(moved)
    }

    /// 批量改标签、标题、可见性。返回改动的张数。
    pub async fn update_images(
        &self,
        ids: &[i64],
        owner_id: i64,
        patch: &ImagePatch,
    ) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db.pool().begin().await?;
        let mut affected = 0u64;

        let mut sets: Vec<&str> = Vec::new();
        let mut args = Args::new();
        if let Some(title) = &patch.title {
            sets.push("title = ?");
            args = args.opt_text(title.clone());
        }
        if let Some(visibility) = &patch.visibility {
            sets.push("visibility = ?");
            args = args.text(visibility);
        }
        if let Some(album_id) = &patch.album_id {
            sets.push("album_id = ?");
            args = args.opt_int(*album_id);
        }

        if !sets.is_empty() {
            let placeholders = vec!["?"; ids.len()].join(", ");
            args = args.int(owner_id);
            for id in ids {
                args = args.int(*id);
            }
            let sql = self.db.sql(&format!(
                "UPDATE images SET {} WHERE owner_id = ? AND id IN ({placeholders}) AND deleted_at IS NULL",
                sets.join(", ")
            ));
            let result = sqlx::query_with(&sql, args.take()).execute(&mut *tx).await?;
            affected = result.rows_affected();
        }

        if let Some(tags) = &patch.tags {
            // 只改标签时 `sets` 是空的，标量那条 UPDATE 不会执行，
            // `affected` 会停在 0——但标签确实写进去了，返回 0 会让「批量打标签」
            // 看起来像什么都没做。这里按实际处理的张数补齐。
            let mut touched = 0u64;
            for id in ids {
                sqlx::query(&self.db.sql("DELETE FROM image_tags WHERE image_id = ?"))
                    .bind(*id)
                    .execute(&mut *tx)
                    .await?;
                for name in normalize_tag_list(tags) {
                    let tag_id = ensure_tag_on(&self.db, &mut tx, owner_id, &name).await?;
                    let link = self.db.sql_v(
                        "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
                        "INSERT IGNORE INTO image_tags (image_id, tag_id) VALUES (?, ?)",
                        "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
                    );
                    sqlx::query(&link)
                        .bind(*id)
                        .bind(tag_id)
                        .execute(&mut *tx)
                        .await?;
                }
                touched += 1;
            }
            affected = affected.max(touched);
        }

        tx.commit().await?;
        Ok(affected)
    }

    /// 单张图的补丁修改。
    pub async fn update_image(&self, id: i64, patch: &ImagePatch) -> Result<()> {
        let mut sets: Vec<String> = Vec::new();
        let mut args = Args::new();
        if let Some(title) = &patch.title {
            sets.push("title = ?".into());
            args = args.opt_text(title.clone());
        }
        if let Some(visibility) = &patch.visibility {
            sets.push("visibility = ?".into());
            args = args.text(visibility);
        }
        if let Some(album_id) = &patch.album_id {
            sets.push("album_id = ?".into());
            args = args.opt_int(*album_id);
        }
        if !sets.is_empty() {
            self.db
                .execute(
                    &format!("UPDATE images SET {} WHERE id = ?", sets.join(", ")),
                    args.int(id),
                )
                .await?;
        }

        if let Some(tags) = &patch.tags {
            let image = self
                .find_image(id)
                .await?
                .ok_or_else(|| RepoError::NotFound(format!("图片 {id}")))?;
            self.set_image_tags(id, image.owner_id, tags).await?;
        }
        Ok(())
    }

    /// 浏览一次。广场的「最热」用得上。
    pub async fn increment_views(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE images SET views = views + 1 WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 图片总数（公开面）。
    pub async fn count_all_images(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM images WHERE deleted_at IS NULL",
                Args::new(),
            )
            .await
    }

    /// 拼接图片列表的 WHERE 子句。拼进去的都是写死的片段，值全部走绑定。
    fn image_where(&self, filter: &ImageFilter) -> (String, Args) {
        let mut builder = WhereBuilder::new();

        if !filter.include_deleted {
            builder = builder.and("i.deleted_at IS NULL", |args| args);
        }
        if let Some(owner_id) = filter.owner_id {
            builder = builder.and("i.owner_id = ?", move |args| args.int(owner_id));
        }
        if let Some(album_id) = filter.album_id {
            builder = builder.and("i.album_id = ?", move |args| args.int(album_id));
        }
        if filter.public_only {
            builder = builder.and("i.visibility = 'public'", |args| args);
        }
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = like_pattern(query);
            builder = builder.and(
                "(i.title LIKE ? ESCAPE '\\' OR i.original_name LIKE ? ESCAPE '\\' \
                  OR EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                              WHERE it.image_id = i.id AND t.name LIKE ? ESCAPE '\\'))",
                move |args| {
                    args.text(pattern.clone())
                        .text(pattern.clone())
                        .text(pattern)
                },
            );
        }
        if let Some(tag) = filter
            .tag
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder = builder.and(
                "EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                          WHERE it.image_id = i.id AND t.name = ?)",
                move |args| args.text(tag),
            );
        }
        if let Some(ids) = &filter.ids {
            if ids.is_empty() {
                builder = builder.and("1 = 0", |args| args);
            } else {
                let placeholders = vec!["?"; ids.len()].join(", ");
                let owned: Vec<i64> = ids.clone();
                builder = builder.and(&format!("i.id IN ({placeholders})"), move |mut args| {
                    for id in &owned {
                        args = args.int(*id);
                    }
                    args
                });
            }
        }

        (builder.sql_string(), builder.args())
    }

    /// 一批图片的标签映射（列表页避免 N+1）。
    pub async fn tag_map_for(&self, image_ids: &[i64]) -> Result<HashMap<i64, Vec<String>>> {
        self.tags_for_images(image_ids).await
    }
}

/// 图片视图的公共 SELECT 前缀。详情、列表、广场、统计面板都复用它。
const IMAGE_VIEW_SQL: &str = "SELECT i.id, i.owner_id, u.uid AS owner_uid, i.album_id, \
        a.name AS album_name, i.title, i.original_name, i.visibility, i.views, i.upload_time, \
        b.sha256, b.ext, b.mime_type, b.size, b.width, b.height, i.thumb_blob_id, \
        b.storage_key, b.storage_driver \
   FROM images i \
   JOIN blobs b ON b.id = i.blob_id \
   JOIN users u ON u.id = i.owner_id \
   LEFT JOIN albums a ON a.id = i.album_id";

/// 缩略图兜底与对账要用到的补充查询。
///
/// 放在文件末尾而不是新开模块：它们和图片列表是同一批表、同一批列，
/// 拆出去反而让人来回翻。
impl Repo {
    /// 找出还没有缩略图的图片，供服务端兜底生成用。
    ///
    /// 远端后端要先把原图下载回来，对低优先级的兜底任务不划算，
    /// 所以传 `only_local` 时只返回存在本地驱动上的图片。
    pub async fn list_images_missing_thumb(
        &self,
        limit: i64,
        only_local: bool,
    ) -> Result<Vec<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE deleted_at IS NULL AND thumb_blob_id IS NULL ORDER BY id ASC LIMIT ?",
            Image::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(limit.clamp(1, 500)))
            .await?;
        let images: Vec<Image> = rows.iter().map(Image::from_row).collect::<Result<_>>()?;
        if !only_local {
            return Ok(images);
        }

        let mut local = Vec::with_capacity(images.len());
        for image in images {
            if let Some(blob) = self.find_blob_by_id(image.blob_id).await? {
                if blob.storage_driver == "local" {
                    local.push(image);
                }
            }
        }
        Ok(local)
    }

    /// 给图片补上缩略图引用，并把缩略图实体的引用计数加一。
    pub async fn attach_image_thumb(&self, image_id: i64, thumb_blob_id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE images SET thumb_blob_id = ? WHERE id = ? AND thumb_blob_id IS NULL",
                Args::new().int(thumb_blob_id).int(image_id),
            )
            .await?;
        self.db
            .execute(
                "UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?",
                Args::new().int(thumb_blob_id),
            )
            .await?;
        Ok(())
    }

    /// 引用计数为 0 且登记时间早于宽限期的实体，GC 的输入。
    pub async fn count_gc_candidates(&self, older_than: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM blobs WHERE ref_count = 0 AND created_at < ?",
                Args::new().text(ts(older_than)),
            )
            .await
    }
}
