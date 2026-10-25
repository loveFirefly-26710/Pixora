//! 公告已读记录。
//!
//! 公告本身仍存在设置里（只有一条当前公告），这里只解决「谁读过哪个版本」：
//! 版本由标题与正文算出，正文一改就是新版本，旧记录自然不再命中。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::Result;
use crate::repo::Repo;

impl Repo {
    /// 标记某个账号读过某个公告版本。重复确认是幂等的。
    pub async fn mark_announcement_read(
        &self,
        user_id: i64,
        version: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO announcement_reads (user_id, version, read_at) VALUES (?, ?, ?) \
             ON CONFLICT (user_id, version) DO UPDATE SET read_at = EXCLUDED.read_at",
            "INSERT INTO announcement_reads (user_id, version, read_at) VALUES (?, ?, ?) \
             ON DUPLICATE KEY UPDATE read_at = VALUES(read_at)",
            "INSERT INTO announcement_reads (user_id, version, read_at) VALUES (?, ?, ?) \
             ON CONFLICT (user_id, version) DO UPDATE SET read_at = excluded.read_at",
        );
        self.db
            .execute(
                &sql,
                Args::new()
                    .int(user_id)
                    .text(version)
                    .text(at.to_rfc3339()),
            )
            .await?;
        Ok(())
    }

    pub async fn announcement_read_by(&self, user_id: i64, version: &str) -> Result<bool> {
        let total = self
            .db
            .count(
                "SELECT COUNT(*) AS total FROM announcement_reads \
                  WHERE user_id = ? AND version = ?",
                Args::new().int(user_id).text(version),
            )
            .await?;
        Ok(total > 0)
    }

    /// 这个版本有多少账号确认过。游客没有账号，不计入分母。
    pub async fn announcement_read_count(&self, version: &str) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM announcement_reads WHERE version = ?",
                Args::new().text(version),
            )
            .await
    }
}
