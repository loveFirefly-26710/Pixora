//! 用户仓储：账号本体、登录簿记、配额与用量、处置动作、管理端列表。
//!
//! 这个文件是其余仓储模块的写法参考：SQL 里写 `?`，值用 [`Args`] 按顺序给，
//! 行模型在 `model.rs` 里统一解码。

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::error::Result;
use crate::model::*;
use crate::repo::images::PurgedBlobs;
use crate::repo::{like_pattern, Page, PageRequest, Repo};

/// 新建用户所需的字段。UID 由服务层生成并保证唯一。
#[derive(Debug, Clone)]
pub struct NewUser {
    pub uid: String,
    pub email: String,
    pub password_hash: String,
    pub role: String,
    pub status: String,
    pub quota_bytes: i64,
    pub email_verified: bool,
}

/// 管理端列表的过滤条件。空字段表示不过滤。
#[derive(Debug, Clone, Default)]
pub struct UserFilter {
    /// 匹配邮箱或 UID
    pub query: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
}

impl Repo {
    // ---------- 存在性判断 ----------

    pub async fn email_exists(&self, email: &str) -> Result<bool> {
        self.db
            .exists("SELECT 1 AS hit FROM users WHERE email = ?", Args::new().text(email))
            .await
    }

    pub async fn uid_exists(&self, uid: &str) -> Result<bool> {
        self.db
            .exists("SELECT 1 AS hit FROM users WHERE uid = ?", Args::new().text(uid))
            .await
    }

    /// 封禁用户会阻断同邮箱重新注册，注册前用这个查。
    pub async fn email_banned(&self, email: &str) -> Result<bool> {
        self.db
            .exists(
                "SELECT 1 AS hit FROM users WHERE email = ? AND status = 'banned'",
                Args::new().text(email),
            )
            .await
    }

    // ---------- 读取 ----------

