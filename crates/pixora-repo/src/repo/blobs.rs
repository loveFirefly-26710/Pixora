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

    /// 批量预检：客户端把一批哈希发过来，服务端只回哪些已存在。
    ///
    /// 这是省带宽的关键一步——重复文件连上传请求都不发。哈希数量可能上千，
    /// 按 500 一批切分，避开各家数据库的参数个数上限。
    pub async fn existing_hashes(&self, hashes: &[String]) -> Result<HashCheck> {
        let mut exist: Vec<String> = Vec::new();
        if hashes.is_empty() {
            return Ok(HashCheck::default());
        }

        for chunk in hashes.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT sha256 FROM blobs WHERE kind = 'original' AND sha256 IN ({placeholders})"
            );
            let mut args = Args::new();
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
                "UPDATE blobs SET ref_count = MAX(ref_count - 1, 0) WHERE id = ?",
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

    /// 某个驱动下的实体数与占用。
    pub async fn blob_stats_by_driver(&self) -> Result<Vec<DriverStat>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT storage_driver AS driver, COUNT(*) AS total, COALESCE(SUM(size), 0) AS bytes \
                   FROM blobs GROUP BY storage_driver ORDER BY bytes DESC",
                Args::new(),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(DriverStat {
                    driver: get_str(row, "driver")?,
                    count: get_i64(row, "total")?,
                    bytes: get_i64(row, "bytes")?,
                })
            })
            .collect()
    }

    /// 按 MIME 类型分布。
    pub async fn blob_stats_by_mime(&self, limit: i64) -> Result<Vec<MimeStat>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT mime_type AS mime, COUNT(*) AS total, COALESCE(SUM(size), 0) AS bytes \
                   FROM blobs WHERE kind = 'original' GROUP BY mime_type ORDER BY total DESC LIMIT ?",
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
                "SELECT COALESCE(SUM(size), 0) AS total FROM blobs WHERE storage_driver = ?",
                Args::new().text(driver),
            )
            .await
    }

    /// 实体的字节合计（全部驱动）。
    pub async fn sum_blob_bytes(&self) -> Result<i64> {
        self.db
            .count("SELECT COALESCE(SUM(size), 0) AS total FROM blobs", Args::new())
            .await
    }

    /// 平均文件大小，统计面板用。
    pub async fn average_blob_size(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT CAST(COALESCE(AVG(size), 0) AS BIGINT) AS total FROM blobs WHERE kind = 'original'",
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
}
