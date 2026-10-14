//! 邮件相关仓储：发件箱、邮件模板、一次性邮箱域名清单。
//!
//! 发件箱的存在意义是「投递失败可重试」：验证码、暂停通知这类邮件一旦丢了
//! 用户就得再等一次，所以先落库再投递，失败按 `next_attempt_at` 退避重试。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::model::*;
use crate::repo::Repo;

/// 发件箱状态。投递成功前一直是 `queued`，失败重试不换状态——
/// 重试节奏由 `next_attempt_at` 表达，状态只在终结时改变。
mod mail_status {
    pub const QUEUED: &str = "queued";
    pub const SENT: &str = "sent";
    pub const FAILED: &str = "failed";
}

/// 一次批量插入的行数上限。避开各家数据库的参数个数上限（3 列 × 200 行）。
const DOMAIN_INSERT_CHUNK: usize = 200;

impl Repo {
    // ---------- 发件箱 ----------

    /// 入队一封邮件。先落库再投递：投递环节崩溃时这封信还在，重试即可，
    /// 验证码这类「丢了用户就得再等一次」的邮件因此不会凭空消失。
    pub async fn enqueue_mail(
        &self,
        to_email: &str,
        template: &str,
        vars: &serde_json::Value,
    ) -> Result<i64> {
        let now = now_ts();
        self.db
            .insert_returning_id(
                "INSERT INTO mail_outbox (to_email, template_name, vars, status, attempts, error, \
                                          next_attempt_at, created_at, sent_at) \
                 VALUES (?, ?, ?, ?, 0, NULL, ?, ?, NULL)",
                Args::new()
                    .text(to_email)
                    .text(template)
                    .text(json_text(vars)?)
                    .text(mail_status::QUEUED)
                    .text(&now)
                    .text(&now),
            )
            .await
    }

    pub async fn find_mail(&self, id: i64) -> Result<Option<MailOutboxItem>> {
        let sql = format!("SELECT {} FROM mail_outbox WHERE id = ?", MailOutboxItem::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(MailOutboxItem::from_row).transpose()
    }

    /// 到点该发的信：状态还是 `queued`，且要么没有下次尝试时间，要么已经到点。
    /// 按 id 升序保证先入先发，重试的信不会插到队头。
    pub async fn due_mails(&self, at: DateTime<Utc>, limit: i64) -> Result<Vec<MailOutboxItem>> {
        let sql = format!(
            "SELECT {} FROM mail_outbox WHERE status = ? \
                AND (next_attempt_at IS NULL OR next_attempt_at <= ?) \
             ORDER BY id ASC LIMIT ?",
            MailOutboxItem::COLUMNS
        );
        let rows = self
            .db
            .fetch_all(
                &sql,
                Args::new()
                    .text(mail_status::QUEUED)
                    .text(ts(at))
                    .int(limit.clamp(1, 500)),
            )
            .await?;
        rows.iter().map(MailOutboxItem::from_row).collect()
    }

    pub async fn mark_mail_sent(&self, id: i64, at: DateTime<Utc>) -> Result<()> {
        self.db
            .execute(
                "UPDATE mail_outbox SET status = ?, sent_at = ?, error = NULL WHERE id = ?",
                Args::new().text(mail_status::SENT).text(ts(at)).int(id),
            )
            .await?;
        Ok(())
    }

    /// 一次投递失败。`attempts` 加一，错误原因留档。
    ///
    /// `permanent = true` 用于「重试也没用」的失败（地址不存在、被拒收）：
    /// 状态直接置 `failed` 并清空下次尝试时间，不再出队。
    /// 否则保持 `queued`，把 `next_attempt_at` 推到退避后的时间点等下一次投递。
    pub async fn mark_mail_failed(
        &self,
        id: i64,
        error: &str,
        next_attempt_at: Option<DateTime<Utc>>,
        permanent: bool,
    ) -> Result<()> {
        if permanent {
            self.db
                .execute(
                    "UPDATE mail_outbox SET status = ?, attempts = attempts + 1, error = ?, \
                                            next_attempt_at = NULL WHERE id = ?",
                    Args::new()
                        .text(mail_status::FAILED)
                        .text(error)
                        .int(id),
                )
                .await?;
        } else {
            self.db
                .execute(
                    "UPDATE mail_outbox SET attempts = attempts + 1, error = ?, next_attempt_at = ? \
                      WHERE id = ?",
                    Args::new()
                        .text(error)
                        .opt_text(next_attempt_at.map(ts))
                        .int(id),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn count_pending_mail(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM mail_outbox WHERE status = ?",
                Args::new().text(mail_status::QUEUED),
            )
            .await
    }

    /// 清理已发出的信，保留最近一段时间的记录方便排查。
    pub async fn purge_sent_mail(&self, before: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM mail_outbox WHERE status = ? AND sent_at IS NOT NULL AND sent_at < ?",
                Args::new().text(mail_status::SENT).text(ts(before)),
            )
            .await
    }

    // ---------- 模板 ----------

    pub async fn find_email_template(&self, name: &str) -> Result<Option<EmailTemplate>> {
        let row = self
            .db
            .fetch_optional(
                "SELECT name, subject, body, updated_at FROM email_templates WHERE name = ?",
                Args::new().text(name),
            )
            .await?;
        row.as_ref().map(EmailTemplate::from_row).transpose()
    }

    pub async fn list_email_templates(&self) -> Result<Vec<EmailTemplate>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT name, subject, body, updated_at FROM email_templates ORDER BY name ASC",
                Args::new(),
            )
            .await?;
        rows.iter().map(EmailTemplate::from_row).collect()
    }

