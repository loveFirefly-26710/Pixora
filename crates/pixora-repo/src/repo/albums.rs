//! 相册仓储。
//!
//! 相册是「用户 → 相册 → 图片」这条链的中间层，slug 决定物理目录名，
//! 因此同一用户内必须唯一（数据库上有唯一约束，这里再多做一次存在性检查，
//! 好给出可读提示而不是抛约束冲突）。

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::error::{RepoError, Result};
use crate::model::*;
use crate::repo::{like_pattern, Page, PageRequest, Repo};

/// 注册时自动建的相册，slug 固定，同一用户内至多一个。
/// 默认相册的 slug。它是「上传不指定相册」的落脚点，别处也要按它判断。
pub const DEFAULT_ALBUM_SLUG: &str = "default";

/// 新建相册的入参。
#[derive(Debug, Clone)]
pub struct NewAlbum {
    pub owner_id: i64,
    pub name: String,
    /// 物理目录名，同一用户内唯一
    pub slug: String,
    pub description: Option<String>,
    pub visibility: String,
    pub sort_order: i64,
}

/// 相册改名 / 改可见性 / 改排序的补丁。
///
/// `None` 表示不动这一项；`Some(None)` 表示清空（只对可空字段有意义）。
#[derive(Debug, Clone, Default)]
pub struct AlbumPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub visibility: Option<String>,
    pub sort_order: Option<i64>,
    pub cover_blob_id: Option<Option<i64>>,
}

/// 相册列表的过滤条件。
#[derive(Debug, Clone, Default)]
pub struct AlbumFilter {
    pub owner_id: Option<i64>,
    /// 只返回公开相册
    pub public_only: bool,
    pub query: Option<String>,
}

impl Repo {
    // ---------- 读取 ----------

    pub async fn find_album_by_id(&self, id: i64) -> Result<Option<Album>> {
        // 只认活着的相册：回收站里的相册不该还能被上传、批量操作选中
        let sql = format!(
            "SELECT {} FROM albums WHERE id = ? AND deleted_at IS NULL",
            Album::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Album::from_row).transpose()
    }

    /// 同一个人名下 slug 唯一，所以这一对键就能定位一行。
    pub async fn find_album_by_slug(&self, owner_id: i64, slug: &str) -> Result<Option<Album>> {
        let sql = format!(
            "SELECT {} FROM albums WHERE owner_id = ? AND slug = ? AND deleted_at IS NULL",
            Album::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(owner_id).text(slug))
            .await?;
        row.as_ref().map(Album::from_row).transpose()
    }

    /// 相册列表。过滤条件按需拼，总数查询与列表查询共用同一组绑定。
    pub async fn list_albums(
        &self,
        filter: &AlbumFilter,
        page: PageRequest,
    ) -> Result<Page<Album>> {
        let mut filters = WhereBuilder::new();
        // 回收站里的相册不进任何列表
        filters = filters.and("deleted_at IS NULL", |args| args);
        if let Some(owner_id) = filter.owner_id {
            filters = filters.and("owner_id = ?", move |args| args.int(owner_id));
        }
        if filter.public_only {
            filters = filters.and("visibility = 'public'", |args| args);
        }
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = like_pattern(query);
            filters = filters.and(
                "(name LIKE ? ESCAPE '\\' OR COALESCE(description, '') LIKE ? ESCAPE '\\')",
                move |args| args.text(pattern.clone()).text(pattern),
            );
        }

        let where_sql = filters.sql_string();
        let args = filters.args();

        let total = self
            .db
            .count(
                &format!("SELECT COUNT(*) AS total FROM albums{where_sql}"),
                args.clone(),
            )
            .await?;

