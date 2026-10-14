//! IP 规则仓储。
//!
//! 规则条数不多（几十到几百），匹配放在进程内用 `ipnet` 做，因此这里只负责
//! 存取与命中计数。CIDR 以文本存，格式校验在服务层。
//!
//! 白名单优先于黑名单：同一条地址同时命中 deny 与 allow 时，allow 赢。

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::error::Result;
use crate::model::{now_ts, ts, IpRule};
use crate::repo::{like_pattern, Repo};

/// 新建规则。
#[derive(Debug, Clone)]
pub struct NewIpRule {
    pub cidr: String,
    pub kind: String,
    pub scope: String,
    pub note: Option<String>,
    pub is_auto: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_by: Option<i64>,
}

/// 规则列表的过滤条件。
#[derive(Debug, Clone, Default)]
pub struct IpRuleFilter {
    pub kind: Option<String>,
    pub scope: Option<String>,
    pub is_auto: Option<bool>,
    pub query: Option<String>,
    /// 只返回未过期的
    pub active_only: bool,
}

impl Repo {
    // ---------- 读取 ----------

    /// 管理页列表。按创建时间倒序：刚加的规则排在最前，符合「刚才那条加上没有」的用法。
    pub async fn list_ip_rules(&self, filter: &IpRuleFilter, limit: i64) -> Result<Vec<IpRule>> {
        let (where_sql, args) = ip_rule_where(filter);
        let sql = format!(
            "SELECT {} FROM ip_rules{where_sql} ORDER BY created_at DESC, id DESC LIMIT ?",
            IpRule::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(limit.clamp(1, 500)))
            .await?;
        rows.iter().map(IpRule::from_row).collect()
    }

    /// 与列表同一组条件，避免两处判断「什么算过期」时走偏。
    pub async fn count_ip_rules(&self, filter: &IpRuleFilter) -> Result<i64> {
        let (where_sql, args) = ip_rule_where(filter);
        self.db
            .count(
                &format!("SELECT COUNT(*) AS total FROM ip_rules{where_sql}"),
                args,
            )
            .await
    }

    /// 鉴权前把这份清单加载进内存做匹配：只要还没过期的，deny 与 allow 都取。
    pub async fn active_rules(&self, at: DateTime<Utc>) -> Result<Vec<IpRule>> {
        let sql = format!(
            "SELECT {} FROM ip_rules WHERE expires_at IS NULL OR expires_at > ? \
             ORDER BY id ASC",
            IpRule::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().text(ts(at))).await?;
        rows.iter().map(IpRule::from_row).collect()
    }

    /// 自动封禁去重用：同一条网段已经有生效中的规则就不再重复加。
    /// `kind` 要一起比，否则「放行」会被「封禁」挡住。
    pub async fn find_active_rule(
        &self,
        cidr: &str,
        kind: &str,
        at: DateTime<Utc>,
    ) -> Result<Option<IpRule>> {
        let sql = format!(
            "SELECT {} FROM ip_rules WHERE cidr = ? AND kind = ? \
             AND (expires_at IS NULL OR expires_at > ?) ORDER BY id ASC LIMIT 1",
            IpRule::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(cidr).text(kind).text(ts(at)))
            .await?;
        row.as_ref().map(IpRule::from_row).transpose()
    }

    /// 生效中的自动规则条数。风控用来判断「今天是不是已经封了太多网段」。
    pub async fn count_auto_rules(&self, at: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM ip_rules WHERE is_auto = 1 \
                 AND (expires_at IS NULL OR expires_at > ?)",
                Args::new().text(ts(at)),
            )
            .await
    }

    // ---------- 写入 ----------

    pub async fn create_ip_rule(&self, new: &NewIpRule) -> Result<IpRule> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO ip_rules (cidr, kind, scope, note, hits, is_auto, expires_at, \
                                       created_by, created_at) \
                 VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?)",
                Args::new()
                    .text(&new.cidr)
                    .text(&new.kind)
                    .text(&new.scope)
                    .opt_text(new.note.clone())
                    .bool(new.is_auto)
                    .opt_text(new.expires_at.map(ts))
                    .opt_int(new.created_by)
                    .text(now_ts()),
            )
            .await?;

        let sql = format!("SELECT {} FROM ip_rules WHERE id = ?", IpRule::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref()
            .map(IpRule::from_row)
            .transpose()?
            .ok_or_else(|| crate::RepoError::NotFound(format!("新建 IP 规则 {id}")))
    }

    /// 命中一次。计数只用于后台展示，不参与匹配，所以单独一条语句直接加。
    pub async fn bump_ip_rule_hits(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE ip_rules SET hits = hits + 1 WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 清掉临时规则。返回删掉的行数，后台日志里要显示清理了多少条。
    pub async fn purge_expired_ip_rules(&self, at: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM ip_rules WHERE expires_at IS NOT NULL AND expires_at <= ?",
                Args::new().text(ts(at)),
            )
            .await
    }

    pub async fn delete_ip_rule(&self, id: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM ip_rules WHERE id = ?", Args::new().int(id))
            .await?;
        Ok(())
    }
}

/// 列表与计数共用的条件拼装。
///
/// `active_only` 的「未过期」判定与 [`Repo::active_rules`] 保持同一写法，
/// 否则会出现「列表里看不到但匹配时生效」的规则。
fn ip_rule_where(filter: &IpRuleFilter) -> (String, Args) {
    let mut builder = WhereBuilder::new();

    if let Some(kind) = filter.kind.as_deref().filter(|value| !value.is_empty()) {
        builder = builder.and("kind = ?", |args| args.text(kind));
    }
    if let Some(scope) = filter.scope.as_deref().filter(|value| !value.is_empty()) {
        builder = builder.and("scope = ?", |args| args.text(scope));
    }
    if let Some(is_auto) = filter.is_auto {
        builder = builder.and("is_auto = ?", |args| args.bool(is_auto));
    }
    if let Some(query) = filter
        .query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = like_pattern(query);
        builder = builder.and(
            "(cidr LIKE ? ESCAPE '\\' OR note LIKE ? ESCAPE '\\')",
            move |args| args.text(pattern.clone()).text(pattern),
        );
    }
    if filter.active_only {
        let now = now_ts();
        builder = builder.and("(expires_at IS NULL OR expires_at > ?)", move |args| {
            args.text(now)
        });
    }

    (builder.sql_string(), builder.args())
}
