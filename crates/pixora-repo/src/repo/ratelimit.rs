//! 频率限制仓储：策略（全局默认 + 用户覆盖）与滑动窗口计数。
//!
//! 策略表 `user_id` 为空的那一行是全局默认，唯一索引保证只有一行。
//! 计数表用于「天」级窗口和多实例共享——单实例的分钟级窗口走进程内令牌桶，
//! 够快也够省内存；一旦有第二个实例，计数必须落库，否则额度会被放大成实例数的倍数。

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::db::{Db, Dialect};
use crate::error::Result;
use crate::model::{get_i64, now_ts, ts, RateLimitPolicy, User, UserSummary};
use crate::repo::stats::rate_hits_for_users;
use crate::repo::users::UserFilter;
use crate::repo::{like_pattern, window_start, Page, PageRequest, Repo};

/// 计数窗口的类型。
pub mod kind {
    pub const API_MINUTE: &str = "api_minute";
    pub const API_HOUR: &str = "api_hour";
    pub const API_READ_MINUTE: &str = "api_read_minute";
    pub const UPLOAD_MINUTE: &str = "upload_minute";
    pub const UPLOAD_HOUR: &str = "upload_hour";
    pub const UPLOAD_DAY: &str = "upload_day";
    pub const UPLOAD_BYTES_DAY: &str = "upload_bytes_day";
}

/// 计数主体：用户、IP 或令牌哈希。
pub mod scope {
    pub const USER: &str = "user";
    pub const IP: &str = "ip";
    pub const TOKEN: &str = "token";
}

/// 用户策略列表行：用户信息 + 生效中的限额。
#[derive(Debug, Clone)]
pub struct UserPolicyRow {
    pub user: UserSummary,
    /// 用户级覆盖，`None` 表示继承全局默认
    pub policy: Option<RateLimitPolicy>,
    /// 近 24 小时命中限制的次数
    pub hits_24h: i64,
}

impl Repo {
    // ---------- 策略 ----------

    /// 全局默认策略。没配过就返回内置默认值——调用方不该为「还没配」写分支。
    pub async fn global_policy(&self) -> Result<RateLimitPolicy> {
        let sql = format!(
            "SELECT {} FROM rate_limit_policies WHERE user_id IS NULL",
            RateLimitPolicy::COLUMNS
        );
        match self.db.fetch_optional(&sql, Args::new()).await? {
            Some(row) => RateLimitPolicy::from_row(&row),
            None => Ok(RateLimitPolicy::defaults()),
        }
    }

    pub async fn user_policy(&self, user_id: i64) -> Result<Option<RateLimitPolicy>> {
        let sql = format!(
            "SELECT {} FROM rate_limit_policies WHERE user_id = ?",
            RateLimitPolicy::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(user_id))
            .await?;
        row.as_ref().map(RateLimitPolicy::from_row).transpose()
    }

    /// 某个用户实际生效的限额：用户覆盖优先，其次全局默认。
    ///
    /// 管理员在这里被豁免（`unlimited`）：后台自己的迁移、回填、批量导出
    /// 不该被上传配额卡住。数值字段仍然按回退链给出，调用方只看这一个布尔位。
    pub async fn effective_policy(&self, user: &User) -> Result<RateLimitPolicy> {
        let policy = match self.user_policy(user.id).await? {
            Some(policy) => policy,
            None => self.global_policy().await?,
        };
        if user.is_admin() {
            return Ok(RateLimitPolicy {
                unlimited: true,
                ..policy
            });
        }
        Ok(policy)
    }