        // 排列顺序：sort_order ASC, id ASC —— 手工排过的相册在前，其余按建立顺序。
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT {} FROM albums{where_sql} ORDER BY sort_order ASC, id ASC LIMIT ? OFFSET ?",
                    Album::COLUMNS
                ),
                args.int(page.per_page).int(page.offset()),
            )
            .await?;

        let items = rows.iter().map(Album::from_row).collect::<Result<Vec<_>>>()?;
        Ok(Page::new(items, total, page))
    }

    /// 侧边栏要全量、不分页，单独给一条查询省掉一次总数统计。
    pub async fn list_albums_all(&self, owner_id: i64) -> Result<Vec<Album>> {
        let sql = format!(
            "SELECT {} FROM albums WHERE owner_id = ? AND deleted_at IS NULL \
             ORDER BY sort_order ASC, id ASC",
            Album::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(owner_id))
            .await?;
        rows.iter().map(Album::from_row).collect()
    }

    // ---------- 写入 ----------

    /// 建相册。`created_at` 与 `updated_at` 同值起步。
    ///
    /// slug 冲突不在这里吸收：让数据库的唯一约束原样抛出，调用方用
    /// [`RepoError::is_unique_violation`] 就能识别并重试；想给用户可读提示的话，
    /// 先调 [`Repo::slug_exists`] 预检。两条路径分工，不重复判断。
    pub async fn create_album(&self, new: &NewAlbum) -> Result<Album> {
        let now = now_ts();
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO albums (owner_id, name, slug, description, visibility, cover_blob_id, \
                                     image_count, sort_order, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, NULL, 0, ?, ?, ?)",
                Args::new()
                    .int(new.owner_id)
                    .text(&new.name)
                    .text(&new.slug)
                    .opt_text(new.description.clone())
                    .text(&new.visibility)
                    .int(new.sort_order)
                    .text(&now)
                    .text(&now),
            )
            .await?;

        self.find_album_by_id(id)
            .await?
            .ok_or_else(|| RepoError::NotFound(format!("新建相册 {id}")))
    }

    /// 改名 / 改可见性 / 改排序 / 换封面。没给字段就原样保留。
    pub async fn update_album(&self, id: i64, patch: &AlbumPatch) -> Result<()> {
        // updated_at 总是跟着走，所以补丁全为空时也还是一次合法的改动（刷新时间戳）
        let mut sets: Vec<&str> = vec!["updated_at = ?"];
        let mut args = Args::new().text(now_ts());
        if let Some(name) = &patch.name {
            sets.push("name = ?");
            args = args.text(name);
        }
        if let Some(description) = &patch.description {
            sets.push("description = ?");
            args = args.opt_text(description.clone());
        }
        if let Some(visibility) = &patch.visibility {
            sets.push("visibility = ?");
            args = args.text(visibility);
        }
        if let Some(sort_order) = patch.sort_order {
            sets.push("sort_order = ?");
            args = args.int(sort_order);
        }
        if let Some(cover_blob_id) = &patch.cover_blob_id {
            sets.push("cover_blob_id = ?");
            args = args.opt_int(*cover_blob_id);
        }

        self.db
            .execute(
                &format!("UPDATE albums SET {} WHERE id = ?", sets.join(", ")),
                args.int(id),
            )
            .await?;
        Ok(())
    }

    /// 彻底删除相册行。回收站的「彻底删除」与管理员清理走这里。
    ///
    /// 里面的图片先改挂到主人的默认相册：图片永远该有个归属，「未分类」这个
    /// 状态已经不对外了（见 `Repo::move_images`）。找不到默认相册就保持原样——
    /// 外键会把归属置空，启动时的 `attach_orphan_images` 再把它收进去。
    pub async fn delete_album(&self, id: i64) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(&self.db.sql(
            "UPDATE images SET album_id = ( \
                 SELECT a.id FROM albums a \
                  WHERE a.owner_id = (SELECT owner_id FROM albums WHERE id = ?) \
                    AND a.slug = ? AND a.deleted_at IS NULL LIMIT 1 \
             ) WHERE album_id = ?",
        ))
        .bind(id)
        .bind(DEFAULT_ALBUM_SLUG)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        // 归属变了，默认相册的计数得跟着涨：这一步外键不会替我们做
        sqlx::query(&self.db.sql(
            "UPDATE albums SET image_count = ( \
                 SELECT COUNT(*) FROM images i WHERE i.album_id = albums.id \
             ), updated_at = ? \
             WHERE slug = ? \
               AND owner_id = (SELECT owner_id FROM albums WHERE id = ?)",
        ))
        .bind(now_ts())
        .bind(DEFAULT_ALBUM_SLUG)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&self.db.sql("DELETE FROM albums WHERE id = ?"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 移进回收站。行还在，只是不再出现在任何列表里。
    pub async fn soft_delete_album(&self, id: i64, at: DateTime<Utc>) -> Result<()> {
        self.db
            .execute(
                "UPDATE albums SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
                Args::new().text(ts(at)).text(ts(at)).int(id),
            )
            .await?;
        Ok(())
    }

    /// 从回收站恢复。
    pub async fn restore_album(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE albums SET deleted_at = NULL, updated_at = ? WHERE id = ?",
                Args::new().text(now_ts()).int(id),
            )
            .await?;
        Ok(())
    }

    /// 按 id 连回收站一起找。恢复、彻底删除这些操作需要看见它。
    pub async fn find_album_any(&self, id: i64) -> Result<Option<Album>> {
        let sql = format!("SELECT {} FROM albums WHERE id = ?", Album::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Album::from_row).transpose()
    }

    /// 回收站里的相册列表（按移入时间倒序）。
    pub async fn list_deleted_albums(&self, owner_id: i64) -> Result<Vec<Album>> {
        let sql = format!(
            "SELECT {} FROM albums WHERE owner_id = ? AND deleted_at IS NOT NULL \
             ORDER BY deleted_at DESC",
            Album::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(owner_id)).await?;
        rows.iter().map(Album::from_row).collect()
    }

    /// 数一下回收站里有多少个相册。
    pub async fn count_deleted_albums(&self, owner_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM albums WHERE owner_id = ? AND deleted_at IS NOT NULL",
                Args::new().int(owner_id),
            )
            .await
    }

    /// 设封面。传 `None` 即回到「没有封面」，列表页据此用第一张图兜底。
    pub async fn set_album_cover(&self, id: i64, blob_id: Option<i64>) -> Result<()> {
        self.db
            .execute(
                "UPDATE albums SET cover_blob_id = ?, updated_at = ? WHERE id = ?",
                Args::new().opt_int(blob_id).text(now_ts()).int(id),
            )
            .await?;
        Ok(())
    }

    // ---------- 计数与对账 ----------

    pub async fn count_albums(&self, owner_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM albums WHERE owner_id = ?",
                Args::new().int(owner_id),
            )
            .await
    }

    pub async fn slug_exists(&self, owner_id: i64, slug: &str) -> Result<bool> {
        self.db
            .exists(
                // 不排除回收站：占着 slug 的相册哪怕在回收站里，也得拦住
                // 「新建同名相册」——不然恢复时就撞唯一键了
                "SELECT 1 AS hit FROM albums WHERE owner_id = ? AND slug = ?",
                Args::new().int(owner_id).text(slug),
            )
            .await
    }

    /// 按 `images` 表重算某个相册的 `image_count`，返回重算后的值。
    ///
    /// 计数在收录 / 移出 / 删除图片时增量维护，对账用这个兜底修正漂移。
    pub async fn recount_album_images(&self, album_id: i64) -> Result<i64> {
        self.db
            .execute(
                "UPDATE albums SET image_count = ( \
                     SELECT COUNT(*) FROM images i \
                      WHERE i.album_id = albums.id AND i.deleted_at IS NULL \
                 ) WHERE id = ?",
                Args::new().int(album_id),
            )
            .await?;

        self.db
            .count(
                "SELECT image_count AS total FROM albums WHERE id = ?",
                Args::new().int(album_id),
            )
            .await
    }

    /// 全库对账：只改数值不一致的行，返回被修正的行数。
    pub async fn recount_all_album_images(&self) -> Result<u64> {
        self.db
            .execute(
                "UPDATE albums SET image_count = ( \
                     SELECT COUNT(*) FROM images i \
                      WHERE i.album_id = albums.id AND i.deleted_at IS NULL \
                 ) WHERE image_count <> ( \
                     SELECT COUNT(*) FROM images i \
                      WHERE i.album_id = albums.id AND i.deleted_at IS NULL \
                 )",
                Args::new(),
            )
            .await
    }

    /// 注册时建「默认相册」。幂等：已有就直接返回那一行。
    ///
    /// 并发注册同一账号不会发生，但两处调用（注册流程与补偿任务）可能撞车，
    /// 所以插入失败时按唯一约束再回读一次，调用方拿到的一定是同一行。
    pub async fn insert_default_album(&self, owner_id: i64, name: &str) -> Result<Album> {
        if let Some(existing) = self.find_album_by_slug(owner_id, DEFAULT_ALBUM_SLUG).await? {
            return Ok(existing);
        }

        let new = NewAlbum {
            owner_id,
            name: name.to_string(),
            slug: DEFAULT_ALBUM_SLUG.to_string(),
            description: None,
            visibility: visibility::PRIVATE.to_string(),
            sort_order: 0,
        };
        match self.create_album(&new).await {
            Ok(album) => Ok(album),
            Err(err) if err.is_unique_violation() => self
                .find_album_by_slug(owner_id, DEFAULT_ALBUM_SLUG)
                .await?
                .ok_or_else(|| RepoError::NotFound(format!("默认相册 owner={owner_id}"))),
            Err(err) => Err(err),
        }
    }
}