    pub async fn find_user_by_id(&self, id: i64) -> Result<Option<User>> {
        let sql = format!("SELECT {} FROM users WHERE id = ?", User::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(User::from_row).transpose()
    }

    pub async fn find_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {} FROM users WHERE email = ?", User::COLUMNS);
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(email))
            .await?;
        row.as_ref().map(User::from_row).transpose()
    }

    pub async fn find_user_by_uid(&self, uid: &str) -> Result<Option<User>> {
        let sql = format!("SELECT {} FROM users WHERE uid = ?", User::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().text(uid)).await?;
        row.as_ref().map(User::from_row).transpose()
    }

    /// 最早创建的那个账号的 UID。
    ///
    /// 安装账号的兜底识别：`install.admin_uid` 这个设置项是后加的，早先装好的
    /// 实例里没有它。账号按 id 递增创建，第一个就是当年安装时建的管理员。
    pub async fn first_user_uid(&self) -> Result<Option<String>> {
        let row = self
            .db
            .fetch_optional("SELECT uid FROM users ORDER BY id ASC LIMIT 1", Args::new())
            .await?;
        row.map(|row| get_str(&row, "uid")).transpose()
    }

    /// 管理端详情：用户 + 图片数 + 是否有自定义限流。
    pub async fn find_user_summary(&self, id: i64) -> Result<Option<UserSummary>> {
        let row = self
            .db
            .fetch_optional(USER_SUMMARY_SQL_WHERE, Args::new().int(id))
            .await?;
        match row {
            Some(row) => Ok(Some(UserSummary::from_row(&row)?)),
            None => Ok(None),
        }
    }

    // ---------- 写入 ----------

    /// 建账号。UID 冲突由调用方重试（[`crate::RepoError::is_unique_violation`] 能识别）。
    pub async fn create_user(&self, new: &NewUser) -> Result<User> {
        let sql = "INSERT INTO users (uid, email, email_verified, password_hash, role, status, \
                                    quota_bytes, used_bytes, login_fail_count, created_at) \
                   VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?)";
        let args = Args::new()
            .text(&new.uid)
            .text(&new.email)
            .bool(new.email_verified)
            .text(&new.password_hash)
            .text(&new.role)
            .text(&new.status)
            .int(new.quota_bytes)
            .text(now_ts());
        self.db.insert_returning_id(sql, args).await?;

        self.find_user_by_email(&new.email)
            .await?
            .ok_or_else(|| crate::RepoError::NotFound(format!("新建用户 {}", new.email)))
    }

    pub async fn update_user_password(&self, id: i64, password_hash: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET password_hash = ? WHERE id = ?",
                Args::new().text(password_hash).int(id),
            )
            .await?;
        Ok(())
    }

    /// 设头像（`None` = 清掉）。地址的形态由服务层归一，仓储只负责写。
    pub async fn set_user_avatar(&self, id: i64, avatar: Option<&str>) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET avatar = ? WHERE id = ?",
                Args::new().opt_text(avatar.map(str::to_string)).int(id),
            )
            .await?;
        Ok(())
    }

    /// 设「下次登录必须改密码」。管理员直接设密码时置上，用户自己改完清掉。
    pub async fn set_must_change_password(&self, id: i64, required: bool) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET must_change_password = ? WHERE id = ?",
                Args::new().bool(required).int(id),
            )
            .await?;
        Ok(())
    }

    /// 改邮箱并显式设置验证状态。管理员直接改时通常传 `true`。
    pub async fn set_email_verified(&self, id: i64, email: &str, verified: bool) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET email = ?, email_verified = ? WHERE id = ?",
                Args::new().text(email).bool(verified).int(id),
            )
            .await?;
        Ok(())
    }

    /// 激活账号：邮箱验证通过后的那一步。
    pub async fn activate_user(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET status = ?, email_verified = ?, status_reason = NULL, \
                                  status_changed_at = ? WHERE id = ?",
                Args::new()
                    .text(status::ACTIVE)
                    .bool(true)
                    .text(now_ts())
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 登录成功：清失败计数与锁，记录时间与 IP。
    pub async fn record_login_success(&self, id: i64, ip: Option<&str>) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET login_fail_count = 0, locked_until = NULL, \
                                  last_login_at = ?, last_login_ip = ? WHERE id = ?",
                Args::new()
                    .text(now_ts())
                    .opt_text(ip.map(str::to_string))
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 登录失败：累加计数，必要时写入锁定截止时间。
    pub async fn record_login_failure(
        &self,
        id: i64,
        fail_count: i32,
        locked_until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET login_fail_count = ?, locked_until = ? WHERE id = ?",
                Args::new()
                    .int(fail_count as i64)
                    .opt_text(locked_until.map(ts))
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 处置账号：改状态 + 记原因 + 记操作人。会话吊销由服务层接着做。
    ///
    /// `auto_unsuspend_at` 只有「内容审核自动暂停」会给值；管理员手工处置一律传
    /// `None`，这样自动恢复的扫描不会把管理员的封禁顺手解掉。
    pub async fn update_user_status(
        &self,
        id: i64,
        status_value: &str,
        reason: Option<&str>,
        by: Option<i64>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.update_user_status_until(id, status_value, reason, by, at, None)
            .await
    }

    /// 带自动恢复时间的处置。`until` 到了由 `ensure_loginable` 一侧自动解除。
    pub async fn update_user_status_until(
        &self,
        id: i64,
        status_value: &str,
        reason: Option<&str>,
        by: Option<i64>,
        at: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET status = ?, status_reason = ?, status_changed_at = ?, \
                                  status_changed_by = ?, auto_unsuspend_at = ? WHERE id = ?",
                Args::new()
                    .text(status_value)
                    .opt_text(reason.map(str::to_string))
                    .text(ts(at))
                    .opt_int(by)
                    .opt_text(until.map(ts))
                    .int(id),
            )
            .await?;
        Ok(())
    }

    pub async fn update_user_quota(&self, id: i64, quota_bytes: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET quota_bytes = ? WHERE id = ?",
                Args::new().int(quota_bytes).int(id),
            )
            .await?;
        Ok(())
    }

    /// 用量增减。删除图片时传负数，低谷在 0 处截断。
    pub async fn add_used_bytes(&self, id: i64, delta: i64) -> Result<()> {
        self.db
            .execute(
                // 两参数的 MAX 只有 MySQL / SQLite 认，PostgreSQL 要用 GREATEST
                &self.db.sql_v(
                    "UPDATE users SET used_bytes = GREATEST(used_bytes + ?, 0) WHERE id = ?",
                    "UPDATE users SET used_bytes = GREATEST(used_bytes + ?, 0) WHERE id = ?",
                    "UPDATE users SET used_bytes = MAX(used_bytes + ?, 0) WHERE id = ?",
                ),
                Args::new().int(delta).int(id),
            )
            .await?;
        Ok(())
    }

    /// 对账：按已存文件重算用量，修正漂移。
    pub async fn recompute_used_bytes(&self, id: i64) -> Result<i64> {
        self.db
            .execute(USER_RECOMPUTE_SQL, Args::new().int(id).int(id))
            .await?;
        Ok(self
            .find_user_by_id(id)
            .await?
            .map(|user| user.used_bytes)
            .unwrap_or(0))
    }

    pub async fn set_totp(&self, id: i64, secret: Option<&str>, enabled: bool) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET totp_secret = ?, totp_enabled = ? WHERE id = ?",
                Args::new()
                    .opt_text(secret.map(str::to_string))
                    .bool(enabled)
                    .int(id),
            )
            .await?;
        Ok(())
    }

    pub async fn update_user_email(&self, id: i64, email: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET email = ? WHERE id = ?",
                Args::new().text(email).int(id),
            )
            .await?;
        Ok(())
    }

    /// 一批 UID → (uid, email)。账号历史要按操作人 id 补出「是谁做的」，
    /// 一页几十条逐条查会变成 N+1。
    pub async fn identities_for_ids(&self, ids: &[i64]) -> Result<std::collections::HashMap<i64, (String, String)>> {
        let mut out = std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("SELECT id, uid, email FROM users WHERE id IN ({placeholders})");
        let rows = self
            .db
            .fetch_all(&sql, ids.iter().fold(Args::new(), |args, id| args.int(*id)))
            .await?;
        for row in rows.iter() {
            out.insert(
                get_i64(row, "id")?,
                (get_str(row, "uid")?, get_str(row, "email")?),
            );
        }
        Ok(out)
    }

    pub async fn update_user_role(&self, id: i64, role_value: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE users SET role = ? WHERE id = ?",
                Args::new().text(role_value).int(id),
            )
            .await?;
        Ok(())
    }

    /// 删除账号，返回它用过的实体（原图 + 缩略图）。
    ///
    /// 图片、相册、令牌、会话、频率策略这些从属表都在外键上挂了级联，删掉这一行
    /// 就一起走。但 `blobs.ref_count` 不会自己变小——级联删行绕开了我们那套记账，
    /// 计数停在原地的话 GC 永远等不到 `ref_count = 0`，文件就再也收不走了。
    /// 所以删之前先把计数重算成「除它之外还有多少条活引用」：只还自己那一份，
    /// 别人也在用的共享实体不受影响。
    ///
    /// 顺序不能反：删完 `users` 就没有 image 行可依了。返回的实体交给调用方
    /// 回收物理文件。
    pub async fn delete_user(&self, id: i64) -> Result<Vec<PurgedBlobs>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT DISTINCT blob_id, thumb_blob_id FROM images WHERE owner_id = ?",
                Args::new().int(id),
            )
            .await?;
        let mut blobs = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            blobs.push(PurgedBlobs {
                blob_id: get_i64(row, "blob_id")?,
                thumb_blob_id: get_opt_i64(row, "thumb_blob_id")?,
            });
        }

        let mut tx = self.db.pool().begin().await?;
        // 活引用才占着计数；进过回收站的在软删时已经还过了
        for column in ["blob_id", "thumb_blob_id"] {
            let sql = format!(
                "UPDATE blobs SET ref_count = ( \
                     SELECT COUNT(*) FROM images i WHERE i.{column} = blobs.id \
                       AND i.deleted_at IS NULL AND i.owner_id <> ? \
                 ) WHERE id IN (SELECT {column} FROM images WHERE owner_id = ? AND {column} IS NOT NULL)"
            );
            sqlx::query(&self.db.sql(&sql))
                .bind(id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        // MySQL 那份表结构里，频率策略行的外键不能挂级联（生成列的基列不允许），
        // 只能在这里自己清；另外两种方言上这条也是幂等的空操作。
        sqlx::query(&self.db.sql("DELETE FROM rate_limit_policies WHERE user_id = ?"))
            .bind(id)
            .execute(&mut *tx)
            .await?;

        sqlx::query(&self.db.sql("DELETE FROM users WHERE id = ?"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(blobs)
    }

    // ---------- 列表与统计 ----------

    /// 活跃的管理员。内容审核上报、批量通知这类「告诉所有管理员」的场景用。
    pub async fn list_active_admins(&self) -> Result<Vec<User>> {
        let sql = format!(
            "SELECT {} FROM users WHERE role = ? AND status = ? ORDER BY id",
            User::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().text(role::ADMIN).text(status::ACTIVE))
            .await?;
        rows.iter().map(User::from_row).collect()
    }

    /// 管理端用户列表。带图片数量与「是否自定义限流」两个派生列。
    pub async fn list_users(
        &self,
        filter: &UserFilter,
        page: PageRequest,
    ) -> Result<Page<UserSummary>> {
        let mut filters = WhereBuilder::new();
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            let pattern = like_pattern(query);
            filters = filters.and(
                "(u.email LIKE ? ESCAPE '\\' OR u.uid LIKE ? ESCAPE '\\')",
                |args| args.text(pattern.clone()).text(pattern),
            );
        }
        if let Some(role_value) = filter.role.as_deref().filter(|r| !r.is_empty()) {
            filters = filters.and("u.role = ?", |args| args.text(role_value));
        }
        if let Some(status_value) = filter.status.as_deref().filter(|s| !s.is_empty()) {
            filters = filters.and("u.status = ?", |args| args.text(status_value));
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
                &format!("{USER_SUMMARY_SQL}{where_sql} ORDER BY u.id ASC LIMIT ? OFFSET ?"),
                args.int(page.per_page).int(page.offset()),
            )
            .await?;

        let items = rows
            .iter()
            .map(UserSummary::from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Page::new(items, total, page))
    }

    /// 已分配配额合计。配额总额校验的第一半。
    pub async fn sum_allocated_quota(&self, exclude_user: Option<i64>) -> Result<i64> {
        match exclude_user {
            Some(id) => {
                self.db
                    .count(
                        &format!(
                            "SELECT {} AS total FROM users WHERE id <> ?",
                            self.db.dialect().sum_i64("quota_bytes")
                        ),
                        Args::new().int(id),
                    )
                    .await
            }
            None => {
                self.db
                    .count(
                        &format!(
                            "SELECT {} AS total FROM users",
                            self.db.dialect().sum_i64("quota_bytes")
                        ),
                        Args::new(),
                    )
                    .await
            }
        }
    }

    pub async fn sum_used_bytes(&self) -> Result<i64> {
        self.db
            .count(
                &format!(
                    "SELECT {} AS total FROM users",
                    self.db.dialect().sum_i64("used_bytes")
                ),
                Args::new(),
            )
            .await
    }

    pub async fn count_users(&self) -> Result<i64> {
        self.db
            .count("SELECT COUNT(*) AS total FROM users", Args::new())
            .await
    }

    pub async fn count_users_created_since(&self, since: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM users WHERE created_at >= ?",
                Args::new().text(ts(since)),
            )
            .await
    }

    /// 用量排行，配额分配明细用。
    pub async fn top_users_by_usage(&self, limit: i64) -> Result<Vec<UserSummary>> {
        let rows = self
            .db
            .fetch_all(
                &format!("{USER_SUMMARY_SQL} ORDER BY u.used_bytes DESC, u.id ASC LIMIT ?"),
                Args::new().int(limit.clamp(1, 200)),
            )
            .await?;
        rows.iter().map(UserSummary::from_row).collect()
    }

    /// 配额对账：重算所有用户的用量，返回被修正的行数。
    pub async fn reconcile_all_used_bytes(&self) -> Result<u64> {
        self.db.execute(USER_RECONCILE_ALL_SQL, Args::new()).await
    }

    /// 详情页与账号页需要的派生统计。
    pub async fn user_stats(&self, user_id: i64) -> Result<UserStats> {
        let row = self
            .db
            .fetch_one(
                "SELECT \
                    (SELECT COUNT(*) FROM albums a WHERE a.owner_id = ?) AS album_count, \
                    (SELECT COUNT(*) FROM images i WHERE i.owner_id = ? AND i.deleted_at IS NULL) AS image_count, \
                    (SELECT COUNT(*) FROM images i WHERE i.owner_id = ? AND i.deleted_at IS NULL AND i.visibility = 'public') AS public_count, \
                    (SELECT COUNT(*) FROM tags t WHERE t.owner_id = ?) AS tag_count",
                Args::new()
                    .int(user_id)
                    .int(user_id)
                    .int(user_id)
                    .int(user_id),
            )
            .await?;
        Ok(UserStats {
            album_count: get_i64(&row, "album_count")?,
            image_count: get_i64(&row, "image_count")?,
            public_count: get_i64(&row, "public_count")?,
            tag_count: get_i64(&row, "tag_count")?,
        })
    }

    // ---------- 处置记录 ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_user_action(
        &self,
        user_id: i64,
        actor_id: Option<i64>,
        action: &str,
        reason: Option<&str>,
        detail: &serde_json::Value,
        mail_sent: bool,
        at: DateTime<Utc>,
    ) -> Result<i64> {
        self.db
            .insert_returning_id(
                "INSERT INTO user_actions (user_id, actor_id, action, reason, detail, mail_sent, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                Args::new()
                    .int(user_id)
                    .opt_int(actor_id)
                    .text(action)
                    .opt_text(reason.map(str::to_string))
                    .text(json_text(detail)?)
                    .bool(mail_sent)
                    .text(ts(at)),
            )
            .await
    }

    pub async fn mark_user_action_mail_sent(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE user_actions SET mail_sent = ? WHERE id = ?",
                Args::new().bool(true).int(id),
            )
            .await?;
        Ok(())
    }

    pub async fn list_user_actions(&self, user_id: i64, limit: i64) -> Result<Vec<UserAction>> {
        let rows = self
            .db
            .fetch_all(
                &format!(
                    "SELECT {} FROM user_actions WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
                    UserAction::COLUMNS
                ),
                Args::new().int(user_id).int(limit.clamp(1, 500)),
            )
            .await?;
        rows.iter().map(UserAction::from_row).collect()
    }
}

