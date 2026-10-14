//! 邮箱验证码仓储。
//!
//! 库里只存验证码的哈希。三个用途共用一张表，用 `purpose` 区分：
//! `register`（注册）、`reset`（找回密码）、`change_email`（换绑邮箱）。
//!
//! 频率限制靠 `count_recent_requests` 查「同一邮箱 / 同一 IP 在窗口内发过几次」。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::{RepoError, Result};
use crate::model::*;
use crate::repo::Repo;

/// 验证码用途。
pub mod purpose {
    pub const REGISTER: &str = "register";
    pub const RESET: &str = "reset";
    pub const CHANGE_EMAIL: &str = "change_email";
}

impl Repo {
    /// 发一个验证码。同一邮箱 + 用途的旧记录不删，靠 [`Repo::find_latest_verification`]
    /// 取最新那条——「重发」时旧码是否立刻失效由服务层决定，仓储不替它做这个判断。
    pub async fn create_email_verification(
        &self,
        email: &str,
        code_hash: &str,
        purpose: &str,
        expires_at: DateTime<Utc>,
        request_ip: Option<&str>,
    ) -> Result<EmailVerification> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO email_verifications (email, code_hash, purpose, expires_at, \
                                                  consumed_at, attempts, request_ip, created_at) \
                 VALUES (?, ?, ?, ?, NULL, 0, ?, ?)",
                Args::new()
                    .text(email)
                    .text(code_hash)
                    .text(purpose)
                    .text(ts(expires_at))
                    .opt_text(request_ip.map(str::to_string))
                    .text(now_ts()),
            )
            .await?;

        self.find_verification(id)
            .await?
            .ok_or_else(|| RepoError::NotFound(format!("新建验证码 {id}")))
    }

    /// 取最新一条验证码。`id` 参与排序是为了同一微秒内发出的两条也有确定顺序。
    pub async fn find_latest_verification(
        &self,
        email: &str,
        purpose: &str,
    ) -> Result<Option<EmailVerification>> {
        let sql = format!(
            "SELECT {} FROM email_verifications WHERE email = ? AND purpose = ? \
             ORDER BY created_at DESC, id DESC LIMIT 1",
            EmailVerification::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(email).text(purpose))
            .await?;
        row.as_ref().map(EmailVerification::from_row).transpose()
    }

    pub async fn find_verification(&self, id: i64) -> Result<Option<EmailVerification>> {
        let sql = format!(
            "SELECT {} FROM email_verifications WHERE id = ?",
            EmailVerification::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(EmailVerification::from_row).transpose()
    }

    /// 标记已使用。只写第一次消费时间：并发的两次校验请求里，
    /// 后到的那个改不动行，据此也能知道自己来晚了。
    pub async fn consume_verification(&self, id: i64, at: DateTime<Utc>) -> Result<()> {
        self.db
            .execute(
                "UPDATE email_verifications SET consumed_at = ? WHERE id = ? AND consumed_at IS NULL",
                Args::new().text(ts(at)).int(id),
            )
            .await?;
        Ok(())
    }

    /// 一次校验失败：累加尝试次数并返回累加后的值，服务层据此决定是否作废。
    /// 次数以库里的值为准，不用「读出来 + 1 再写回」的写法，避免并发下丢计数。
    pub async fn increment_verification_attempts(&self, id: i64) -> Result<i32> {
        self.db
            .execute(
                "UPDATE email_verifications SET attempts = attempts + 1 WHERE id = ?",
                Args::new().int(id),
            )
            .await?;

        let row = self
            .db
            .fetch_one(
                "SELECT attempts FROM email_verifications WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        get_i32(&row, "attempts")
    }

    /// 同一邮箱 + 用途在窗口内发过几次。注册验证码的「每分钟一次」靠它。
    pub async fn count_recent_verifications_for_email(
        &self,
        email: &str,
        purpose: &str,
        since: DateTime<Utc>,
    ) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM email_verifications \
                  WHERE email = ? AND purpose = ? AND created_at >= ?",
                Args::new().text(email).text(purpose).text(ts(since)),
            )
            .await
    }

    /// 同一 IP 在窗口内发过几次。跨用途统计——换邮箱绕过限流也要拦。
    pub async fn count_recent_verifications_for_ip(
        &self,
        ip: &str,
        since: DateTime<Utc>,
    ) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM email_verifications \
                  WHERE request_ip = ? AND created_at >= ?",
                Args::new().text(ip).text(ts(since)),
            )
            .await
    }

    /// 清理过期的验证码行。过期的码在服务层本就不可用，这里只是不让表无限膨胀。
    pub async fn purge_expired_verifications(&self, before: DateTime<Utc>) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM email_verifications WHERE expires_at < ?",
                Args::new().text(ts(before)),
            )
            .await
    }
}
