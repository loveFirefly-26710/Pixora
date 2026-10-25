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
    /// 这次上传来自哪个 IP、什么终端。访客上传靠它留痕，登录用户顺手也记
    pub upload_ip: Option<String>,
    pub upload_agent: Option<String>,
}

/// 彻底删除一条墓碑之后，需要调用方回收的实体 id。
///
/// 墓碑一删，引用它的那行就没了；这时实体如果已经没人引用，物理文件也该
/// 当场收掉，而不是等后台扫描——「彻底删除」的意思就是立刻销毁。
#[derive(Debug, Clone)]
pub struct PurgedBlobs {
    pub blob_id: i64,
    pub thumb_blob_id: Option<i64>,
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
    /// 是否「随相册一起」进回收站：恢复相册时只带回这一批，
    /// 不碰用户早先单独删掉的那些
    pub by_album: bool,
}

/// 图片列表的排序方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageSort {
    /// 默认：打乱顺序看。种子由客户端给（见 `ImageFilter::seed`），
    /// 同一个种子翻页不会重复也不会漏——真随机排序做不到这两点
    #[default]
    Shuffle,
    Newest,
    Oldest,
    Largest,
    Smallest,
    NameAsc,
    NameDesc,
    /// 按扩展名（文件类型）分组，同类型内新图在前
    TypeAsc,
    /// 按浏览量，广场的「最热」
    Hottest,
}

impl ImageSort {
    pub fn parse(input: &str) -> Self {
        match input.trim().to_ascii_lowercase().as_str() {
            "new" | "newest" | "time_desc" => Self::Newest,
            "old" | "oldest" | "time_asc" => Self::Oldest,
            "size" | "largest" | "size_desc" => Self::Largest,
            "smallest" | "size_asc" => Self::Smallest,
            "name" | "name_asc" => Self::NameAsc,
            "name_desc" => Self::NameDesc,
            "type" | "type_asc" | "ext" => Self::TypeAsc,
            "hot" | "hottest" => Self::Hottest,
            _ => Self::Shuffle,
        }
    }

    /// 排序表达式。`seed` 只有 `Shuffle` 用得上。
    ///
    /// 打乱用的是「id 乘一个种子再取模」：它是个确定的伪随机排列，
    /// 同一个种子翻到第几页都是同一份顺序，换一次种子就等于重洗一次牌。
    /// 种子是解析出来的整数（不是字符串拼接），直接写字面量没有注入面。
    fn order_by(self, seed: u64) -> String {
        match self {
            Self::Shuffle => {
                // 取一个接近 2^31 的质数做模，避免整除导致的规律性空档
                let seed = seed.max(1);
                format!("((i.id * {seed}) % 2147483647) ASC, i.id ASC")
            }
            Self::Newest => "i.upload_time DESC, i.id DESC".to_string(),
            Self::Oldest => "i.upload_time ASC, i.id ASC".to_string(),
            Self::Largest => "b.size DESC, i.id DESC".to_string(),
            Self::Smallest => "b.size ASC, i.id ASC".to_string(),
            Self::NameAsc => "COALESCE(i.title, i.original_name) ASC, i.id ASC".to_string(),
            Self::NameDesc => "COALESCE(i.title, i.original_name) DESC, i.id DESC".to_string(),
            // 文件类型：先按扩展名分组，组内新图在前
            Self::TypeAsc => "LOWER(b.ext) ASC, i.upload_time DESC, i.id DESC".to_string(),
            Self::Hottest => "i.views DESC, i.upload_time DESC, i.id DESC".to_string(),
        }
    }
}

/// 列表与广场共用的过滤条件。
#[derive(Debug, Clone, Default)]
pub struct ImageFilter {
    pub owner_id: Option<i64>,
    pub album_id: Option<i64>,
    /// 只要这些相册里的图片（广场的「只展示选中的公开相册」用它）
    pub album_ids: Option<Vec<i64>>,
    /// 只要公开图片（广场、未登录浏览）
    pub public_only: bool,
    /// 标题或标签包含该关键词
    pub query: Option<String>,
    /// 精确命中某个标签
    pub tag: Option<String>,
    /// 任一命中（OR）的一组标签。广场的标签多选走这里；
    /// 与 `tag` 同时给出时两条条件都要满足。
    pub tags_any: Vec<String>,
    /// 全部命中（AND）的一组标签。
    pub tags_all: Vec<String>,
    /// 只要指定的一批 id（批量操作前回读用）
    pub ids: Option<Vec<i64>>,
    /// 包含已软删的图片（回收站/审计才需要）
    pub include_deleted: bool,
    pub sort: ImageSort,
    /// 打乱排序的种子。0 表示让服务端随机挑一个
    pub seed: u64,
}

/// 随机图查询的扩展条件。与列表的 ImageFilter 分开，避免把广场列表没用到
/// 的方向/宽高/时间窗口/排除 id 等字段塞进通用过滤器。
#[derive(Debug, Clone, Default)]
pub struct RandomFilter {
    pub owner_id: Option<i64>,
    /// 只在这些相册里挑；空 = 不限
    pub album_ids: Vec<i64>,
    pub public_only: bool,
    /// `public` / `private`；None 表示不按图片可见性加条件
    pub visibility: Option<String>,
    pub query: Option<String>,
    /// 全部命中（AND）
    pub tags_all: Vec<String>,
    /// 任一命中（OR）
    pub tags_any: Vec<String>,
    pub exclude_tag: Option<String>,
    pub exclude_ids: Vec<i64>,
    pub min_width: Option<i64>,
    pub min_height: Option<i64>,
    /// landscape / portrait / square
    pub orientation: Option<String>,
    /// 目标宽高比（宽 / 高），带 3% 容差
    pub aspect_ratio: Option<f64>,
    pub uploaded_after: Option<DateTime<Utc>>,
    pub uploaded_before: Option<DateTime<Utc>>,
    /// 从最近 500 张里做一次偏向新图的加权抽样
    pub weight_recent: bool,
    /// 0 = 每次随机；非 0 时用确定性洗牌，方便轮播保持稳定
    pub seed: u64,
}

impl ImageFilter {
    /// 把 0 号种子换成一个随机值，让「默认排序」每次都不一样。
    pub fn resolved_seed(&self) -> u64 {
        if self.seed != 0 {
            return self.seed;
        }
        // 只借用时间戳的低位：够用了，这里要的是「每次刷新不一样」
        (chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64 % 2_000_000_000).max(1)
    }

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
    /// 打乱排序的种子，与 `ImageFilter::seed` 同一个约定
    pub seed: u64,
}

