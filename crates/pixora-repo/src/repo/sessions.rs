//! 会话仓储。
//!
//! 会话独立建表，一行的生命周期：签发 → 每次访问刷新 `last_seen_at` → 到期/被吊销。
//! 账号被暂停或封禁时按 `user_id` 批量吊销，这就是「踢下线」的全部实现。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::model::*;
use crate::repo::{like_pattern, Page, PageRequest, Repo};

impl Repo {
    /// 建会话行。`expires_at` 只有「记住我」会给：给了就写死绝对时间，
    /// 没给就按「闲置时长 + 最长寿命」在读取侧判断（见 `Session::is_expired`）。
    pub async fn create_session(
        &self,
        user_id: i64,
        token_hash: &str,
        device: Option<&str>,
        ip: Option<&str>,
        user_agent: Option<&str>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Session> {
        let now = now_ts();
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO sessions (user_id, token_hash, device, ip, user_agent, created_at, \
                                       last_seen_at, expires_at, revoked_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)",
                Args::new()
                    .int(user_id)
                    .text(token_hash)
                    .opt_text(device.map(str::to_string))
                    .opt_text(ip.map(str::to_string))
                    .opt_text(user_agent.map(str::to_string))
                    .text(&now)
                    .text(&now)
                    .opt_text(expires_at.map(ts)),
            )
            .await?;

        self.find_session_by_id(id)
            .await?
            .ok_or_else(|| crate::RepoError::NotFound(format!("新建会话 {id}")))
    }

