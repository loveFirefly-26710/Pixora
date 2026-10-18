//! 物理文件实体的仓储。去重与引用计数的落点就在这个文件里。
//!
//! 三条规则贯穿全库：
//!   1. `blobs` 里一行对应磁盘上一份字节，`(sha256, kind)` 唯一。
//!   2. `ref_count` 只反映「有多少个 images 行引用它」，加一减一必须成对。
//!   3. 只有 `ref_count = 0` 的 blob 才允许被 GC 删物理文件。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::model::*;
use crate::repo::Repo;

/// 新建 blob 的入参。存储位置由 driver 决定。
#[derive(Debug, Clone)]
pub struct NewBlob {
    pub sha256: String,
    pub kind: String,
    pub size: i64,
    pub mime_type: String,
    pub ext: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub storage_driver: String,
    pub storage_key: String,
    pub backend_id: Option<i64>,
}

/// 一次批量预检的结果。
#[derive(Debug, Clone, Default)]
pub struct HashCheck {
    pub exist: Vec<String>,
    pub missing: Vec<String>,
}

impl Repo {
    /// 按内容哈希找实体。`kind` 区分原图与缩略图。
    pub async fn find_blob_by_sha(&self, sha256: &str, kind: &str) -> Result<Option<Blob>> {
        let sql = format!(
            "SELECT {} FROM blobs WHERE sha256 = ? AND kind = ?",
            Blob::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(sha256).text(kind))
            .await?;
        row.as_ref().map(Blob::from_row).transpose()
    }

    pub async fn find_blob_by_id(&self, id: i64) -> Result<Option<Blob>> {
        let sql = format!("SELECT {} FROM blobs WHERE id = ?", Blob::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Blob::from_row).transpose()
    }

    /// 这个 blob 是否被某个用户的图片引用。相册封面只允许从自己的图里挑，
    /// 不能拿任意 blob id 去探测别处是否存在这张图。
    pub async fn owner_has_blob(&self, owner_id: i64, blob_id: i64) -> Result<bool> {
        self.db
            .exists(
                "SELECT 1 FROM images WHERE owner_id = ? AND blob_id = ? AND deleted_at IS NULL",
                Args::new().int(owner_id).int(blob_id),
            )
            .await
    }

    /// 按原图哈希找这张图片的缩略图实体。
    ///
    /// 直链的形状是「原图哈希 + `?thumb=1`」：URL 里那个哈希标识的是这张图片，
    /// 而缩略图是另算过哈希的一份实体，所以拿同一个哈希去 `kind = 'thumb'` 里查
    /// 永远查不到——缩略图得顺着图片行找。
    ///
    /// 同一份原图可能被多人引用（去重），各自的缩略图编码参数可能不同，
    /// 取一张就行。优先取还活着的引用：回收站里的图没必要再拿它的缩略图。
    pub async fn find_thumb_blob_for_original(&self, sha256: &str) -> Result<Option<Blob>> {
        let sql = format!(
            "SELECT {} FROM blobs WHERE id = ( \
                 SELECT i.thumb_blob_id FROM images i \
                   JOIN blobs o ON o.id = i.blob_id \
                  WHERE o.sha256 = ? AND o.kind = ? AND i.thumb_blob_id IS NOT NULL \
                  ORDER BY CASE WHEN i.deleted_at IS NULL THEN 0 ELSE 1 END \
                  LIMIT 1)",
            Blob::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(
                &sql,
                Args::new()
                    .text(sha256)
                    .text(storage_kind::ORIGINAL),
            )
            .await?;
        row.as_ref().map(Blob::from_row).transpose()
    }

    /// 批量预检：客户端把一批哈希发过来，服务端只回哪些已存在。
    ///
    /// 这是省带宽的关键一步——重复文件连上传请求都不发。哈希数量可能上千，
    /// 按 500 一批切分，避开各家数据库的参数个数上限。
    /// 预检：这些哈希里，哪些**在调用者的图库里已经有一张活着的图**。
    ///
    /// 判定口径是「有没有活着的引用」，不是「blobs 表里还有没有这份文件」。
    /// 物理文件在最后一张引用删掉后还会留一阵（等 GC 回收），拿它当依据的话，
    /// 用户把图全删了再传同一批会被判成「已存在」而整批跳过——图没进来，
    /// 看起来就像删除没删干净。
    pub async fn existing_hashes(&self, owner_id: i64, hashes: &[String]) -> Result<HashCheck> {
        let mut exist: Vec<String> = Vec::new();
        if hashes.is_empty() {
            return Ok(HashCheck::default());
        }

        for chunk in hashes.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT DISTINCT b.sha256 AS sha256 FROM blobs b                  JOIN images i ON i.blob_id = b.id                  WHERE b.kind = 'original' AND i.deleted_at IS NULL AND i.owner_id = ?                  AND b.sha256 IN ({placeholders})"
            );
            let mut args = Args::new().int(owner_id);
            for hash in chunk {
                args = args.text(hash);
            }
            let rows = self.db.fetch_all(&sql, args).await?;
            for row in rows {
                exist.push(get_str(&row, "sha256")?);
            }
        }