impl Repo {
    // ---------- 读取 ----------

    pub async fn find_image(&self, id: i64) -> Result<Option<Image>> {
        let sql = format!("SELECT {} FROM images WHERE id = ?", Image::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Image::from_row).transpose()
    }

    /// 这个账号名下引用着某份实体的活图片（按 id 最早的那张）。
    ///
    /// 「已有就复用」的上传靠它：头像这类同一张图反复设的场景，第二次起
    /// 直接引用图库里那张，不再多出一条记录。只看活着的引用——回收站里的
    /// 那张不该被悄悄翻出来用。
    pub async fn find_live_image_by_blob(
        &self,
        owner_id: i64,
        blob_id: i64,
    ) -> Result<Option<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE owner_id = ? AND blob_id = ? AND deleted_at IS NULL ORDER BY id LIMIT 1",
            Image::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(owner_id).int(blob_id))
            .await?;
        row.as_ref().map(Image::from_row).transpose()
    }

    /// 找一条还活着的图片行，用来给「下载」取原始文件名。
    ///
    /// 同一个实体可能被多人引用（内容去重）：优先给请求者自己那条，
    /// 他传的名字才是他认得的那个；没有就随便给一条活着的。
    pub async fn find_live_image_for_download(
        &self,
        blob_id: i64,
        prefer_owner: Option<i64>,
    ) -> Result<Option<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE blob_id = ? AND deleted_at IS NULL ORDER BY id LIMIT 1",
            Image::COLUMNS
        );
        if let Some(owner_id) = prefer_owner {
            let own = format!(
                "SELECT {} FROM images WHERE blob_id = ? AND owner_id = ? AND deleted_at IS NULL ORDER BY id LIMIT 1",
                Image::COLUMNS
            );
            if let Some(row) = self
                .db
                .fetch_optional(&own, Args::new().int(blob_id).int(owner_id))
                .await?
            {
                return Ok(Some(Image::from_row(&row)?));
            }
        }
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(blob_id))
            .await?;
        row.as_ref().map(Image::from_row).transpose()
    }

    /// 单张图的完整视图（含物理属性、相册名与标签）。
    /// 换掉一张图的缩略图实体：新的一份引用加一，旧的一份减一。
    ///
    /// 与 `attach_image_thumb`（只补缺的）分开：重算缩略图时要**替换**已有的，
    /// 旧实体少减一次引用就会永远回收不掉。
    pub async fn replace_image_thumb(
        &self,
        image_id: i64,
        new_thumb_id: i64,
        old_thumb_id: Option<i64>,
    ) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            &self
                .db
                .sql("UPDATE images SET thumb_blob_id = ? WHERE id = ?"),
        )
        .bind(new_thumb_id)
        .bind(image_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            &self
                .db
                .sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"),
        )
        .bind(new_thumb_id)
        .execute(&mut *tx)
        .await?;
        if let Some(old_thumb_id) = old_thumb_id.filter(|old| *old != new_thumb_id) {
            let dec_ref = self.db.sql_v(
                "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
                "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
                "UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?",
            );
            sqlx::query(&dec_ref)
                .bind(old_thumb_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 缩略图偏小（或尺寸未知）的图片：原图本身够大、但挂着的缩略图不够用。
    ///
    /// 「够不够用」看缩略图实体上记的像素宽度（客户端与服务端都会写）。老数据没记
    /// 尺寸，`IS NULL` 一并捞出来重算一次，重算之后就记上了，不会反复重来。
    /// 原图本身就比目标尺寸小的不在此列——给它重算也变不出更多像素。
    pub async fn list_images_with_small_thumb(
        &self,
        min_edge: i32,
        max_source_bytes: i64,
        max_source_pixels: i64,
        limit: i64,
    ) -> Result<Vec<Image>> {
        // 三个表都有 id：列表要逐列带上 `i.` 前缀，否则 SQLite 会报列名歧义
        let columns = Image::COLUMNS
            .split(',')
            .map(|name| format!("i.{}", name.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        // 「够不够用」看**长边**：竖图的宽度天然比高度小（1024 高的图宽只有 683），
        // 拿宽度比会把一批竖图反复捞出来重算
        let long_edge = |alias: &str| {
            format!("(CASE WHEN {alias}.width >= {alias}.height THEN {alias}.width ELSE {alias}.height END)")
        };
        // 体积与像素超标的原图直接不选：那些重算也解不出来，选进来只会被反复跳过
        let sql = format!(
            "SELECT {columns} FROM images i \
               JOIN blobs b ON b.id = i.blob_id \
               JOIN blobs tb ON tb.id = i.thumb_blob_id \
              WHERE i.deleted_at IS NULL \
                AND b.width IS NOT NULL AND b.height IS NOT NULL \
                AND b.size <= ? AND b.width * b.height <= ? \
                AND {} >= ? \
                AND (tb.width IS NULL OR tb.height IS NULL OR {} < ?) \
              ORDER BY i.id ASC LIMIT ?",
            long_edge("b"),
            long_edge("tb")
        );
        let rows = self
            .db
            .fetch_all(
                &sql,
                Args::new()
                    .int(max_source_bytes)
                    .int(max_source_pixels)
                    .int(min_edge as i64)
                    .int(min_edge as i64)
                    .int(limit.clamp(1, 500)),
            )
            .await?;
        rows.iter().map(Image::from_row).collect()
    }

    /// 按 id 递增取一批活着的图片（缩略图重算按游标推进）。
    pub async fn list_images_after_id(&self, after_id: i64, limit: i64) -> Result<Vec<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE id > ? AND deleted_at IS NULL ORDER BY id ASC LIMIT ?",
            Image::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(after_id).int(limit.clamp(1, 500)))
            .await?;
        rows.iter().map(Image::from_row).collect()
    }

    /// 某份实体在该账号名下、被标成头像的那张图（最早的）。换头像之后的清理靠它。
    ///
    /// 只认带 `avatar` 标签的：用户把相册里某张照片的直链手动填成头像时不会打这个
    /// 标签，换头像也就不该动那张照片。
    pub async fn find_avatar_image(&self, owner_id: i64, blob_id: i64) -> Result<Option<Image>> {
        let sql = format!(
            "SELECT {} FROM images i \
              WHERE i.owner_id = ? AND i.blob_id = ? AND i.deleted_at IS NULL \
                AND EXISTS ( \
                    SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                     WHERE it.image_id = i.id AND t.owner_id = ? AND t.name = 'avatar') \
              ORDER BY i.id LIMIT 1",
            Image::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(owner_id).int(blob_id).int(owner_id))
            .await?;
        row.as_ref().map(Image::from_row).transpose()
    }

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
        let mut views: Vec<ImageView> = rows
            .iter()
            .map(ImageView::from_row)
            .collect::<Result<_>>()?;
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
            filter.sort.order_by(filter.resolved_seed())
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(page.per_page).int(page.offset()))
            .await?;
        let mut views: Vec<ImageView> = rows
            .iter()
            .map(ImageView::from_row)
            .collect::<Result<_>>()?;
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
                seed: filter.seed,
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
        let mut views: Vec<ImageView> = rows
            .iter()
            .map(ImageView::from_row)
            .collect::<Result<_>>()?;
        let ids: Vec<i64> = views.iter().map(|view| view.id).collect();
        let mut tag_map = self.tags_for_images(&ids).await?;
        for view in &mut views {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(views)
    }

    /// 相册维度的文件数与字节数，`fs/tree` 与容量展示都要。
    /// 从回收站恢复一条引用：`delete_image_reference` 的逆操作。
    ///
    /// 引用计数与相册张数要回到原样。**账号用量不动**：回收站里的文件一直算在
    /// 已用额度里（在那里躺着也占着盘），只有彻底删除时才把字节数还回去。
    /// `count_album` 为 false 用于「随相册一起恢复」——那批图的张数在相册软删时
    /// 也没减过，这里自然不该加，否则来回删/恢复几次计数就飘了。
    pub async fn restore_image_reference(
        &self,
        image: &Image,
        size: i64,
        count_album: bool,
    ) -> Result<()> {
        let _ = size;
        let mut tx = self.db.pool().begin().await?;
        let stamp = now_ts();

        sqlx::query(
            &self
                .db
                .sql("UPDATE images SET deleted_at = NULL, deleted_by_album = 0 WHERE id = ? AND deleted_at IS NOT NULL"),
        )
        .bind(image.id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            &self
                .db
                .sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"),
        )
        .bind(image.blob_id)
        .execute(&mut *tx)
        .await?;
        if let Some(thumb_id) = image.thumb_blob_id {
            sqlx::query(
                &self
                    .db
                    .sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"),
            )
            .bind(thumb_id)
            .execute(&mut *tx)
            .await?;
        }

        if count_album {
            if let Some(album_id) = image.album_id {
                sqlx::query(&self.db.sql(
                    "UPDATE albums SET image_count = image_count + 1, updated_at = ? WHERE id = ?",
                ))
                .bind(&stamp)
                .bind(album_id)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// 彻底删掉一条墓碑（只对已软删的行生效）。物理文件由 GC 按引用计数回收。
    ///
    /// 这一步才是真正「腾出额度」的地方：回收站里的文件一直算在已用额度里，
    /// 所以删行与退额度必须在同一个事务里做完。
    pub async fn purge_image_row(&self, id: i64) -> Result<Option<PurgedBlobs>> {
        let Some(row) = self
            .db
            .fetch_optional(
                "SELECT i.owner_id AS owner_id, b.size AS size, i.blob_id AS blob_id, \
                        i.thumb_blob_id AS thumb_blob_id \
                   FROM images i JOIN blobs b ON b.id = i.blob_id \
                  WHERE i.id = ? AND i.deleted_at IS NOT NULL",
                Args::new().int(id),
            )
            .await?
        else {
            // 不存在或没进回收站：当作没删，不报错
            return Ok(None);
        };
        let owner_id = get_i64(&row, "owner_id")?;
        let size = get_i64(&row, "size")?;
        let purged = PurgedBlobs {
            blob_id: get_i64(&row, "blob_id")?,
            thumb_blob_id: get_opt_i64(&row, "thumb_blob_id")?,
        };

        let mut tx = self.db.pool().begin().await?;
        let deleted = sqlx::query(
            &self
                .db
                .sql("DELETE FROM images WHERE id = ? AND deleted_at IS NOT NULL"),
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted > 0 {
            self.subtract_used_bytes(&mut tx, owner_id, size).await?;
        }
        tx.commit().await?;
        Ok(if deleted > 0 { Some(purged) } else { None })
    }

    /// 在事务里把账号的已用额度减掉 `bytes`（不足则归零）。
    ///
    /// `MAX(a, b)` 两个参数的写法只有 SQLite 认，PostgreSQL 与 MySQL 都要
    /// `GREATEST`，所以这类「减到 0 为止」的语句统一按方言出。
    async fn subtract_used_bytes(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        owner_id: i64,
        bytes: i64,
    ) -> Result<()> {
        let sql = self.db.sql_v(
            "UPDATE users SET used_bytes = GREATEST(used_bytes - ?, 0) WHERE id = ?",
            "UPDATE users SET used_bytes = GREATEST(used_bytes - ?, 0) WHERE id = ?",
            "UPDATE users SET used_bytes = MAX(used_bytes - ?, 0) WHERE id = ?",
        );
        sqlx::query(&sql)
            .bind(bytes)
            .bind(owner_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// 数一下回收站里有多少张图（按账号）。
    pub async fn count_deleted_images(&self, owner_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM images WHERE owner_id = ? AND deleted_at IS NOT NULL",
                Args::new().int(owner_id),
            )
            .await
    }

    /// 回收站列表：只列墓碑，按移入时间倒序。形状与图片列表一致，前端直接渲染。
    pub async fn list_deleted_images(
        &self,
        owner_id: i64,
        page: PageRequest,
    ) -> Result<Page<ImageView>> {
        let total = self.count_deleted_images(owner_id).await?;
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "{IMAGE_VIEW_SQL} WHERE i.owner_id = ? AND i.deleted_at IS NOT NULL \
                     ORDER BY i.deleted_at DESC, i.id DESC LIMIT ? OFFSET ?"
                ),
                Args::new()
                    .int(owner_id)
                    .int(page.per_page)
                    .int(page.offset()),
            )
            .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            items.push(ImageView::from_row(row)?);
        }
        let ids: Vec<i64> = items.iter().map(|item| item.id).collect();
        let mut tag_map = self.tags_for_images(&ids).await?;
        for view in items.iter_mut() {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(Page::new(items, total, page))
    }

    /// 清掉某个实体上「已经过期」的墓碑（GC 回收物理文件前用）。
    ///
    /// 外键挡着实体行不让删，所以得先把墓碑摘掉；但只摘过期的——
    /// 保留期内的墓碑是回收站，文件要留给恢复用。
    pub async fn purge_tombstones_for_blob_before(
        &self,
        blob_id: i64,
        cutoff: DateTime<Utc>,
    ) -> Result<u64> {
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT owner_id, {} AS bytes FROM ( \
                       SELECT i.owner_id AS owner_id, b.size AS size FROM images i \
                       JOIN blobs b ON b.id = i.blob_id \
                       WHERE (i.blob_id = ? OR i.thumb_blob_id = ?) AND i.deleted_at IS NOT NULL \
                         AND i.deleted_at < ? \
                     ) AS gone GROUP BY owner_id",
                    self.db.dialect().sum_i64("size")
                ),
                Args::new().int(blob_id).int(blob_id).text(ts(cutoff)),
            )
            .await?;
        let mut per_owner = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            per_owner.push((get_i64(row, "owner_id")?, get_i64(row, "bytes")?));
        }

        let mut tx = self.db.pool().begin().await?;
        let deleted = sqlx::query(&self.db.sql(
            "DELETE FROM images WHERE (blob_id = ? OR thumb_blob_id = ?) \
                 AND deleted_at IS NOT NULL AND deleted_at < ?",
        ))
        .bind(blob_id)
        .bind(blob_id)
        .bind(ts(cutoff))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted > 0 {
            for (owner_id, bytes) in per_owner {
                self.subtract_used_bytes(&mut tx, owner_id, bytes).await?;
            }
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// 清空某个账号的回收站。返回删掉的墓碑各自对应的实体，供调用方当场回收。
    pub async fn purge_deleted_images(
        &self,
        owner_id: i64,
        limit: i64,
    ) -> Result<Vec<PurgedBlobs>> {
        let limit = limit.clamp(1, 20_000);
        // 先记下这批墓碑的字节数与实体，删完从额度里扣掉——顺序不能反，
        // 行没了就算不出大小，也不知道该回收哪些实体了
        let rows = self
            .db
            .fetch_all(
                "SELECT b.size AS size, i.blob_id AS blob_id, i.thumb_blob_id AS thumb_blob_id \
                   FROM images i JOIN blobs b ON b.id = i.blob_id \
                  WHERE i.owner_id = ? AND i.deleted_at IS NOT NULL \
                  ORDER BY i.deleted_at ASC LIMIT ?",
                Args::new().int(owner_id).int(limit),
            )
            .await?;
        let mut bytes = 0i64;
        let mut targets = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            bytes += get_i64(row, "size")?;
            targets.push(PurgedBlobs {
                blob_id: get_i64(row, "blob_id")?,
                thumb_blob_id: get_opt_i64(row, "thumb_blob_id")?,
            });
        }

        let mut tx = self.db.pool().begin().await?;
        let deleted = sqlx::query(&self.db.sql(
            "DELETE FROM images WHERE id IN ( \
                   SELECT id FROM images WHERE owner_id = ? AND deleted_at IS NOT NULL \
                   ORDER BY deleted_at ASC LIMIT ?)",
        ))
        .bind(owner_id)
        .bind(limit)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted > 0 {
            self.subtract_used_bytes(&mut tx, owner_id, bytes).await?;
        }
        tx.commit().await?;
        Ok(if deleted > 0 { targets } else { Vec::new() })
    }

    /// 相册里还活着的图片行（软删相册时逐张走删除流程用）。
    pub async fn list_album_live_images(&self, album_id: i64) -> Result<Vec<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE album_id = ? AND deleted_at IS NULL ORDER BY id",
            Image::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(album_id)).await?;
        rows.iter().map(Image::from_row).collect()
    }

    /// 相册里「随相册一起」进回收站的图片行（恢复相册时只带回这一批）。
    pub async fn list_album_binned_images(&self, album_id: i64) -> Result<Vec<Image>> {
        let sql = format!(
            "SELECT {} FROM images WHERE album_id = ? AND deleted_at IS NOT NULL \
             AND deleted_by_album = ? ORDER BY id",
            Image::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(album_id).bool(true))
            .await?;
        rows.iter().map(Image::from_row).collect()
    }

    /// 某个相册里所有墓碑的 id（彻底删除相册时用）。
    pub async fn deleted_image_ids_in_album(&self, album_id: i64, limit: i64) -> Result<Vec<i64>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT id FROM images WHERE album_id = ? AND deleted_at IS NOT NULL \
                 ORDER BY id LIMIT ?",
                Args::new().int(album_id).int(limit.clamp(1, 20_000)),
            )
            .await?;
        rows.iter().map(|row| get_i64(row, "id")).collect()
    }

    /// 这个实体上还有多少张「仍在保留期内」的墓碑。
    ///
    /// 回收站靠它把物理文件留住：只要还有没到期的墓碑，文件就不能收
    /// （收走了就恢复不出来了）。到期的墓碑由 `purge_expired_tombstones` 清掉。
    pub async fn count_tombstones_after(&self, blob_id: i64, cutoff: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM images \
                 WHERE (blob_id = ? OR thumb_blob_id = ?) AND deleted_at IS NOT NULL AND deleted_at >= ?",
                Args::new().int(blob_id).int(blob_id).text(ts(cutoff)),
            )
            .await
    }

    /// 清掉已经过了保留期的墓碑，返回清掉几条。
    ///
    /// 这一步由后台按天跑，也要退额度：到期的内容不再属于任何人的回收站。
    pub async fn purge_expired_tombstones(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<PurgedBlobs>> {
        let limit = limit.clamp(1, 5000);
        let rows = self
            .db
            .fetch_all(
                "SELECT i.id AS id, i.owner_id AS owner_id, b.size AS size, \
                        i.blob_id AS blob_id, i.thumb_blob_id AS thumb_blob_id \
                   FROM images i JOIN blobs b ON b.id = i.blob_id \
                  WHERE i.deleted_at IS NOT NULL AND i.deleted_at < ? \
                  ORDER BY i.deleted_at ASC LIMIT ?",
                Args::new().text(ts(cutoff)).int(limit),
            )
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut per_owner: HashMap<i64, i64> = HashMap::new();
        let mut ids = Vec::with_capacity(rows.len());
        let mut targets = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            ids.push(get_i64(row, "id")?);
            *per_owner.entry(get_i64(row, "owner_id")?).or_default() += get_i64(row, "size")?;
            targets.push(PurgedBlobs {
                blob_id: get_i64(row, "blob_id")?,
                thumb_blob_id: get_opt_i64(row, "thumb_blob_id")?,
            });
        }

        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = self
            .db
            .sql(&format!("DELETE FROM images WHERE id IN ({placeholders})"));
        let mut query = sqlx::query(&sql);
        for id in &ids {
            query = query.bind(*id);
        }

        let mut tx = self.db.pool().begin().await?;
        let deleted = query.execute(&mut *tx).await?.rows_affected();
        if deleted > 0 {
            for (owner_id, bytes) in per_owner {
                self.subtract_used_bytes(&mut tx, owner_id, bytes).await?;
            }
        }
        tx.commit().await?;
        Ok(if deleted > 0 { targets } else { Vec::new() })
    }

    /// 按范围取还活着的图片 id：`album_id` 为 `None` 表示不限相册（全部图片）。
    ///
    /// 给「整相册批量操作」用：前端不必先把几千个 id 拉到浏览器再发回来，
    /// 服务端自己按条件取。上限由调用方给，避免一次把整库拖进内存。
    pub async fn image_ids_in_scope(
        &self,
        owner_id: i64,
        album_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<i64>> {
        let (sql, args) = match album_id {
            Some(album_id) => (
                "SELECT id FROM images WHERE owner_id = ? AND album_id = ? AND deleted_at IS NULL                  ORDER BY id LIMIT ?"
                    .to_string(),
                Args::new().int(owner_id).int(album_id).int(limit.clamp(1, 20_000)),
            ),
            None => (
                "SELECT id FROM images WHERE owner_id = ? AND deleted_at IS NULL ORDER BY id LIMIT ?"
                    .to_string(),
                Args::new().int(owner_id).int(limit.clamp(1, 20_000)),
            ),
        };
        let rows = self.db.fetch_all(&sql, args).await?;
        rows.iter().map(|row| get_i64(row, "id")).collect()
    }

    /// 相册里还活着的图片引用的实体（删除相册时用来清镜像树）。
    ///
    /// 列名统一加 `b.` 前缀并 `AS` 回裸名：JOIN 之后 `id` 会撞名，
    /// 而 `from_row` 是按裸列名读的。
    pub async fn album_original_blobs(&self, album_id: i64) -> Result<Vec<crate::model::Blob>> {
        const COLS: &str = "b.id AS id, b.sha256 AS sha256, b.kind AS kind, b.size AS size,              b.mime_type AS mime_type, b.ext AS ext, b.width AS width, b.height AS height,              b.storage_driver AS storage_driver, b.storage_key AS storage_key,              b.backend_id AS backend_id, b.ref_count AS ref_count, b.created_at AS created_at";
        let sql = format!(
            "SELECT {COLS} FROM blobs b JOIN images i ON i.blob_id = b.id              WHERE i.album_id = ? AND i.deleted_at IS NULL AND b.kind = 'original'"
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(album_id)).await?;
        rows.iter().map(crate::model::Blob::from_row).collect()
    }

    pub async fn album_aggregates(&self, owner_id: i64) -> Result<Vec<AlbumAggregate>> {
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT i.album_id AS album_id, COUNT(*) AS files, {} AS bytes \
                       FROM images i JOIN blobs b ON b.id = i.blob_id \
                      WHERE i.owner_id = ? AND i.deleted_at IS NULL \
                      GROUP BY i.album_id",
                    self.db.dialect().sum_i64("b.size")
                ),
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
                                visibility, views, upload_time, deleted_at, upload_ip, upload_agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL, ?, ?) RETURNING id",
            "INSERT INTO images (owner_id, album_id, blob_id, thumb_blob_id, original_name, title, \
                                visibility, views, upload_time, deleted_at, upload_ip, upload_agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL, ?, ?)",
            "INSERT INTO images (owner_id, album_id, blob_id, thumb_blob_id, original_name, title, \
                                visibility, views, upload_time, deleted_at, upload_ip, upload_agent) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, NULL, ?, ?) RETURNING id",
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
                .bind(new.upload_ip.clone())
                .bind(new.upload_agent.clone())
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
                .bind(new.upload_ip.clone())
                .bind(new.upload_agent.clone())
                .fetch_one(&mut *tx)
                .await?;
            get_i64(&row, "id")?
        };

        sqlx::query(
            &self
                .db
                .sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"),
        )
        .bind(new.blob_id)
        .execute(&mut *tx)
        .await?;
        if let Some(thumb_id) = new.thumb_blob_id {
            sqlx::query(
                &self
                    .db
                    .sql("UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?"),
            )
            .bind(thumb_id)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            &self
                .db
                .sql("UPDATE users SET used_bytes = used_bytes + ? WHERE id = ?"),
        )
        .bind(blob_size)
        .bind(new.owner_id)
        .execute(&mut *tx)
        .await?;

        if let Some(album_id) = new.album_id {
            sqlx::query(&self.db.sql(
                "UPDATE albums SET image_count = image_count + 1, updated_at = ? WHERE id = ?",
            ))
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

    /// 软删除一张引用，并把引用计数减一。
    ///
    /// **账号用量不动**：进回收站的文件还占着盘，也继续算在已用额度里，
    /// 只有彻底删除（`purge_*`）才把字节数还回去。
    pub async fn delete_image_reference(
        &self,
        deletion: &ImageDeletion,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        let stamp = ts(at);

        sqlx::query(
            &self
                .db
                .sql(
                    "UPDATE images SET deleted_at = ?, deleted_by_album = ? WHERE id = ? AND deleted_at IS NULL",
                ),
        )
        .bind(&stamp)
        // 列是 BIGINT（0/1）：直接绑 bool 在 PostgreSQL 上会报
        // 「column deleted_by_album is of type bigint but expression is of type boolean」
        .bind(i64::from(deletion.by_album))
        .bind(deletion.image_id)
        .execute(&mut *tx)
        .await?;

        let dec_ref = self.db.sql_v(
            "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
            "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
            "UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?",
        );
        sqlx::query(&dec_ref)
            .bind(deletion.blob_id)
            .execute(&mut *tx)
            .await?;
        if let Some(thumb_id) = deletion.thumb_blob_id {
            sqlx::query(&dec_ref)
                .bind(thumb_id)
                .execute(&mut *tx)
                .await?;
        }

        if let Some(album_id) = deletion.album_id {
            sqlx::query(
                &self
                    .db
                    .sql_v(
                        "UPDATE albums SET image_count = GREATEST(image_count - 1, 0), updated_at = ? WHERE id = ?",
                        "UPDATE albums SET image_count = GREATEST(image_count - 1, 0), updated_at = ? WHERE id = ?",
                        "UPDATE albums SET image_count = MAX(image_count - 1, 0), updated_at = ? WHERE id = ?",
                    ),
            )
            .bind(&stamp)
            .bind(album_id)
            .execute(&mut *tx)
            .await?;
        }

        // 标签链接留着：进回收站的图仍然带着自己的标签，
        // 「恢复」才能把内容完整地带回来（标签也是用户写的内容）。
        // 真正清掉它们的是彻底删除——`images` 行没了，`image_tags` 的外键级联跟着走。
        tx.commit().await?;
        Ok(())
    }

    /// 批量软删。返回实际删除的张数。
    ///
    /// 逐张在同一个事务里删：跨表计数必须成对变化，一次性 `UPDATE ... WHERE id IN (...)`
    /// 做不到「按每张图的大小回退用量」。
    pub async fn delete_images(
        &self,
        deletions: &[ImageDeletion],
        at: DateTime<Utc>,
    ) -> Result<u64> {
        let mut removed = 0u64;
        for deletion in deletions {
            self.delete_image_reference(deletion, at).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// 有哪些账号名下有「没有归属的图片」。
    ///
    /// 早期版本允许「移出相册」，留下了一批 `album_id` 为空的行；现在图片
    /// 总要落在某个相册里（默认相册兜底），所以启动/对账时把它们收编。
    pub async fn owners_with_unattached_images(&self) -> Result<Vec<i64>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT DISTINCT owner_id FROM images WHERE album_id IS NULL",
                Args::new(),
            )
            .await?;
        rows.iter().map(|row| get_i64(row, "owner_id")).collect()
    }

    /// 把某个账号名下没有归属的图片收进指定相册，并维护相册计数。
    pub async fn attach_images_to_album(&self, owner_id: i64, album_id: i64) -> Result<u64> {
        let mut tx = self.db.pool().begin().await?;
        let moved = sqlx::query(
            &self
                .db
                .sql("UPDATE images SET album_id = ? WHERE owner_id = ? AND album_id IS NULL"),
        )
        .bind(album_id)
        .bind(owner_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved > 0 {
            sqlx::query(&self.db.sql(
                "UPDATE albums SET image_count = image_count + ?, updated_at = ? WHERE id = ?",
            ))
            .bind(moved as i64)
            .bind(now_ts())
            .bind(album_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(moved)
    }

    /// 批量移动到相册。
    ///
    /// `None` 表示「不指定」——落点是主人的**默认相册**，不再有「移出相册」：
    /// 图片总得有个归属，界面上那个「未分类」状态已经取消了。
    pub async fn move_images(
        &self,
        ids: &[i64],
        owner_id: i64,
        album_id: Option<i64>,
    ) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 不指定相册时落进默认相册：解析放在这里而不是各个调用方，
        // 免得「有的入口会落进去、有的入口留下未分类」
        let album_id = match album_id {
            Some(id) => Some(id),
            None => self
                .find_album_by_slug(owner_id, crate::repo::albums::DEFAULT_ALBUM_SLUG)
                .await?
                .map(|album| album.id),
        };
        let placeholders = vec!["?"; ids.len()].join(", ");
        let now = now_ts();

        // 先回读旧归属，才能把两个相册的计数都改对
        let old_rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT id, album_id FROM images WHERE owner_id = ? AND id IN ({placeholders})"
                ),
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
                    &self.db.sql_v(
                        "UPDATE albums SET image_count = GREATEST(image_count - 1, 0), updated_at = ? WHERE id = ?",
                        "UPDATE albums SET image_count = GREATEST(image_count - 1, 0), updated_at = ? WHERE id = ?",
                        "UPDATE albums SET image_count = MAX(image_count - 1, 0), updated_at = ? WHERE id = ?",
                    ),
                )
                .bind(&now)
                .bind(old)
                .execute(&mut *tx)
                .await?;
            }
            if let Some(new_album) = album_id {
                sqlx::query(&self.db.sql(
                    "UPDATE albums SET image_count = image_count + 1, updated_at = ? WHERE id = ?",
                ))
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
            let result = sqlx::query_with(&sql, args.take())
                .execute(&mut *tx)
                .await?;
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

    /// 批量追加标签：保留原有标签，只把缺少的那些补上。
    ///
    /// 与 [`Self::update_images`] 的整组替换不同——相册页的「打标签」按钮
    /// 和界面文案都是追加语义，服务端不能顺手把旧标签清掉。
    pub async fn append_images_tags(
        &self,
        ids: &[i64],
        owner_id: i64,
        tags: &[String],
    ) -> Result<u64> {
        let names = normalize_tag_list(tags);
        if ids.is_empty() || names.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db.pool().begin().await?;
        let mut touched = 0u64;
        for id in ids {
            for name in &names {
                let tag_id = ensure_tag_on(&self.db, &mut tx, owner_id, name).await?;
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
        tx.commit().await?;
        Ok(touched)
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

    /// 浏览一次，返回自增后的浏览量。广场的「最热」用得上。
    ///
    /// PostgreSQL / SQLite 本可以用 `RETURNING`，但 MySQL 不支持同一写法；
    /// 这里统一先更新再读一次，两条语句都不重，读到的也是并发累加后的真实值。
    pub async fn increment_views(&self, id: i64) -> Result<i64> {
        self.db
            .execute(
                "UPDATE images SET views = views + 1 WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        let row = self
            .db
            .fetch_one("SELECT views FROM images WHERE id = ?", Args::new().int(id))
            .await?;
        get_i64(&row, "views")
    }

    /// 随机取一张图。给随机图 API 用：博客头图、桌面壁纸这类场景。
    ///
    /// 无筛选的公开图走「随机 id + 主键范围」的快路径：`ORDER BY RANDOM()`
    /// 在几十万行上要全表扫一遍再排序，而随机图 API 可能被首页每隔几秒调一次。
    /// 带搜索/标签/相册的请求仍走原来的随机排序，保证语义不变。
    pub async fn random_image(&self, filter: &ImageFilter) -> Result<Option<ImageView>> {
        let simple_public = filter.public_only
            && !filter.include_deleted
            && filter.owner_id.is_none()
            && filter.album_id.is_none()
            && filter.query.is_none()
            && filter.tag.is_none()
            && filter.tags_any.is_empty()
            && filter.tags_all.is_empty()
            && filter.ids.is_none();
        if simple_public {
            if let Some(view) = self.random_public_image_fast().await? {
                return Ok(Some(view));
            }
            return Ok(None);
        }

        let (where_sql, args) = self.image_where(filter);
        let order = self.db.sql_v(
            "ORDER BY RANDOM() LIMIT 1",
            "ORDER BY RAND() LIMIT 1",
            "ORDER BY RANDOM() LIMIT 1",
        );
        // ORDER BY 里的 ORDER 是方言差异，占位符在 where 里，各自独立拼接
        let head = self.db.sql(&format!("{IMAGE_VIEW_SQL}{where_sql}"));
        let sql = format!("{head} {order}");
        let rows = self.db.fetch_all(&sql, args).await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        Ok(Some(ImageView::from_row(row)?))
    }

    /// 扩展随机图查询：支持相册、标签逻辑、方向/尺寸、时间窗口、排除 id、
    /// 加权新图与确定性种子。`count` 最多 50，避免一次请求把整库拖出来。
    pub async fn random_images(
        &self,
        filter: &RandomFilter,
        count: i64,
    ) -> Result<Vec<ImageView>> {
        let count = count.clamp(1, 50);
        // 无筛选的单张公开图继续走主键随机快路径，避免每次全表排序。
        let simple_public = count == 1
            && filter.seed == 0
            && !filter.weight_recent
            && filter.public_only
            && filter.owner_id.is_none()
            && filter.album_ids.is_empty()
            && filter.visibility.is_none()
            && filter.query.is_none()
            && filter.tags_all.is_empty()
            && filter.tags_any.is_empty()
            && filter.exclude_tag.is_none()
            && filter.exclude_ids.is_empty()
            && filter.min_width.is_none()
            && filter.min_height.is_none()
            && filter.orientation.is_none()
            && filter.aspect_ratio.is_none()
            && filter.uploaded_after.is_none()
            && filter.uploaded_before.is_none();
        if simple_public {
            if let Some(view) = self.random_public_image_fast().await? {
                let mut views = vec![view];
                self.fill_random_tags(&mut views).await?;
                return Ok(views);
            }
            return Ok(Vec::new());
        }

        let (where_sql, args) = self.random_where(filter);

        if filter.weight_recent {
            let sql =
                format!("{IMAGE_VIEW_SQL}{where_sql} ORDER BY i.upload_time DESC LIMIT 500");
            let rows = self.db.fetch_all(&sql, args).await?;
            let views: Vec<ImageView> = rows
                .iter()
                .map(ImageView::from_row)
                .collect::<Result<_>>()?;
            if views.is_empty() {
                return Ok(Vec::new());
            }
            let seed = if filter.seed == 0 {
                random_u64()
            } else {
                filter.seed
            };
            let picks = weighted_indices(views.len(), count as usize, seed);
            let mut chosen: Vec<ImageView> =
                picks.into_iter().map(|index| views[index].clone()).collect();
            self.fill_random_tags(&mut chosen).await?;
            return Ok(chosen);
        }

        let order = if filter.seed == 0 {
            self.db
                .sql_v("ORDER BY RANDOM()", "ORDER BY RAND()", "ORDER BY RANDOM()")
        } else {
            // 用 seed 做一次稳定的伪随机排序：同一个 seed 的轮播顺序不会跳。
            format!(
                "ORDER BY ((i.id * 1103515245 + {}) % 2147483647)",
                (filter.seed % 2_147_483_647) as i64
            )
        };
        let sql = format!("{IMAGE_VIEW_SQL}{where_sql} {order} LIMIT ?");
        let rows = self.db.fetch_all(&sql, args.int(count)).await?;
        let mut views: Vec<ImageView> = rows
            .iter()
            .map(ImageView::from_row)
            .collect::<Result<_>>()?;
        self.fill_random_tags(&mut views).await?;
        Ok(views)
    }

    async fn fill_random_tags(&self, views: &mut [ImageView]) -> Result<()> {
        let ids: Vec<i64> = views.iter().map(|view| view.id).collect();
        let mut tag_map = self.tags_for_images(&ids).await?;
        for view in views {
            view.tags = tag_map.remove(&view.id).unwrap_or_default();
        }
        Ok(())
    }

    fn random_where(&self, filter: &RandomFilter) -> (String, Args) {
        let mut builder = WhereBuilder::new();
        builder = builder.and("i.deleted_at IS NULL", |args| args);
        if filter.public_only {
            builder = builder.and("i.visibility = 'public'", |args| args);
        }
        if let Some(visibility) = &filter.visibility {
            let visibility = visibility.clone();
            builder = builder.and("i.visibility = ?", move |args| args.text(visibility));
        }
        if let Some(owner_id) = filter.owner_id {
            builder = builder.and("i.owner_id = ?", move |args| args.int(owner_id));
        }
        if !filter.album_ids.is_empty() {
            let placeholders = vec!["?"; filter.album_ids.len()].join(", ");
            let ids = filter.album_ids.clone();
            builder = builder.and(&format!("i.album_id IN ({placeholders})"), move |mut args| {
                for id in &ids {
                    args = args.int(*id);
                }
                args
            });
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
        for tag in &filter.tags_all {
            let tag = tag.clone();
            builder = builder.and(
                "EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                          WHERE it.image_id = i.id AND t.name = ?)",
                move |args| args.text(tag),
            );
        }
        if !filter.tags_any.is_empty() {
            let placeholders = vec!["?"; filter.tags_any.len()].join(", ");
            let tags = filter.tags_any.clone();
            builder = builder.and(
                &format!(
                    "EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                              WHERE it.image_id = i.id AND t.name IN ({placeholders}))"
                ),
                move |mut args| {
                    for tag in &tags {
                        args = args.text(tag.clone());
                    }
                    args
                },
            );
        }
        if let Some(tag) = &filter.exclude_tag {
            let tag = tag.clone();
            builder = builder.and(
                "NOT EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                              WHERE it.image_id = i.id AND t.name = ?)",
                move |args| args.text(tag),
            );
        }
        if !filter.exclude_ids.is_empty() {
            let placeholders = vec!["?"; filter.exclude_ids.len()].join(", ");
            let ids = filter.exclude_ids.clone();
            builder = builder.and(&format!("i.id NOT IN ({placeholders})"), move |mut args| {
                for id in &ids {
                    args = args.int(*id);
                }
                args
            });
        }
        if let Some(width) = filter.min_width {
            builder = builder.and("b.width >= ?", move |args| args.int(width));
        }
        if let Some(height) = filter.min_height {
            builder = builder.and("b.height >= ?", move |args| args.int(height));
        }
        match filter.orientation.as_deref() {
            Some("landscape") => builder = builder.and("b.width > b.height", |args| args),
            Some("portrait") => builder = builder.and("b.width < b.height", |args| args),
            Some("square") => {
                builder = builder.and(
                    "ABS(CAST(b.width AS DOUBLE PRECISION) - b.height) <= 0.03 * b.height",
                    |args| args,
                )
            }
            _ => {}
        }
        if let Some(ratio) = filter.aspect_ratio {
            let scaled = (ratio * 1_000_000.0).round() as i64;
            builder = builder.and(
                "ABS(CAST(b.width AS DOUBLE PRECISION) * 1000000 / NULLIF(b.height, 0) - ?) <= 30000",
                move |args| args.int(scaled),
            );
        }
        if let Some(after) = filter.uploaded_after {
            builder = builder.and("i.upload_time >= ?", move |args| args.text(ts(after)));
        }
        if let Some(before) = filter.uploaded_before {
            builder = builder.and("i.upload_time <= ?", move |args| args.text(ts(before)));
        }
        (builder.sql_string(), builder.args())
    }

    /// 公开图随机快路径：取 id 区间内的随机点，再找第一张 id 不小于它的公开图；
    /// 到末尾没有就回绕到第一张。仍然是均匀的「随机起点」，只是不保证每张图
    /// 被选中的概率完全相等（id 有空洞时会有偏差，对随机图 API 足够）。
    async fn random_public_image_fast(&self) -> Result<Option<ImageView>> {
        let bounds = self
            .db
            .fetch_optional(
                "SELECT MIN(id) AS min_id, MAX(id) AS max_id FROM images \
                  WHERE visibility = 'public' AND deleted_at IS NULL",
                Args::new(),
            )
            .await?;
        let Some(bounds) = bounds else {
            return Ok(None);
        };
        let (Some(min_id), Some(max_id)) = (
            get_opt_i64(&bounds, "min_id")?,
            get_opt_i64(&bounds, "max_id")?,
        ) else {
            return Ok(None);
        };
        let span = (max_id as i128 - min_id as i128 + 1).max(1) as u128;
        let target = min_id + (random_u64() as u128 % span) as i64;

        let forward = format!(
            "{IMAGE_VIEW_SQL} WHERE i.visibility = 'public' AND i.deleted_at IS NULL \
                  AND i.id >= ? ORDER BY i.id ASC LIMIT 1"
        );
        if let Some(row) = self
            .db
            .fetch_optional(&forward, Args::new().int(target))
            .await?
        {
            return Ok(Some(ImageView::from_row(&row)?));
        }

        let wrap = format!(
            "{IMAGE_VIEW_SQL} WHERE i.visibility = 'public' AND i.deleted_at IS NULL \
                  ORDER BY i.id ASC LIMIT 1"
        );
        let row = self.db.fetch_optional(&wrap, Args::new()).await?;
        row.as_ref().map(ImageView::from_row).transpose()
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
        if let Some(album_ids) = &filter.album_ids {
            if album_ids.is_empty() {
                builder = builder.and("1 = 0", |args| args);
            } else {
                let placeholders = vec!["?"; album_ids.len()].join(", ");
                let owned: Vec<i64> = album_ids.clone();
                builder = builder.and(
                    // 选中的相册必须仍是公开的：设置存的是 id，相册后来改成私有
                    // 不能靠旧设置继续把图放上广场
                    &format!(
                        "i.album_id IN ({placeholders}) AND EXISTS (\
                         SELECT 1 FROM albums pa WHERE pa.id = i.album_id \
                         AND pa.visibility = 'public' AND pa.deleted_at IS NULL)"
                    ),
                    move |mut args| {
                        for id in &owned {
                            args = args.int(*id);
                        }
                        args
                    },
                );
            }
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
        if !filter.tags_any.is_empty() {
            let placeholders = vec!["?"; filter.tags_any.len()].join(", ");
            let tags = filter.tags_any.clone();
            builder = builder.and(
                &format!(
                    "EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                      WHERE it.image_id = i.id AND t.name IN ({placeholders}))"
                ),
                move |mut args| {
                    for tag in &tags {
                        args = args.text(tag.clone());
                    }
                    args
                },
            );
        }
        for tag in &filter.tags_all {
            let tag = tag.clone();
            builder = builder.and(
                "EXISTS (SELECT 1 FROM image_tags it JOIN tags t ON t.id = it.tag_id                           WHERE it.image_id = i.id AND t.name = ?)",
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

/// 从按新到旧排列的候选里做加权抽样：越靠前权重越高，抽 count 个不重复。
fn weighted_indices(len: usize, count: usize, seed: u64) -> Vec<usize> {
    let mut available: Vec<usize> = (0..len).collect();
    let mut out = Vec::new();
    let mut state = seed.max(1);
    while out.len() < count.min(len) && !available.is_empty() {
        let total: f64 = available
            .iter()
            .map(|index| 1.0 / (*index as f64 + 1.0))
            .sum();
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let target = (state as f64 / u64::MAX as f64) * total;
        let mut acc = 0.0;
        let mut pick = available.len() - 1;
        for (position, index) in available.iter().enumerate() {
            acc += 1.0 / (*index as f64 + 1.0);
            if acc >= target {
                pick = position;
                break;
            }
        }
        out.push(available.remove(pick));
    }
    out
}

/// 给随机图用的轻量随机数。不需要密码学强度，只要每次调用分布不同。
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    hasher.finish()
}

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

impl Repo {
    /// 回收站里某个相册的图片数量与字节数（回收站列表要显示「带回来多少」）。
    pub async fn album_binned_totals(&self, album_id: i64) -> Result<(i64, i64)> {
        let row = self
            .db
            .fetch_optional(
                &format!(
                    "SELECT COUNT(*) AS files, {} AS bytes \
                       FROM images i JOIN blobs b ON b.id = i.blob_id \
                      WHERE i.album_id = ? AND i.deleted_at IS NOT NULL",
                    self.db.dialect().sum_i64("b.size")
                ),
                Args::new().int(album_id),
            )
            .await?;
        match row {
            Some(row) => Ok((get_i64(&row, "files")?, get_i64(&row, "bytes")?)),
            None => Ok((0, 0)),
        }
    }
}

impl Repo {
    /// 有多少张图片（含回收站里的墓碑）引用了这个实体。
    ///
    /// 「彻底删除后要不要马上删文件」看的就是它：只要还有一行引用——哪怕是
    /// 回收站里等待恢复的那一行——字节就得留着。原图和缩略图都算在内。
    pub async fn count_image_refs_for_blob(&self, blob_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM images \
                  WHERE blob_id = ? OR thumb_blob_id = ?",
                Args::new().int(blob_id).int(blob_id),
            )
            .await
    }
}
