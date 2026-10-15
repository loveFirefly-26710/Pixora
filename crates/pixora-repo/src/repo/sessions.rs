//! 会话仓储。
//!
//! 会话独立建表，一行的生命周期：签发 → 每次访问刷新 `last_seen_at` → 到期/被吊销。
//! 账号被暂停或封禁时按 `user_id` 批量吊销，这就是「踢下线」的全部实现。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::model::*;
use crate::repo::Repo;

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

    /// 清理过期会话：已吊销或长期未活动的行。
    pub async fn purge_stale_sessions(&self, older_than: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM sessions WHERE revoked_at IS NOT NULL OR last_seen_at < ?",
                Args::new().text(ts(older_than)),
            )
            .await
    }
}
