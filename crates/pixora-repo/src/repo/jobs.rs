//! 上传任务账本仓储。
//!
//! 每次上传尝试都会在这里留一行：`queued → running → done | skipped | failed`。
//! 用途有三层：
//!   1. 客户端可以凭 job id 轮询单张进度（`GET /api/v1/queue/{job_id}`）；
//!   2. 队列深度作为背压依据（深度超阈值时新请求直接 429）；
//!   3. 进程重启后能看见「卡在 running 的任务」，把超时的重新排队或标记失败。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::{RepoError, Result};
use crate::model::*;
use crate::repo::Repo;

/// 新建任务账本记录。
#[derive(Debug, Clone)]
pub struct NewJob {
    pub owner_id: i64,
    pub album_id: Option<i64>,
    pub sha256: Option<String>,
    pub original_name: String,
    pub size: i64,
}

/// 队列深度的构成。
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueDepth {
    pub queued: i64,
    pub running: i64,
}

impl QueueDepth {
    pub fn total(&self) -> i64 {
        self.queued + self.running
    }
}

impl Repo {
    // ---------- 记录 ----------

    /// 登记一次上传尝试，起始状态 `queued`。
    ///
    /// 账本先于实际落盘写入，客户端因此总能凭 job id 轮询进度。
    pub async fn create_job(&self, new: &NewJob) -> Result<UploadJob> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO upload_jobs (owner_id, album_id, sha256, original_name, size, status, \
                                          dedup, image_id, attempts, error, created_at, started_at, finished_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, NULL, 0, NULL, ?, NULL, NULL)",
                Args::new()
                    .int(new.owner_id)
                    .opt_int(new.album_id)
                    .opt_text(new.sha256.clone())
                    .text(&new.original_name)
                    .int(new.size)
                    .text(job_status::QUEUED)
                    .bool(false)
                    .text(now_ts()),
            )
            .await?;

        self.find_job(id)
            .await?
            .ok_or_else(|| RepoError::NotFound(format!("新建上传任务 {id}")))
    }

    pub async fn find_job(&self, id: i64) -> Result<Option<UploadJob>> {
        let sql = format!("SELECT {} FROM upload_jobs WHERE id = ?", UploadJob::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(UploadJob::from_row).transpose()
    }

    /// 开始处理：记开始时间并把 `attempts` 加一。
    /// 重试也走这里，所以次数直接写在挂账本上，不靠外部计数。
    pub async fn mark_job_running(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE upload_jobs SET status = ?, started_at = ?, attempts = attempts + 1 WHERE id = ?",
                Args::new()
                    .text(job_status::RUNNING)
                    .text(now_ts())
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 落盘成功。`dedup = true` 表示服务器早就有同一份字节、只建了引用，
    /// 因此不算限流窗口内的「新文件」——两种完成态共用同一个状态值，靠 `dedup` 区分。
    pub async fn mark_job_done(
        &self,
        id: i64,
        image_id: i64,
        sha256: &str,
        dedup: bool,
    ) -> Result<()> {
        self.db
            .execute(
                "UPDATE upload_jobs SET status = ?, image_id = ?, sha256 = ?, dedup = ?, \
                                        error = NULL, finished_at = ? WHERE id = ?",
                Args::new()
                    .text(job_status::DONE)
                    .int(image_id)
                    .text(sha256)
                    .bool(dedup)
                    .text(now_ts())
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 处理失败：错误原因落库，供客户端轮询时显示。
    pub async fn mark_job_failed(&self, id: i64, error: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE upload_jobs SET status = ?, error = ?, finished_at = ? WHERE id = ?",
                Args::new()
                    .text(job_status::FAILED)
                    .text(error)
                    .text(now_ts())
                    .int(id),
            )
            .await?;
        Ok(())
    }

    // ---------- 队列视图 ----------

    /// 背压依据：排队中 + 处理中的条数，一条条件聚合查完。
    pub async fn queue_depth(&self) -> Result<QueueDepth> {
        let row = self
            .db
            .fetch_one(
                "SELECT COALESCE(SUM(CASE WHEN status = ? THEN 1 ELSE 0 END), 0) AS queued, \
                        COALESCE(SUM(CASE WHEN status = ? THEN 1 ELSE 0 END), 0) AS running \
                   FROM upload_jobs",
                Args::new()
                    .text(job_status::QUEUED)
                    .text(job_status::RUNNING),
            )
            .await?;
        Ok(QueueDepth {
            queued: get_i64(&row, "queued")?,
            running: get_i64(&row, "running")?,
        })
    }

    /// 限流用：窗口内真正落盘的新文件数。
    ///
    /// 去重命中的任务（`dedup = true`）不占新空间，不算在内；
    /// 完成时间缺失时退回建立时间，保证「登记完就崩」的任务也被计入。
    pub async fn count_new_files_since(&self, owner_id: i64, since: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM upload_jobs \
                  WHERE owner_id = ? AND status = ? AND dedup = ? \
                    AND COALESCE(finished_at, created_at) >= ?",
                Args::new()
                    .int(owner_id)
                    .text(job_status::DONE)
                    .bool(false)
                    .text(ts(since)),
            )
            .await
    }

    /// 单日上传流量：同一个口径（只算新落盘的字节），去重命中不计。
    pub async fn sum_bytes_since(&self, owner_id: i64, since: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COALESCE(SUM(size), 0) AS total FROM upload_jobs \
                  WHERE owner_id = ? AND status = ? AND dedup = ? \
                    AND COALESCE(finished_at, created_at) >= ?",
                Args::new()
                    .int(owner_id)
                    .text(job_status::DONE)
                    .bool(false)
                    .text(ts(since)),
            )
            .await
    }

    pub async fn list_jobs(&self, owner_id: i64, limit: i64) -> Result<Vec<UploadJob>> {
        let sql = format!(
            "SELECT {} FROM upload_jobs WHERE owner_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
            UploadJob::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(owner_id).int(limit.clamp(1, 500)))
            .await?;
        rows.iter().map(UploadJob::from_row).collect()
    }

    /// 卡在 `running` 的任务。`started_at` 缺失的行按 `created_at` 计龄——
    /// 崩在「标记 running」之前不等于还活着。
    pub async fn list_stale_running(
        &self,
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<UploadJob>> {
        let sql = format!(
            "SELECT {} FROM upload_jobs WHERE status = ? AND COALESCE(started_at, created_at) < ? \
             ORDER BY created_at ASC LIMIT ?",
            UploadJob::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(
                &sql,
                Args::new()
                    .text(job_status::RUNNING)
                    .text(ts(older_than))
                    .int(limit.clamp(1, 500)),
            )
            .await?;
        rows.iter().map(UploadJob::from_row).collect()
    }

    /// 回收卡住的任务：退回 `queued` 并清掉 `started_at`，让它们重新被拾取。
    /// `attempts` 不重置，重试次数累计在账上，超过阈值的由服务层判失败。
    pub async fn requeue_stale(&self, older_than: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "UPDATE upload_jobs SET status = ?, started_at = NULL \
                  WHERE status = ? AND COALESCE(started_at, created_at) < ?",
                Args::new()
                    .text(job_status::QUEUED)
                    .text(job_status::RUNNING)
                    .text(ts(older_than)),
            )
            .await
    }

    /// 清理终态账本：只删已经结束（done / skipped / failed）且够老的行，
    /// 排队与处理中的一条都不能碰。
    pub async fn purge_jobs_before(&self, before: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM upload_jobs WHERE status IN (?, ?, ?) \
                    AND COALESCE(finished_at, created_at) < ?",
                Args::new()
                    .text(job_status::DONE)
                    .text(job_status::SKIPPED)
                    .text(job_status::FAILED)
                    .text(ts(before)),
            )
            .await
    }
}