    /// 写模板：有则覆盖、无则新建。
    ///
    /// 「按主键 upsert」是少数必须写三份方言的语句（同 `settings.rs` 的 `set_setting`）：
    /// PG / SQLite 走 `ON CONFLICT ... DO UPDATE` 并把参数再写一遍，MySQL 走
    /// `ON DUPLICATE KEY UPDATE ... VALUES(...)` 只用一份参数。
    pub async fn upsert_email_template(
        &self,
        name: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO email_templates (name, subject, body, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET subject = ?, body = ?, updated_at = ?",
            "INSERT INTO email_templates (name, subject, body, updated_at) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE subject = VALUES(subject), body = VALUES(body), updated_at = VALUES(updated_at)",
            "INSERT INTO email_templates (name, subject, body, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET subject = ?, body = ?, updated_at = ?",
        );
        let now = now_ts();
        let args = match self.db.dialect() {
            crate::db::Dialect::MySql => Args::new()
                .text(name)
                .text(subject)
                .text(body)
                .text(&now),
            _ => Args::new()
                .text(name)
                .text(subject)
                .text(body)
                .text(&now)
                .text(subject)
                .text(body)
                .text(&now),
        };
        self.db.execute(&sql, args).await?;
        Ok(())
    }

    // ---------- 一次性邮箱域名 ----------

    pub async fn disposable_domain_exists(&self, domain: &str) -> Result<bool> {
        self.db
            .exists(
                "SELECT 1 AS hit FROM disposable_domains WHERE domain = ?",
                Args::new().text(domain),
            )
            .await
    }

    /// 批量导入域名清单。返回真正新增的行数。
    ///
    /// 幂等：重复导入同一份清单不会报错也不会重复计数，撞主键的行直接跳过。
    /// 先分块再拼多值 VALUES，比逐行插入少几个数量级的往返。
    pub async fn insert_disposable_domains(&self, domains: &[String], source: &str) -> Result<u64> {
        // 先归一化：去空白、丢空串、同一批里去重，避免同一语句里出现重复主键
        let mut seen = std::collections::HashSet::new();
        let normalized: Vec<&str> = domains
            .iter()
            .map(|domain| domain.trim())
            .filter(|domain| !domain.is_empty())
            .filter(|domain| seen.insert(domain.to_string()))
            .collect();
        if normalized.is_empty() {
            return Ok(0);
        }

        let now = now_ts();
        let mut inserted = 0u64;
        for chunk in normalized.chunks(DOMAIN_INSERT_CHUNK) {
            // 每行三个占位符，拼成一整条多行 VALUES
            let values_clause = vec!["(?, ?, ?)"; chunk.len()].join(", ");
            let sql = self.db.sql_v(
                &format!(
                    "INSERT INTO disposable_domains (domain, source, updated_at) \
                     VALUES {values_clause} ON CONFLICT DO NOTHING"
                ),
                &format!(
                    "INSERT IGNORE INTO disposable_domains (domain, source, updated_at) \
                     VALUES {values_clause}"
                ),
                &format!(
                    "INSERT INTO disposable_domains (domain, source, updated_at) \
                     VALUES {values_clause} ON CONFLICT DO NOTHING"
                ),
            );
            let mut args = Args::new();
            for domain in chunk {
                args = args.text(*domain).text(source).text(&now);
            }
            inserted += self.db.execute(&sql, args).await?;
        }
        Ok(inserted)
    }

    pub async fn count_disposable_domains(&self) -> Result<i64> {
        self.db
            .count("SELECT COUNT(*) AS total FROM disposable_domains", Args::new())
            .await
    }
}