    pub async fn find_session_by_id(&self, id: i64) -> Result<Option<Session>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE id = ?",
            Session::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(Session::from_row).transpose()
    }

    /// 按令牌哈希找会话。鉴权中间件每次请求都会走这里。
    pub async fn find_session_by_hash(&self, token_hash: &str) -> Result<Option<Session>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE token_hash = ? AND revoked_at IS NULL",
            Session::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(token_hash))
            .await?;
        row.as_ref().map(Session::from_row).transpose()
    }

    /// 刷新最近活动时间。IP 变了顺带更新（换网了）。
    pub async fn touch_session(&self, id: i64, ip: Option<&str>, at: DateTime<Utc>) -> Result<()> {
        self.db
            .execute(
                "UPDATE sessions SET last_seen_at = ?, ip = COALESCE(?, ip) WHERE id = ?",
                Args::new()
                    .text(ts(at))
                    .opt_text(ip.map(str::to_string))
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 用户的活跃会话列表（安全页要展示设备、IP、最近活动）。
    ///
    /// 过期的行不在这里筛：判定要用设置里的策略，仓储层读不到。
    /// 服务层筛完会把过期的行顺手吊销，所以列表会越用越干净。
    pub async fn list_sessions(&self, user_id: i64) -> Result<Vec<Session>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE user_id = ? AND revoked_at IS NULL \
             ORDER BY last_seen_at DESC",
            Session::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(user_id)).await?;
        rows.iter().map(Session::from_row).collect()
    }

    /// 全部会话记录，含已吊销与已过期的行。
    ///
    /// 安全页把「当前活动」「免登录记录」「登录历史」分开摆，所以这里连同
    /// 历史一起取回来；`limit` 兜住那些长期不清理的账号。
    pub async fn list_all_sessions(&self, user_id: i64, limit: i64) -> Result<Vec<Session>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE user_id = ? \
             ORDER BY last_seen_at DESC, id DESC LIMIT ?",
            Session::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, Args::new().int(user_id).int(limit.clamp(1, 1000)))
            .await?;
        rows.iter().map(Session::from_row).collect()
    }

    /// 改会话备注。`label` 为 `None` 表示清掉备注。
    pub async fn rename_session(
        &self,
        id: i64,
        user_id: i64,
        label: Option<&str>,
    ) -> Result<u64> {
        self.db
            .execute(
                "UPDATE sessions SET label = ? WHERE id = ? AND user_id = ?",
                Args::new()
                    .opt_text(label.map(str::to_string))
                    .int(id)
                    .int(user_id),
            )
            .await
    }

    /// 彻底删除一条会话记录（用户清历史 / 管理员清理）。
    ///
    /// 与「下线」不同：下线只写 `revoked_at`，记录还在；这里是删行。
    pub async fn delete_session(&self, id: i64, user_id: Option<i64>) -> Result<u64> {
        match user_id {
            Some(user_id) => {
                self.db
                    .execute(
                        "DELETE FROM sessions WHERE id = ? AND user_id = ?",
                        Args::new().int(id).int(user_id),
                    )
                    .await
            }
            None => {
                self.db
                    .execute("DELETE FROM sessions WHERE id = ?", Args::new().int(id))
                    .await
            }
        }
    }

    /// 删除已经结束的会话记录（已吊销 / 已过期）。
    ///
    /// `user_id` 为 `None` 时清全站，管理端「清理历史记录」走这条路。
    pub async fn delete_session_history(
        &self,
        user_id: Option<i64>,
        now: DateTime<Utc>,
        idle_before: Option<DateTime<Utc>>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<u64> {
        // 参数顺序跟 SQL 里的问号走：有 user_id 时它在最前面
        let mut args = match user_id {
            Some(user_id) => Args::new().int(user_id),
            None => Args::new(),
        };
        let mut conditions = vec!["revoked_at IS NOT NULL".to_string()];
        if let Some(at) = idle_before {
            conditions.push(
                "(revoked_at IS NULL AND expires_at IS NULL AND last_seen_at < ?)".to_string(),
            );
            args = args.text(ts(at));
        }
        if let Some(at) = created_before {
            conditions.push(
                "(revoked_at IS NULL AND expires_at IS NULL AND created_at < ?)".to_string(),
            );
            args = args.text(ts(at));
        }
        conditions.push(
            "(revoked_at IS NULL AND expires_at IS NOT NULL AND expires_at <= ?)".to_string(),
        );
        args = args.text(ts(now));
        let where_sql = conditions.join(" OR ");
        match user_id {
            Some(_) => {
                self.db
                    .execute(
                        &format!("DELETE FROM sessions WHERE user_id = ? AND ({where_sql})"),
                        args,
                    )
                    .await
            }
            None => {
                self.db
                    .execute(&format!("DELETE FROM sessions WHERE ({where_sql})"), args)
                    .await
            }
        }
    }

    /// 管理端会话记录列表：按账号、IP、设备或备注搜索。
    pub async fn admin_list_sessions(
        &self,
        query: Option<&str>,
        page: PageRequest,
    ) -> Result<Page<SessionRecord>> {
        let mut where_sql = String::new();
        let mut args = Args::new();
        if let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) {
            let pattern = like_pattern(query);
            where_sql = " WHERE (u.email LIKE ? ESCAPE '\\' OR u.uid LIKE ? ESCAPE '\\' \
                          OR s.ip LIKE ? ESCAPE '\\' OR s.device LIKE ? ESCAPE '\\' \
                          OR s.label LIKE ? ESCAPE '\\')"
                .to_string();
            args = args
                .text(pattern.clone())
                .text(pattern.clone())
                .text(pattern.clone())
                .text(pattern.clone())
                .text(pattern);
        }

        let total = self
            .db
            .count(
                &format!(
                    "SELECT COUNT(*) AS total FROM sessions s JOIN users u ON u.id = s.user_id{where_sql}"
                ),
                args.clone(),
            )
            .await?;

        let sql = format!(
            "SELECT {} FROM sessions s JOIN users u ON u.id = s.user_id{where_sql} \
             ORDER BY s.last_seen_at DESC, s.id DESC LIMIT ? OFFSET ?",
            SessionRecord::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(page.per_page).int(page.offset()))
            .await?;
        let items = rows
            .iter()
            .map(SessionRecord::from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Page::new(items, total, page))
    }

    /// 吊销一批已经过期的会话：记住我的按 `expires_at`，普通的按闲置与寿命两个切点。
    ///
    /// 吊销而不是删除，是为了留着「这台设备什么时候掉线的」这条线索。
    /// 两个切点传 `None` 表示那一项不判（对应设置里填 0 = 不限）。
    pub async fn revoke_expired_sessions(
        &self,
        user_id: i64,
        now: DateTime<Utc>,
        idle_before: Option<DateTime<Utc>>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<u64> {
        // 参数顺序跟着 SQL 里的问号走：SET 的时间 → user_id → 各条件
        let mut args = Args::new().text(ts(now)).int(user_id).text(ts(now));
        let mut conditions = vec!["expires_at IS NOT NULL AND expires_at <= ?".to_string()];
        match (idle_before, created_before) {
            (Some(idle), Some(created)) => {
                conditions.push(
                    "expires_at IS NULL AND (last_seen_at < ? OR created_at < ?)".to_string(),
                );
                args = args.text(ts(idle)).text(ts(created));
            }
            (Some(idle), None) => {
                conditions.push("expires_at IS NULL AND last_seen_at < ?".to_string());
                args = args.text(ts(idle));
            }
            (None, Some(created)) => {
                conditions.push("expires_at IS NULL AND created_at < ?".to_string());
                args = args.text(ts(created));
            }
            (None, None) => {}
        }

        let sql = format!(
            "UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL AND ({})",
            conditions.join(" OR ")
        );
        self.db.execute(&sql, args).await
    }

    /// 吊销一个会话。带上 `user_id` 是为了防止拿别人的会话 id 越权下线。
    pub async fn revoke_session(
        &self,
        id: i64,
        user_id: i64,
        at: DateTime<Utc>,
    ) -> Result<u64> {
        self.db
            .execute(
                "UPDATE sessions SET revoked_at = ? WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
                Args::new().text(ts(at)).int(id).int(user_id),
            )
            .await
    }

    /// 批量吊销：暂停、封禁、改密码、以及用户自己点「下线所有其他设备」都走这里。
    pub async fn revoke_all_sessions(&self, user_id: i64, at: DateTime<Utc>) -> Result<u64> {
        self.revoke_other_sessions(user_id, None, at).await
    }

    /// 吊销除 `keep_session_id` 之外的全部会话。
    pub async fn revoke_other_sessions(
        &self,
        user_id: i64,
        keep_session_id: Option<i64>,
        at: DateTime<Utc>,
    ) -> Result<u64> {
        match keep_session_id {
            Some(keep) => {
                self.db
                    .execute(
                        "UPDATE sessions SET revoked_at = ? \
                          WHERE user_id = ? AND id <> ? AND revoked_at IS NULL",
                        Args::new().text(ts(at)).int(user_id).int(keep),
                    )
                    .await
            }
            None => {
                self.db
                    .execute(
                        "UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
                        Args::new().text(ts(at)).int(user_id),
                    )
                    .await
            }
        }
    }

    pub async fn count_live_sessions(&self, user_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM sessions WHERE user_id = ? AND revoked_at IS NULL",
                Args::new().int(user_id),
            )
            .await
    }

    /// 清理过期会话：已结束且超过保留期的行，或长期未活动的行。
    ///
    /// 以前只要 `revoked_at IS NOT NULL` 就删，登录历史实际上活不过一次 GC；
    /// 现在已下线的记录也按保留期留一阵，用户能在「登录历史」里看到。
    pub async fn purge_stale_sessions(&self, older_than: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM sessions                   WHERE (revoked_at IS NOT NULL AND revoked_at < ?) OR last_seen_at < ?",
                Args::new().text(ts(older_than)).text(ts(older_than)),
            )
            .await
    }
}
