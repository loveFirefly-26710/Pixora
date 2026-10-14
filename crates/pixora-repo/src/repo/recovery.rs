//! 二次验证的恢复码仓储。
//!
//! 恢复码是一次性的：验证器 App 丢了、手机换了，用户还能靠它登进来。
//! 库里只存哈希——恢复码的熵比口令低（8 位可读字符），明文存库等于把
//! 二次验证的强度降回「知道数据库内容就能过」。

use crate::args::Args;
use crate::error::Result;
use crate::model::{get_i64, get_str, now_ts};
use crate::repo::Repo;

impl Repo {
    /// 重新生成：先清掉旧码，再写一批新的。哈希由调用方算好。
    pub async fn replace_recovery_codes(&self, user_id: i64, hashes: &[String]) -> Result<()> {
        let mut tx = self.db().pool().begin().await?;
        sqlx::query(&self.db().sql("DELETE FROM recovery_codes WHERE user_id = ?"))
            .bind(user_id)
            .execute(&mut *tx)
            .await?;

        let created_at = now_ts();
        for hash in hashes {
            sqlx::query(
                &self
                    .db()
                    .sql("INSERT INTO recovery_codes (user_id, code_hash, used_at, created_at) VALUES (?, ?, NULL, ?)"),
            )
            .bind(user_id)
            .bind(hash)
            .bind(&created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 未使用的恢复码哈希。命中哪一条由调用方逐条比对。
    pub async fn list_unused_recovery_codes(&self, user_id: i64) -> Result<Vec<(i64, String)>> {
        let rows = self
            .db()
            .fetch_all(
                "SELECT id, code_hash FROM recovery_codes WHERE user_id = ? AND used_at IS NULL",
                Args::new().int(user_id),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    get_i64(row, "id")?,
                    get_str(row, "code_hash")?,
                ))
            })
            .collect()
    }

    /// 标记为已使用。带上 `used_at IS NULL` 条件，并发下只会成功一次。
    pub async fn consume_recovery_code(&self, id: i64) -> Result<u64> {
        Ok(self
            .db()
            .execute(
                "UPDATE recovery_codes SET used_at = ? WHERE id = ? AND used_at IS NULL",
                Args::new().text(now_ts()).int(id),
            )
            .await?)
    }

    pub async fn count_unused_recovery_codes(&self, user_id: i64) -> Result<i64> {
        Ok(self
            .db()
            .count(
                "SELECT COUNT(*) AS total FROM recovery_codes WHERE user_id = ? AND used_at IS NULL",
                Args::new().int(user_id),
            )
            .await?)
    }
}