/// 管理端列表共用的列清单与派生列。
const USER_SUMMARY_SQL: &str = "SELECT u.id, u.uid, u.email, u.email_verified, u.role, u.status, u.quota_bytes, \
        u.used_bytes, u.created_at, u.last_login_at, \
        (SELECT COUNT(*) FROM images i WHERE i.owner_id = u.id AND i.deleted_at IS NULL) AS image_count, \
        (SELECT COUNT(*) FROM rate_limit_policies r WHERE r.user_id = u.id) AS rate_custom \
   FROM users u";

const USER_SUMMARY_SQL_WHERE: &str = "SELECT u.id, u.uid, u.email, u.email_verified, u.role, u.status, u.quota_bytes, \
        u.used_bytes, u.created_at, u.last_login_at, \
        (SELECT COUNT(*) FROM images i WHERE i.owner_id = u.id AND i.deleted_at IS NULL) AS image_count, \
        (SELECT COUNT(*) FROM rate_limit_policies r WHERE r.user_id = u.id) AS rate_custom \
   FROM users u WHERE u.id = ?";

const USER_RECOMPUTE_SQL: &str = "UPDATE users SET used_bytes = COALESCE(( \
        SELECT SUM(b.size) FROM images i JOIN blobs b ON b.id = i.blob_id \
         WHERE i.owner_id = ?), 0) WHERE id = ?";

/// 对账口径：**回收站里的文件也算已用额度**（它们还占着盘，只有彻底删除才归还）。
const USER_RECONCILE_ALL_SQL: &str = "UPDATE users SET used_bytes = COALESCE(( \
        SELECT SUM(b.size) FROM images i JOIN blobs b ON b.id = i.blob_id \
         WHERE i.owner_id = users.id), 0) \
   WHERE used_bytes <> COALESCE(( \
        SELECT SUM(b.size) FROM images i JOIN blobs b ON b.id = i.blob_id \
         WHERE i.owner_id = users.id), 0)";
