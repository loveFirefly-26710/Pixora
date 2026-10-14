//! 审计日志仓储。
//!
//! 只增不改。写入点分散在各服务里（登录失败、命限流、管理员跨用户访问、删除图片），
//! 因此插入接口要足够省事：一个结构体 + 一次调用。

use chrono::{DateTime, Utc};

use crate::args::{Args, WhereBuilder};
use crate::error::Result;
use crate::model::{get_i64, get_str, json_text, now_ts, ts, AuditLog};
use crate::repo::{prefix_pattern, Page, PageRequest, Repo};

/// 待写入的一条审计事件。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub actor_id: Option<i64>,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
    pub ip: Option<String>,
}

impl AuditEntry {
    /// 常用构造：只有动作，没有目标与细节。
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            actor_id: None,
            action: action.into(),
            target: None,
            detail: serde_json::Value::Null,
            ip: None,
        }
    }

    pub fn actor(mut self, actor_id: Option<i64>) -> Self {
        self.actor_id = actor_id;
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn ip(mut self, ip: Option<String>) -> Self {
        self.ip = ip;
        self
    }
}

/// 审计列表的过滤条件。
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub actor_id: Option<i64>,
    pub action: Option<String>,
    /// 只看某个动作前缀（如 `login`）
    pub action_prefix: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

impl Repo {
    /// 写一条审计。时间戳取当前时刻：审计的意义在于「什么时候发生的」，
    /// 不该由调用方随便指定。返回新行的 id，方便需要时回查。
    pub async fn insert_audit(&self, entry: &AuditEntry) -> Result<i64> {
        self.db
            .insert_returning_id(
                "INSERT INTO audit_logs (actor_id, action, target, detail, ip, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                Args::new()
                    .opt_int(entry.actor_id)
                    .text(&entry.action)
                    .opt_text(entry.target.clone())
                    .text(json_text(&entry.detail)?)
                    .opt_text(entry.ip.clone())
                    .text(now_ts()),
            )
            .await
    }

    /// 审计流水列表，最近的在前。同秒写入的多行用 id 兜底排序，翻页不会串行。
    pub async fn list_audit(
        &self,
        filter: &AuditFilter,
        page: PageRequest,
    ) -> Result<Page<AuditLog>> {
        let (where_sql, args) = audit_where(filter);

        let total = self
            .db
            .count(
                &format!("SELECT COUNT(*) AS total FROM audit_logs{where_sql}"),
                args.clone(),
            )
            .await?;

        let sql = format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_logs{where_sql} \
             ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(page.per_page).int(page.offset()))
            .await?;
        let items = rows
            .iter()
            .map(AuditLog::from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(Page::new(items, total, page))
    }

    /// 某个动作在窗口内发生的次数。`actor_id` 给了就只数这个人，
    /// 管理后台「近 24 小时命中限制」那一列用的就是它。
    pub async fn count_audit_action_since(
        &self,
        actor_id: Option<i64>,
        action: &str,
        since: DateTime<Utc>,
    ) -> Result<i64> {
        let (where_sql, args) = audit_where(&AuditFilter {
            actor_id,
            action: Some(action.to_string()),
            action_prefix: None,
            since: Some(since),
        });
        self.db
            .count(
                &format!("SELECT COUNT(*) AS total FROM audit_logs{where_sql}"),
                args,
            )
            .await
    }

    /// 全站限流命中次数。风控面板判断「今天是不是不太平」的第一个数字。
    pub async fn count_rate_limited_since(&self, since: DateTime<Utc>) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM audit_logs \
                  WHERE action = 'rate_limited' AND created_at >= ?",
                Args::new().text(ts(since)),
            )
            .await
    }

    /// 安全事件按动作汇总，次数多的在前。界面按这个顺序决定先看哪一类。
    pub async fn count_audit_by_action(&self, since: DateTime<Utc>) -> Result<Vec<(String, i64)>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT action, COUNT(*) AS total FROM audit_logs \
                  WHERE created_at >= ? GROUP BY action ORDER BY total DESC, action ASC",
                Args::new().text(ts(since)),
            )
            .await?;
        rows.iter()
            .map(|row| Ok((get_str(row, "action")?, get_i64(row, "total")?)))
            .collect()
    }

    /// 一组 IP 在窗口内某个动作的次数。登录失败连击、同 IP 批量上传都靠它判。
    pub async fn count_action_for_ips(
        &self,
        action: &str,
        ips: &[String],
        since: DateTime<Utc>,
    ) -> Result<i64> {
        if ips.is_empty() {
            return Ok(0);
        }

        let placeholders = vec!["?"; ips.len()].join(", ");
        let mut args = Args::new().text(action).text(ts(since));
        for ip in ips {
            args = args.text(ip);
        }
        self.db
            .count(
                &format!(
                    "SELECT COUNT(*) AS total FROM audit_logs \
                      WHERE action = ? AND created_at >= ? AND ip IN ({placeholders})"
                ),
                args,
            )
            .await
    }

    /// 清理旧流水。保留窗口由服务层的设置项决定，仓储只按时间点删。
    pub async fn purge_audit_before(&self, before: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM audit_logs WHERE created_at < ?",
                Args::new().text(ts(before)),
            )
            .await
    }
}

/// 审计行的列清单。`model.rs` 里只给了 `AuditLog::from_row`，列清单在这里补齐，
/// 免得列表查询写成 `SELECT *`（列顺序变了就解码错位）。
const AUDIT_COLUMNS: &str = "id, actor_id, action, target, detail, ip, created_at";

/// 列表与计数共用的条件拼装。两边共用一个函数，条目数与列表内容才对得上。
fn audit_where(filter: &AuditFilter) -> (String, Args) {
    let mut builder = WhereBuilder::new();

    if let Some(actor_id) = filter.actor_id {
        builder = builder.and("actor_id = ?", move |args| args.int(actor_id));
    }
    if let Some(action) = filter
        .action
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.and("action = ?", |args| args.text(action));
    }
    if let Some(prefix) = filter
        .action_prefix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = prefix_pattern(prefix);
        builder = builder.and("action LIKE ? ESCAPE '\\'", move |args| args.text(pattern));
    }
    if let Some(since) = filter.since {
        builder = builder.and("created_at >= ?", move |args| args.text(ts(since)));
    }

    (builder.sql_string(), builder.args())
}