        let exist_set: std::collections::HashSet<&str> =
            exist.iter().map(|value| value.as_str()).collect();
        let missing = hashes
            .iter()
            .filter(|hash| !exist_set.contains(hash.as_str()))
            .cloned()
            .collect();

        Ok(HashCheck { exist, missing })
    }

    /// 登记一份新实体。
    ///
    /// 并发上传同一张图时两个请求会同时走到这里，所以插入用幂等写法：
    /// 命中唯一键就什么都不做，随后把已有行读出来返回。调用方拿到的一定是同一行。
    pub async fn insert_blob_if_absent(&self, new: &NewBlob) -> Result<Blob> {
        let insert = self.db.sql_v(
            "INSERT INTO blobs (sha256, kind, size, mime_type, ext, width, height, \
                                storage_driver, storage_key, backend_id, ref_count, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) \
             ON CONFLICT (sha256, kind) DO NOTHING",
            "INSERT IGNORE INTO blobs (sha256, kind, size, mime_type, ext, width, height, \
                                storage_driver, storage_key, backend_id, ref_count, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)",
            "INSERT INTO blobs (sha256, kind, size, mime_type, ext, width, height, \
                                storage_driver, storage_key, backend_id, ref_count, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?) \
             ON CONFLICT (sha256, kind) DO NOTHING",
        );
        let args = Args::new()
            .text(&new.sha256)
            .text(&new.kind)
            .int(new.size)
            .text(&new.mime_type)
            .text(&new.ext)
            .opt_int(new.width.map(|value| value as i64))
            .opt_int(new.height.map(|value| value as i64))
            .text(&new.storage_driver)
            .text(&new.storage_key)
            .opt_int(new.backend_id)
            .text(now_ts());
        self.db.execute(&insert, args).await?;

        self.find_blob_by_sha(&new.sha256, &new.kind)
            .await?
            .ok_or_else(|| crate::RepoError::NotFound(format!("登记 blob {}", new.sha256)))
    }

    /// 引用计数 +1。建立图片引用时调用。
    pub async fn inc_blob_ref(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE blobs SET ref_count = ref_count + 1 WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 引用计数 -1。删除图片引用时调用，不允许减成负数。
    pub async fn dec_blob_ref(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                &self.db.sql_v(
                    "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
                    "UPDATE blobs SET ref_count = GREATEST(ref_count - 1, 0) WHERE id = ?",
                    "UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?",
                ),
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// GC 候选：没有引用、且登记时间早于宽限期。
    ///
    /// 宽限期是为了避开「先登记实体、再建引用」之间的时间窗——
    /// 那个窗口里 ref_count 天然是 0，直接删会误伤正在上传的文件。
    pub async fn list_gc_candidates(
        &self,
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Blob>> {
        let sql = format!(
            "SELECT {} FROM blobs WHERE ref_count = 0 AND created_at < ? \
             ORDER BY created_at ASC LIMIT ?",
            Blob::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(
                &sql,
                Args::new().text(ts(older_than)).int(limit.clamp(1, 1000)),
            )
            .await?;
        rows.iter().map(Blob::from_row).collect()
    }

    /// 数一下「引用已归零、但还没到宽限期」的实体。
    ///
    /// 回收任务拿它决定下一轮什么时候再看：刚失去引用的文件在宽限期到点后就该被收走，
    /// 不该再等一个完整的扫描周期。
    pub async fn count_unreferenced_newer_than(&self, newer_than: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM blobs WHERE ref_count = 0 AND created_at >= ?",
                Args::new().text(ts(newer_than)),
            )
            .await
    }

    /// 删除实体行。调用前必须确认 `ref_count = 0`，物理文件由调用方先删。
    pub async fn delete_blob(&self, id: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM blobs WHERE id = ?", Args::new().int(id))
            .await?;
        Ok(())
    }

    /// 只删引用计数已归零的行，作为 GC 的最后一道保险。
    pub async fn delete_blob_if_unreferenced(&self, id: i64) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM blobs WHERE id = ? AND ref_count = 0",
                Args::new().int(id),
            )
            .await
    }

    /// 迁移或回退存储位置时更新物理位置，不动引用计数。
    pub async fn update_blob_location(
        &self,
        id: i64,
        storage_driver: &str,
        storage_key: &str,
        backend_id: Option<i64>,
    ) -> Result<()> {
        self.db
            .execute(
                "UPDATE blobs SET storage_driver = ?, storage_key = ?, backend_id = ? WHERE id = ?",
                Args::new()
                    .text(storage_driver)
                    .text(storage_key)
                    .opt_int(backend_id)
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 每个后端上的「在用」内容。
    ///
    /// 两个数字的口径都改过一次，原因值得写下来：
    ///
    ///   - **张数是图片数，不是实体条数**。一张图通常配一个缩略图，按实体数
    ///     一张图会显示成「2 张」，用户会以为传了两遍。
    ///   - **字节是这个后端上仍被引用的实体**（原图 + 缩略图，每个实体只算一次）。
    ///     去重命中的共享实体只算一份，这才是磁盘上的真实占用。
    ///
    /// 已经被彻底删除、只剩实体行等后台回收的，不在这里出现（见
    /// [`Repo::unreferenced_usage`]）——那些东西在界面上没有任何对应物，
    /// 混进分布只会让人怀疑「删了怎么还在」。
    pub async fn blob_stats_by_driver(&self) -> Result<Vec<DriverStat>> {
        let counts = self.image_counts_by_driver(false).await?;
        let bytes = self.entity_bytes_by_driver(false).await?;
        let mut stats: Vec<DriverStat> = Vec::new();
        for (driver, count) in counts {
            stats.push(DriverStat {
                driver,
                count,
                bytes: 0,
            });
        }
        for (driver, value) in bytes {
            match stats.iter_mut().find(|item| item.driver == driver) {
                Some(item) => item.bytes = value,
                None => stats.push(DriverStat {
                    driver,
                    count: 0,
                    bytes: value,
                }),
            }
        }
        stats.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.driver.cmp(&b.driver)));
        Ok(stats)
    }

    /// 回收站里还没过保留期的内容：(张数, 被引用的实体字节)。
    pub async fn binned_usage(&self) -> Result<(i64, i64)> {
        let counts = self.image_counts_by_driver(true).await?;
        let bytes = self.entity_bytes_by_driver(true).await?;
        Ok((
            counts.iter().map(|(_, count)| *count).sum(),
            bytes.iter().map(|(_, value)| *value).sum(),
        ))
    }

    /// 已经没有任何图片引用的实体：(条数, 字节)。
    ///
    /// 彻底删除之后、后台回收之前，实体会短暂停在这个状态。界面上把它单独列一行，
    /// 附一个「立即回收」入口，比让这些字节混进正常占用要好解释。
    pub async fn unreferenced_usage(&self) -> Result<(i64, i64)> {
        let row = self
            .db
            .fetch_optional(
                &format!(
                    "SELECT COUNT(*) AS total, {} AS bytes \
                       FROM blobs b \
                      WHERE NOT EXISTS ( \
                            SELECT 1 FROM images i WHERE i.blob_id = b.id OR i.thumb_blob_id = b.id)",
                    self.db.dialect().sum_i64("b.size")
                ),
                Args::new(),
            )
            .await?;
        let Some(row) = row else {
            return Ok((0, 0));
        };
        Ok((get_i64(&row, "total")?, get_i64(&row, "bytes")?))
    }

    /// 图片张数按「原图所在后端」分组。`binned` 选择活着的还是回收站里的。
    async fn image_counts_by_driver(&self, binned: bool) -> Result<Vec<(String, i64)>> {
        let condition = if binned {
            "i.deleted_at IS NOT NULL"
        } else {
            "i.deleted_at IS NULL"
        };
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT b.storage_driver AS driver, COUNT(*) AS total \
                       FROM images i JOIN blobs b ON b.id = i.blob_id \
                      WHERE {condition} GROUP BY b.storage_driver"
                ),
                Args::new(),
            )
            .await?;
        rows.iter()
            .map(|row| Ok((get_str(row, "driver")?, get_i64(row, "total")?)))
            .collect()
    }

    /// 被图片引用到的实体字节，按后端分组，每个实体只算一次。
    ///
    /// 原图与缩略图各取一次再 UNION 去重：同一份实体被多张图引用时只算一份，
    /// 这正是「去重省下多少」的另一面。
    async fn entity_bytes_by_driver(&self, binned: bool) -> Result<Vec<(String, i64)>> {
        let condition = if binned {
            "i.deleted_at IS NOT NULL"
        } else {
            "i.deleted_at IS NULL"
        };
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT driver, {} AS bytes FROM ( \
                        SELECT DISTINCT b.id AS id, b.storage_driver AS driver, b.size AS size \
                          FROM images i JOIN blobs b ON b.id = i.blob_id WHERE {condition} \
                        UNION \
                        SELECT DISTINCT b.id, b.storage_driver, b.size \
                          FROM images i JOIN blobs b ON b.id = i.thumb_blob_id \
                         WHERE {condition} AND i.thumb_blob_id IS NOT NULL \
                     ) AS refs GROUP BY driver",
                    self.db.dialect().sum_i64("size")
                ),
                Args::new(),
            )
            .await?;
        rows.iter()
            .map(|row| Ok((get_str(row, "driver")?, get_i64(row, "bytes")?)))
            .collect()
    }

    /// 逻辑占用：每张图各自引用的原图大小之和（去重前该占多少）。
    pub async fn logical_bytes_all(&self) -> Result<i64> {
        self.db
            .count(
                &format!(
                    "SELECT {} AS total FROM images i JOIN blobs b ON b.id = i.blob_id",
                    self.db.dialect().sum_i64("b.size")
                ),
                Args::new(),
            )
            .await
    }

    /// 仍在被引用的实体字节合计（不含等回收的孤儿）。
    pub async fn referenced_bytes_all(&self) -> Result<i64> {
        let live = self
            .entity_bytes_by_driver(false)
            .await?
            .iter()
            .map(|(_, value)| *value)
            .sum::<i64>();
        let binned = self
            .entity_bytes_by_driver(true)
            .await?
            .iter()
            .map(|(_, value)| *value)
            .sum::<i64>();
        Ok(live + binned)
    }

    /// 按 MIME 类型分布。
    pub async fn blob_stats_by_mime(&self, limit: i64) -> Result<Vec<MimeStat>> {
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT mime_type AS mime, COUNT(*) AS total, {} AS bytes \
                       FROM blobs WHERE kind = 'original' GROUP BY mime_type ORDER BY total DESC LIMIT ?",
                    self.db.dialect().sum_i64("size")
                ),
                Args::new().int(limit.clamp(1, 100)),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(MimeStat {
                    mime: get_str(row, "mime")?,
                    count: get_i64(row, "total")?,
                    bytes: get_i64(row, "bytes")?,
                })
            })
            .collect()
    }

    /// 某驱动下所有实体的字节合计——容量展示的「已用」按这个口径算，
    /// 不依赖文件系统的 statfs。
    pub async fn sum_blob_bytes_by_driver(&self, driver: &str) -> Result<i64> {
        self.db
            .count(
                &format!(
                    "SELECT {} AS total FROM blobs WHERE storage_driver = ?",
                    self.db.dialect().sum_i64("size")
                ),
                Args::new().text(driver),
            )
            .await
    }

    /// 实体的字节合计（全部驱动）。
    pub async fn sum_blob_bytes(&self) -> Result<i64> {
        self.db
            .count(
                &format!("SELECT {} AS total FROM blobs", self.db.dialect().sum_i64("size")),
                Args::new(),
            )
            .await
    }

    /// 平均文件大小，统计面板用。
    pub async fn average_blob_size(&self) -> Result<i64> {
        self.db
            .count(
                &self.db.sql_v(
                    "SELECT CAST(COALESCE(AVG(size), 0) AS BIGINT) AS total \
                       FROM blobs WHERE kind = 'original'",
                    "SELECT CAST(COALESCE(AVG(size), 0) AS SIGNED) AS total \
                       FROM blobs WHERE kind = 'original'",
                    "SELECT CAST(COALESCE(AVG(size), 0) AS INTEGER) AS total \
                       FROM blobs WHERE kind = 'original'",
                ),
                Args::new(),
            )
            .await
    }

    pub async fn count_blobs(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM blobs WHERE kind = 'original'",
                Args::new(),
            )
            .await
    }

    /// 按实际引用重算 `ref_count`，返回被修正的行数。
    ///
    /// 为什么需要它：引用计数靠服务层加减，但有些路径绕开了服务层——
    /// 删除账号时 `images` 随外键级联消失（计数没人减）、有人在库里手工删了行、
    /// 或者事务写到一半进程没了。这些情况下 `ref_count` 会虚高，物理文件于是
    /// 永远等不到归零、也永远不被 GC 回收，磁盘就这么慢慢漏。
    ///
    /// 这条语句把计数拉回「实际引用」的事实：原图引用 + 缩略图引用。
    /// 逐行执行的是同一条 SQL，代价是一次全表扫描，放在每小时的对账里跑。
    pub async fn recompute_blob_ref_counts(&self) -> Result<u64> {
        let actual = "((SELECT COUNT(*) FROM images i WHERE i.blob_id = blobs.id AND i.deleted_at IS NULL)                        + (SELECT COUNT(*) FROM images i WHERE i.thumb_blob_id = blobs.id AND i.deleted_at IS NULL))";
        let sql = format!(
            "UPDATE blobs SET ref_count = {actual} WHERE ref_count <> {actual}"
        );
        self.db.execute(&sql, Args::new()).await
    }
}
