//! 统计聚合仓储：把管理后台统计面板要的数字一次算齐。
//!
//! 这里只做「读」的聚合，不改变任何状态。趋势与分时统计都靠 `substr` 从
//! RFC3339 文本里截日期与小时（`Dialect::date_expr` / `hour_expr`），
//! 三种方言写法一致，不需要各自的日期函数。

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};

use crate::args::Args;
use crate::db::Db;
use crate::error::Result;
use crate::model::*;
use crate::repo::{window_start, Repo};

impl Repo {
    /// 存储面板：占用、去重收益、分布、趋势与最近上传一次组装齐。
    ///
    /// `total_bytes` 与 `drivers` / `mimes` 都复用 `blobs.rs` 里的口径，
    /// 面板和实体页因此不会给出两个不同的数字。
    pub async fn storage_stats(&self, recent_limit: i64) -> Result<StorageStats> {
        let image_count = self.count_all_images().await?;
        let total_bytes = self.sum_blob_bytes().await?;
        // 逻辑占用按「未删图片各自引用的原图大小」算：去重命中时同一份 blob
        // 会被多行引用，这正好是「没有去重时该占多少」
        let logical_bytes = self
            .db
            .count(
                "SELECT COALESCE(SUM(b.size), 0) AS total FROM images i \
                   JOIN blobs b ON b.id = i.blob_id WHERE i.deleted_at IS NULL",
                Args::new(),
            )
            .await?;

        Ok(StorageStats {
            image_count,
            total_bytes,
            logical_bytes,
            dedup_saved_bytes: (logical_bytes - total_bytes).max(0),
            average_size: self.average_blob_size().await?,
            drivers: self.blob_stats_by_driver().await?,
            mimes: self.blob_stats_by_mime(8).await?,
            trend: self.upload_trend(30).await?,
            hourly: self.upload_hourly_today().await?,
            recent: self.recent_images(recent_limit).await?,
        })
    }

    /// 近 N 天每天的上传张数，`bucket` 形如 `2026-10-14`。
    ///
    /// 空桶不补：补零是展示逻辑，放到服务层做，仓储只回有数据的桶。
    /// 分桶表达式取自 `Dialect::date_expr`，与 MySQL、PostgreSQL 共用一份写法。
    pub async fn upload_trend(&self, days: i64) -> Result<Vec<TrendPoint>> {
        let bucket = self.db.dialect().date_expr("upload_time");
        let sql = format!(
            "SELECT {bucket} AS bucket, COUNT(*) AS total FROM images \
              WHERE deleted_at IS NULL AND upload_time >= ? \
              GROUP BY {bucket} ORDER BY bucket ASC"
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().text(ts(window_start(days))))
            .await?;
        rows.iter().map(trend_point_from).collect()
    }

    /// 今日分时，`bucket` 形如 `2026-10-14T09`。
    ///
    /// 「今天」用日期前缀比对（与分桶同一口径），省掉三种方言各自算当天 0 点的麻烦。
    pub async fn upload_hourly_today(&self) -> Result<Vec<TrendPoint>> {
        let day = self.db.dialect().date_expr("upload_time");
        let hour = self.db.dialect().hour_expr("upload_time");
        let sql = format!(
            "SELECT {hour} AS bucket, COUNT(*) AS total FROM images \
              WHERE deleted_at IS NULL AND {day} = ? \
              GROUP BY {hour} ORDER BY bucket ASC"
        );
        let stamp = now_ts();
        let rows = self
            .db
            .fetch_all(&sql, Args::new().text(&stamp[..10]))
            .await?;
        rows.iter().map(trend_point_from).collect()
    }

    /// 管理端首页的数字。`register_open` 是配置项，留给服务层填，这里给 `true`。
    pub async fn admin_stats(&self) -> Result<AdminStats> {
        let month_start = current_month_start();
        Ok(AdminStats {
            user_total: self.count_users().await?,
            user_new_this_month: self.count_users_created_since(month_start).await?,
            allocated_quota: self.sum_allocated_quota(None).await?,
            used_bytes: self.sum_used_bytes().await?,
            image_total: self.count_all_images().await?,
            storage_bytes: self.sum_blob_bytes().await?,
            register_open: true,
        })
    }

    /// 某个用户近 24 小时命中限制的次数（后台用户表里那一列）。
    pub async fn count_rate_hits_24h(&self, user_id: i64) -> Result<i64> {
        let hits = rate_hits_for_users(self.db(), &[user_id], window_start(1)).await?;
        Ok(hits.get(&user_id).copied().unwrap_or(0))
    }
}

/// 一批用户在窗口内命中限制的次数，口径是 `audit_logs` 里 `rate_limited` 的次数。
///
/// 做成「一批」是因为管理页按页取用户：逐行回查会变成 N+1，而单用户查询
/// （[`Repo::count_rate_hits_24h`]）只是这一批里只有一个元素的情形。
pub(crate) async fn rate_hits_for_users(
    db: &Db,
    user_ids: &[i64],
    since: DateTime<Utc>,
) -> Result<HashMap<i64, i64>> {
    let mut hits = HashMap::new();
    if user_ids.is_empty() {
        return Ok(hits);
    }

    let placeholders = vec!["?"; user_ids.len()].join(", ");
    let mut args = Args::new().text(ts(since));
    for id in user_ids {
        args = args.int(*id);
    }
    let rows = db
        .fetch_all(
            &format!(
                "SELECT actor_id, COUNT(*) AS total FROM audit_logs \
                  WHERE action = 'rate_limited' AND created_at >= ? \
                    AND actor_id IN ({placeholders}) \
                  GROUP BY actor_id"
            ),
            args,
        )
        .await?;
    for row in rows {
        if let Some(actor_id) = get_opt_i64(&row, "actor_id")? {
            hits.insert(actor_id, get_i64(&row, "total")?);
        }
    }
    Ok(hits)
}

/// 本月 1 号 0 点（UTC）。注册统计的窗口按自然月算，运维才复现得出同一个数。
fn current_month_start() -> DateTime<Utc> {
    Utc::now()
        .date_naive()
        .with_day(1)
        .expect("每个月的 1 号都存在")
        .and_time(chrono::NaiveTime::MIN)
        .and_utc()
}

fn trend_point_from(row: &sqlx::any::AnyRow) -> Result<TrendPoint> {
    Ok(TrendPoint {
        bucket: get_str(row, "bucket")?,
        count: get_i64(row, "total")?,
    })
}