    /// 写全局默认。
    ///
    /// 全局行的唯一性建在表达式索引（`user_id IS NULL`）上，冲突目标得跟着
    /// 把索引谓词写出来；MySQL 侧靠生成列 + 唯一键，所以是 `ON DUPLICATE KEY`。
    pub async fn upsert_global_policy(&self, policy: &RateLimitPolicy) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT ((user_id IS NULL)) WHERE user_id IS NULL DO UPDATE SET \
                 api_per_minute = ?, api_per_hour = ?, api_read_per_minute = ?, api_burst = ?, \
                 upload_per_minute = ?, upload_per_hour = ?, upload_per_day = ?, \
                 upload_bytes_per_day = ?, max_concurrent_uploads = ?, max_batch_size = ?, \
                 max_file_size = ?, max_pixels = ?, max_width = ?, max_height = ?, formats = ?, download_per_minute = ?, download_per_day = ?, unlimited = ?, note = ?, updated_at = ?",
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE api_per_minute = VALUES(api_per_minute), \
                 api_per_hour = VALUES(api_per_hour), \
                 api_read_per_minute = VALUES(api_read_per_minute), api_burst = VALUES(api_burst), \
                 upload_per_minute = VALUES(upload_per_minute), \
                 upload_per_hour = VALUES(upload_per_hour), \
                 upload_per_day = VALUES(upload_per_day), \
                 upload_bytes_per_day = VALUES(upload_bytes_per_day), \
                 max_concurrent_uploads = VALUES(max_concurrent_uploads), \
                 max_batch_size = VALUES(max_batch_size), \
                 max_file_size = VALUES(max_file_size), max_pixels = VALUES(max_pixels), \
                 max_width = VALUES(max_width), max_height = VALUES(max_height), \
                 download_per_minute = VALUES(download_per_minute), \
                 download_per_day = VALUES(download_per_day), \
                 unlimited = VALUES(unlimited), \
                 note = VALUES(note), updated_at = VALUES(updated_at)",
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (NULL, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT ((user_id IS NULL)) WHERE user_id IS NULL DO UPDATE SET \
                 api_per_minute = ?, api_per_hour = ?, api_read_per_minute = ?, api_burst = ?, \
                 upload_per_minute = ?, upload_per_hour = ?, upload_per_day = ?, \
                 upload_bytes_per_day = ?, max_concurrent_uploads = ?, max_batch_size = ?, \
                 max_file_size = ?, max_pixels = ?, max_width = ?, max_height = ?, formats = ?, download_per_minute = ?, download_per_day = ?, unlimited = ?, note = ?, updated_at = ?",
        );
        let stamp = now_ts();
        let args = rate_policy_args(
            Args::new(),
            policy,
            &stamp,
            upsert_passes(self.db.dialect()),
        );
        self.db.execute(&sql, args).await?;
        Ok(())
    }

    /// 写用户级覆盖。唯一索引锁住 `(user_id)`，所以重复调用只会更新同一行。
    pub async fn upsert_user_policy(&self, user_id: i64, policy: &RateLimitPolicy) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id) WHERE user_id IS NOT NULL DO UPDATE SET \
                 api_per_minute = ?, api_per_hour = ?, api_read_per_minute = ?, api_burst = ?, \
                 upload_per_minute = ?, upload_per_hour = ?, upload_per_day = ?, \
                 upload_bytes_per_day = ?, max_concurrent_uploads = ?, max_batch_size = ?, \
                 max_file_size = ?, max_pixels = ?, max_width = ?, max_height = ?, formats = ?, download_per_minute = ?, download_per_day = ?, unlimited = ?, note = ?, updated_at = ?",
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE api_per_minute = VALUES(api_per_minute), \
                 api_per_hour = VALUES(api_per_hour), \
                 api_read_per_minute = VALUES(api_read_per_minute), api_burst = VALUES(api_burst), \
                 upload_per_minute = VALUES(upload_per_minute), \
                 upload_per_hour = VALUES(upload_per_hour), \
                 upload_per_day = VALUES(upload_per_day), \
                 upload_bytes_per_day = VALUES(upload_bytes_per_day), \
                 max_concurrent_uploads = VALUES(max_concurrent_uploads), \
                 max_batch_size = VALUES(max_batch_size), \
                 max_file_size = VALUES(max_file_size), max_pixels = VALUES(max_pixels), \
                 max_width = VALUES(max_width), max_height = VALUES(max_height), \
                 download_per_minute = VALUES(download_per_minute), \
                 download_per_day = VALUES(download_per_day), \
                 unlimited = VALUES(unlimited), \
                 note = VALUES(note), updated_at = VALUES(updated_at)",
            "INSERT INTO rate_limit_policies (user_id, api_per_minute, api_per_hour, \
                 api_read_per_minute, api_burst, upload_per_minute, upload_per_hour, upload_per_day, \
                 upload_bytes_per_day, max_concurrent_uploads, max_batch_size, \
                 max_file_size, max_pixels, max_width, max_height, formats, download_per_minute, download_per_day, unlimited, note, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id) WHERE user_id IS NOT NULL DO UPDATE SET \
                 api_per_minute = ?, api_per_hour = ?, api_read_per_minute = ?, api_burst = ?, \
                 upload_per_minute = ?, upload_per_hour = ?, upload_per_day = ?, \
                 upload_bytes_per_day = ?, max_concurrent_uploads = ?, max_batch_size = ?, \
                 max_file_size = ?, max_pixels = ?, max_width = ?, max_height = ?, formats = ?, download_per_minute = ?, download_per_day = ?, unlimited = ?, note = ?, updated_at = ?",
        );
        let stamp = now_ts();
        let args = rate_policy_args(
            Args::new().int(user_id),
            policy,
            &stamp,
            upsert_passes(self.db.dialect()),
        );
        self.db.execute(&sql, args).await?;
        Ok(())
    }

    /// 恢复默认：删掉覆盖行，这个用户就重新跟着全局默认走。
    pub async fn delete_user_policy(&self, user_id: i64) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM rate_limit_policies WHERE user_id = ?",
                Args::new().int(user_id),
            )
            .await?;
        Ok(())
    }

    /// 管理页「用户级覆盖」表：每个用户一行，带是否有覆盖与近 24 小时命中次数。
    pub async fn list_user_policies(
        &self,
        filter: &UserFilter,
        page: PageRequest,
    ) -> Result<Page<UserPolicyRow>> {
        let mut filters = WhereBuilder::new();
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = like_pattern(query);
            filters = filters.and(
                "(u.email LIKE ? ESCAPE '\\' OR u.uid LIKE ? ESCAPE '\\')",
                |args| args.text(pattern.clone()).text(pattern),
            );
        }
        if let Some(role) = filter.role.as_deref().filter(|value| !value.is_empty()) {
            filters = filters.and("u.role = ?", |args| args.text(role));
        }
        if let Some(status) = filter.status.as_deref().filter(|value| !value.is_empty()) {
            filters = filters.and("u.status = ?", |args| args.text(status));
        }

        let where_sql = filters.sql_string();
        let args = filters.args();

        let total = self
            .db
            .count(
                &format!("SELECT COUNT(*) AS total FROM users u{where_sql}"),
                args.clone(),
            )
            .await?;

        let rows = self
            .db
            .fetch_all(
                &format!("{USER_POLICY_SQL}{where_sql} ORDER BY u.id ASC LIMIT ? OFFSET ?"),
                args.int(page.per_page).int(page.offset()),
            )
            .await?;
        let users: Vec<UserSummary> = rows
            .iter()
            .map(UserSummary::from_row)
            .collect::<Result<_>>()?;
        if users.is_empty() {
            return Ok(Page::new(Vec::new(), total, page));
        }

        // 覆盖策略与命中次数各批量查一次：逐行回查会变成 2N 次查询，翻页越深越慢
        let ids: Vec<i64> = users.iter().map(|user| user.id).collect();
        let mut policies = policies_for_users(self.db(), &ids).await?;
        let mut hits = rate_hits_for_users(self.db(), &ids, window_start(1)).await?;

        let items = users
            .into_iter()
            .map(|user| {
                let policy = policies.remove(&user.id);
                let hits_24h = hits.remove(&user.id).unwrap_or(0);
                UserPolicyRow {
                    user,
                    policy,
                    hits_24h,
                }
            })
            .collect();

        Ok(Page::new(items, total, page))
    }

    /// 有覆盖的用户数。后台首页用来说明「有多少人不在全局默认上」。
    pub async fn count_custom_policies(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM rate_limit_policies WHERE user_id IS NOT NULL",
                Args::new(),
            )
            .await
    }

    // ---------- 计数 ----------

    /// 累加一个窗口的计数，返回累加后的 `count`。
    ///
    /// 同一窗口重复调用是 UPSERT：`UNIQUE (scope, subject, kind, window_start)`
    /// 决定它只会更新那一行。返回值从库里读回来，而不是自己算——
    /// 多实例下别的实例可能刚刚也加过，只有库里的值才是真的。
    pub async fn bump_counter(
        &self,
        scope_value: &str,
        subject: &str,
        kind_value: &str,
        window_start: DateTime<Utc>,
        count_delta: i64,
        bytes_delta: i64,
    ) -> Result<i64> {
        let sql = self.db.sql_v(
            "INSERT INTO rate_counters (scope, subject, kind, window_start, count, bytes, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (scope, subject, kind, window_start) DO UPDATE SET \
                 count = count + ?, bytes = bytes + ?, updated_at = ?",
            "INSERT INTO rate_counters (scope, subject, kind, window_start, count, bytes, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE count = count + VALUES(count), \
                 bytes = bytes + VALUES(bytes), updated_at = VALUES(updated_at)",
            "INSERT INTO rate_counters (scope, subject, kind, window_start, count, bytes, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (scope, subject, kind, window_start) DO UPDATE SET \
                 count = count + ?, bytes = bytes + ?, updated_at = ?",
        );

        let start = ts(window_start);
        let stamp = now_ts();
        let mut args = Args::new()
            .text(scope_value)
            .text(subject)
            .text(kind_value)
            .text(&start)
            .int(count_delta)
            .int(bytes_delta)
            .text(&stamp);
        // PG/SQLite 的 `DO UPDATE SET count = count + ?` 要把增量再写一遍
        // （只重复这三个值，不是整组插入值）；MySQL 自增引用 `VALUES(...)`，不用重复
        if self.db.dialect() != Dialect::MySql {
            args = args.int(count_delta).int(bytes_delta).text(&stamp);
        }
        self.db.execute(&sql, args).await?;

        self.read_counter(scope_value, subject, kind_value, window_start)
            .await
    }

    /// 读窗口里的次数。窗口还没开始计数时是 0，不是错误。
    pub async fn read_counter(
        &self,
        scope_value: &str,
        subject: &str,
        kind_value: &str,
        window_start: DateTime<Utc>,
    ) -> Result<i64> {
        let row = self
            .db
            .fetch_optional(
                "SELECT count AS total FROM rate_counters \
                 WHERE scope = ? AND subject = ? AND kind = ? AND window_start = ?",
                Args::new()
                    .text(scope_value)
                    .text(subject)
                    .text(kind_value)
                    .text(ts(window_start)),
            )
            .await?;
        match row {
            Some(row) => get_i64(&row, "total"),
            None => Ok(0),
        }
    }

    /// 读窗口里的字节数。上传流量类额度用这个。
    pub async fn read_counter_bytes(
        &self,
        scope_value: &str,
        subject: &str,
        kind_value: &str,
        window_start: DateTime<Utc>,
    ) -> Result<i64> {
        let row = self
            .db
            .fetch_optional(
                "SELECT bytes AS total FROM rate_counters \
                 WHERE scope = ? AND subject = ? AND kind = ? AND window_start = ?",
                Args::new()
                    .text(scope_value)
                    .text(subject)
                    .text(kind_value)
                    .text(ts(window_start)),
            )
            .await?;
        match row {
            Some(row) => get_i64(&row, "total"),
            None => Ok(0),
        }
    }

    /// 某个用户当天已上传的字节数。上传前检查日流量额度用得上。
    pub async fn sum_bytes_today(&self, user_id: i64, window_start: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COALESCE(SUM(bytes), 0) AS total FROM rate_counters \
                 WHERE scope = 'user' AND subject = ? AND kind = 'upload_bytes_day' \
                   AND window_start = ?",
                Args::new().text(user_id.to_string()).text(ts(window_start)),
            )
            .await
    }

    /// 清理过期窗口。计数行只在窗口内有用，留着只会让表越长越大。
    pub async fn purge_counters_before(&self, before: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM rate_counters WHERE window_start < ?",
                Args::new().text(ts(before)),
            )
            .await
    }
}

