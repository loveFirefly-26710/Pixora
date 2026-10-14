//! 服务条款仓储：版本化的条款本体 + 只增不改的同意记录。
//!
//! 两条硬约束：
//!   1. 同一时间只有一个 `active` 版本（数据库唯一索引兜底，`publish` 里先归档旧的）。
//!   2. 同意记录里冗余存一份版本号，条款被删也查得到「当初同意的是哪一版」。
//!      生效中的版本不允许直接删除，必须先让另一个版本生效。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::model::{get_i64, now_ts, ts, TermsConsent, TermsVersion};
use crate::repo::Repo;

/// 新建或修改条款的入参。
#[derive(Debug, Clone)]
pub struct NewTermsVersion {
    pub version: String,
    pub title: String,
    pub body: String,
    pub summary: Option<String>,
    pub require_reconsent: bool,
    pub effective_at: Option<DateTime<Utc>>,
    pub created_by: Option<i64>,
}

/// 条款管理列表行：版本 + 同意人数。
#[derive(Debug, Clone)]
pub struct TermsRow {
    pub version: TermsVersion,
    pub consent_count: i64,
    pub user_total: i64,
}

impl Repo {
    // ---------- 条款本体 ----------

    /// 新建一版条款。一律以 `draft` 落地，生效必须走 [`Repo::publish_terms`]，
    /// 这样「写好了但还没对外」和「已经对外」在库里始终区分得开。
    pub async fn create_terms(&self, new: &NewTermsVersion) -> Result<TermsVersion> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO terms_versions (version, title, body, summary, status, \
                     require_reconsent, effective_at, published_at, created_by, created_at) \
                 VALUES (?, ?, ?, ?, 'draft', ?, ?, NULL, ?, ?)",
                Args::new()
                    .text(&new.version)
                    .text(&new.title)
                    .text(&new.body)
                    .opt_text(new.summary.clone())
                    .bool(new.require_reconsent)
                    .opt_text(new.effective_at.map(ts))
                    .opt_int(new.created_by)
                    .text(now_ts()),
            )
            .await?;

        self.find_terms(id)
            .await?
            .ok_or_else(|| crate::RepoError::NotFound(format!("新建条款 {id}")))
    }

    pub async fn find_terms(&self, id: i64) -> Result<Option<TermsVersion>> {
        let sql = format!(
            "SELECT {} FROM terms_versions WHERE id = ?",
            TermsVersion::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(TermsVersion::from_row).transpose()
    }

    pub async fn find_terms_by_version(&self, version: &str) -> Result<Option<TermsVersion>> {
        let sql = format!(
            "SELECT {} FROM terms_versions WHERE version = ?",
            TermsVersion::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(version))
            .await?;
        row.as_ref().map(TermsVersion::from_row).transpose()
    }

    /// 当前生效版本。注册页与「重新同意」弹窗都从这里取。
    /// 唯一索引保证至多一行，这里再限一次取一行只是顺手防脏数据。
    pub async fn active_terms(&self) -> Result<Option<TermsVersion>> {
        let sql = format!(
            "SELECT {} FROM terms_versions WHERE status = 'active' ORDER BY id DESC LIMIT 1",
            TermsVersion::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new()).await?;
        row.as_ref().map(TermsVersion::from_row).transpose()
    }

    /// 管理列表：版本 + 同意人数 + 用户总数，界面直接显示「3/128 人已同意」。
    pub async fn list_terms(&self) -> Result<Vec<TermsRow>> {
        let sql = format!(
            "SELECT {}, \
                (SELECT COUNT(*) FROM terms_consents c WHERE c.version_id = v.id) AS consent_count, \
                (SELECT COUNT(*) FROM users) AS user_total \
               FROM terms_versions v ORDER BY v.created_at DESC, v.id DESC",
            TermsVersion::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new()).await?;
        rows.iter()
            .map(|row| {
                Ok(TermsRow {
                    version: TermsVersion::from_row(row)?,
                    consent_count: get_i64(row, "consent_count")?,
                    user_total: get_i64(row, "user_total")?,
                })
            })
            .collect()
    }

    /// 覆盖条款内容。只该用于草稿：已生效的版本改了正文，之前同意的记录就说不清了。
    /// 状态校验在服务层，这里照写不误。
    pub async fn update_terms(&self, id: i64, new: &NewTermsVersion) -> Result<()> {
        self.db
            .execute(
                "UPDATE terms_versions SET version = ?, title = ?, body = ?, summary = ?, \
                     require_reconsent = ?, effective_at = ? WHERE id = ?",
                Args::new()
                    .text(&new.version)
                    .text(&new.title)
                    .text(&new.body)
                    .opt_text(new.summary.clone())
                    .bool(new.require_reconsent)
                    .opt_text(new.effective_at.map(ts))
                    .int(id),
            )
            .await?;
        Ok(())
    }

    /// 发布一版条款：归档旧的 active，再把这一版置为 active。
    ///
    /// 唯一索引 `(status) WHERE status = 'active'` 只允许一行 active，
    /// 所以两步必须在一个事务里——先归档再置真，中间失败不会出现两个 active，
    /// 但会留下「没有生效版本」的状态，那是不能接受的。
    /// `effective_at` 没填就按「立即生效」写成本次发布时刻。
    pub async fn publish_terms(&self, id: i64, at: DateTime<Utc>) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        let stamp = ts(at);

        sqlx::query(&self.db.sql(
            "UPDATE terms_versions SET status = 'archived' WHERE status = 'active' AND id <> ?",
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let affected = sqlx::query(&self.db.sql(
            "UPDATE terms_versions SET status = 'active', published_at = ?, \
                 effective_at = COALESCE(effective_at, ?) WHERE id = ?",
        ))
        .bind(&stamp)
        .bind(&stamp)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if affected == 0 {
            // 目标行不存在：丢掉事务即回滚，刚归档的那一版会回到 active
            return Err(crate::RepoError::NotFound(format!("发布条款 {id}")));
        }

        tx.commit().await?;
        Ok(())
    }

    /// 下架一个版本，但不指定接任者——生效中的版本要先发布别的版本再归档，
    /// 否则会短暂处于「没有生效条款」的状态。
    pub async fn archive_terms(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE terms_versions SET status = 'archived' WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 删除一个版本。有同意记录的版本（外键 `ON DELETE RESTRICT`）删不掉，
    /// 报错就是正确结果：那些记录还要能指出「当初同意的是哪一版」。
    pub async fn delete_terms(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM terms_versions WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 是否需要重新同意。改版后忘了勾这一项是运营事故，所以单独留一个开关。
    pub async fn set_require_reconsent(&self, id: i64, value: bool) -> Result<()> {
        self.db
            .execute(
                "UPDATE terms_versions SET require_reconsent = ? WHERE id = ?",
                Args::new().bool(value).int(id),
            )
            .await?;
        Ok(())
    }

    // ---------- 同意记录 ----------

    /// 记一次同意。`version` 号冗余存一份，条款日后被删也查得到当初同意的是哪一版。
    ///
    /// 重复提交（刷新页面、双开标签）不该报错，唯一键命中就直接忽略。
    pub async fn record_consent(
        &self,
        user_id: i64,
        version: &TermsVersion,
        ip: Option<&str>,
        user_agent: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO terms_consents (user_id, version_id, version, agreed_at, ip, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (user_id, version_id) DO NOTHING",
            "INSERT IGNORE INTO terms_consents (user_id, version_id, version, agreed_at, ip, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?)",
            "INSERT INTO terms_consents (user_id, version_id, version, agreed_at, ip, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (user_id, version_id) DO NOTHING",
        );
        self.db
            .execute(
                &sql,
                Args::new()
                    .int(user_id)
                    .int(version.id)
                    .text(&version.version)
                    .text(ts(at))
                    .opt_text(ip.map(str::to_string))
                    .opt_text(user_agent.map(str::to_string)),
            )
            .await?;
        Ok(())
    }

    pub async fn find_consent(
        &self,
        user_id: i64,
        version_id: i64,
    ) -> Result<Option<TermsConsent>> {
        let sql = format!(
            "SELECT {} FROM terms_consents WHERE user_id = ? AND version_id = ?",
            TermsConsent::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().int(user_id).int(version_id))
            .await?;
        row.as_ref().map(TermsConsent::from_row).transpose()
    }

    /// 某个用户同意过的全部版本，最近的在前。
    pub async fn list_consents(&self, user_id: i64) -> Result<Vec<TermsConsent>> {
        let sql = format!(
            "SELECT {} FROM terms_consents WHERE user_id = ? ORDER BY agreed_at DESC, id DESC",
            TermsConsent::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(user_id)).await?;
        rows.iter().map(TermsConsent::from_row).collect()
    }

    pub async fn count_consents(&self, version_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM terms_consents WHERE version_id = ?",
                Args::new().int(version_id),
            )
            .await
    }

    /// 当前生效版本是否已被该用户同意。
    ///
    /// 没有生效版本时返回 `true`：实例还没发布过条款，不该拦着任何人用功能。
    pub async fn has_consented_active(&self, user_id: i64) -> Result<bool> {
        let Some(active) = self.active_terms().await? else {
            return Ok(true);
        };
        Ok(self.find_consent(user_id, active.id).await?.is_some())
    }
}