/// 管理页共用的用户列 + 「有没有覆盖」派生列，与 `users.rs` 的口径一致。
const USER_POLICY_SQL: &str = "SELECT u.id, u.uid, u.email, u.email_verified, u.role, u.status, u.quota_bytes, \
        u.used_bytes, u.created_at, u.last_login_at, \
        (SELECT COUNT(*) FROM images i WHERE i.owner_id = u.id AND i.deleted_at IS NULL) AS image_count, \
        (SELECT COUNT(*) FROM rate_limit_policies r WHERE r.user_id = u.id) AS rate_custom \
   FROM users u";

/// UPSERT 里参数要写几遍：PG 与 SQLite 的 `DO UPDATE SET x = ?` 得把值再给一次，
/// MySQL 用 `VALUES(x)` 引用刚插入的值，只给一遍。
fn upsert_passes(dialect: Dialect) -> usize {
    match dialect {
        Dialect::MySql => 1,
        _ => 2,
    }
}

/// 按 `rate_limit_policies` 的列顺序拼参数（不含 `user_id`）。
/// 策略有十几列，把「值怎么排」收在这里，两个 UPSERT 才不用各写一遍。
fn rate_policy_args(initial: Args, policy: &RateLimitPolicy, stamp: &str, passes: usize) -> Args {
    let mut args = initial;
    for _ in 0..passes {
        args = args
            .int(policy.api_per_minute)
            .int(policy.api_per_hour)
            .int(policy.api_read_per_minute)
            .int(policy.api_burst)
            .int(policy.upload_per_minute)
            .int(policy.upload_per_hour)
            .int(policy.upload_per_day)
            .int(policy.upload_bytes_per_day)
            .int(policy.max_concurrent_uploads)
            .int(policy.max_batch_size)
            .int(policy.max_file_size)
            .int(policy.max_pixels)
            .int(policy.max_width)
            .int(policy.max_height)
            .opt_text(policy.formats.clone())
            .int(policy.download_per_minute)
            .int(policy.download_per_day)
            .bool(policy.unlimited)
            .opt_text(policy.note.clone())
            .text(stamp);
    }
    args
}

/// 一批用户的覆盖策略。分页只有几十行，一次 `IN` 查回来就够，不必逐行回查。
async fn policies_for_users(db: &Db, user_ids: &[i64]) -> Result<HashMap<i64, RateLimitPolicy>> {
    let mut policies = HashMap::new();
    if user_ids.is_empty() {
        return Ok(policies);
    }

    let placeholders = vec!["?"; user_ids.len()].join(", ");
    let mut args = Args::new();
    for id in user_ids {
        args = args.int(*id);
    }
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {} FROM rate_limit_policies WHERE user_id IN ({placeholders})",
                RateLimitPolicy::COLUMNS
            ),
            args,
        )
        .await?;
    for row in rows {
        let policy = RateLimitPolicy::from_row(&row)?;
        if let Some(user_id) = policy.user_id {
            policies.insert(user_id, policy);
        }
    }
    Ok(policies)
}
