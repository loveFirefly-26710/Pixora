//! API 令牌仓储。
//!
//! 库里只存哈希（`token_hash`）与一个展示用的片段（`display`，形如 `px_live_9f2a…`），
//! 明文只在签发那一刻返回一次。

use chrono::{DateTime, Utc};

use crate::args::Args;
use crate::error::{RepoError, Result};
use crate::model::*;
use crate::repo::Repo;

/// 新建令牌的入参。`token_hash` 是明文过 SHA-256 后的结果。
#[derive(Debug, Clone)]
pub struct NewApiToken {
    pub user_id: i64,
    pub name: String,
    pub token_hash: String,
    /// 展示片段，例如 `px_live_9f2a7c41`
    pub display: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl Repo {
    /// 签发令牌。`expires_at` 传 `None` 即长期有效。
    pub async fn create_api_token(&self, new: &NewApiToken) -> Result<ApiToken> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO api_tokens (user_id, name, token_hash, display, scopes, last_used_at, \
                                         expires_at, revoked_at, created_at) \
                 VALUES (?, ?, ?, ?, ?, NULL, ?, NULL, ?)",
                Args::new()
                    .int(new.user_id)
                    .text(&new.name)
                    .text(&new.token_hash)
                    .text(&new.display)
                    .text(json_text(&new.scopes)?)
                    .opt_text(new.expires_at.map(ts))
                    .text(now_ts()),
            )
            .await?;

        let sql = format!("SELECT {} FROM api_tokens WHERE id = ?", ApiToken::COLUMNS);
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref()
            .map(ApiToken::from_row)
            .transpose()?
            .ok_or_else(|| RepoError::NotFound(format!("新建令牌 {id}")))
    }

    /// 按哈希找令牌（鉴权中间件每个请求都走这里）。
    ///
    /// 不在查询里过滤撤销与过期：把判据留给 [`ApiToken::is_usable`]，
    /// 调用方才能区分「查无此令牌」和「令牌已撤销」，给出不同的提示与审计记录。
    pub async fn find_api_token_by_hash(&self, token_hash: &str) -> Result<Option<ApiToken>> {
        let sql = format!(
            "SELECT {} FROM api_tokens WHERE token_hash = ?",
            ApiToken::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(token_hash))
            .await?;
        row.as_ref().map(ApiToken::from_row).transpose()
    }

    /// 用户的令牌列表。含已撤销的行——安全页要展示「哪个令牌什么时候被撤的」。
    pub async fn list_api_tokens(&self, user_id: i64) -> Result<Vec<ApiToken>> {
        let sql = format!(
            "SELECT {} FROM api_tokens WHERE user_id = ? ORDER BY created_at DESC, id DESC",
            ApiToken::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new().int(user_id)).await?;
        rows.iter().map(ApiToken::from_row).collect()
    }

    /// 撤销一个令牌。带上 `user_id` 是为了防止拿别人的令牌 id 越权撤销。
    /// 只写 `revoked_at` 留痕，不物理删除。
    pub async fn revoke_api_token(&self, id: i64, user_id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE api_tokens SET revoked_at = ? \
                  WHERE id = ? AND user_id = ? AND revoked_at IS NULL",
                Args::new().text(now_ts()).int(id).int(user_id),
            )
            .await?;
        Ok(())
    }

    /// 记一次使用时间。鉴权通过后异步刷新，失败不影响本次请求。
    pub async fn touch_api_token(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "UPDATE api_tokens SET last_used_at = ? WHERE id = ?",
                Args::new().text(now_ts()).int(id),
            )
            .await?;
        Ok(())
    }

    /// 有效令牌数（上限校验用）。已撤销的不计；到期与否由服务层另行判断。
    pub async fn count_api_tokens(&self, user_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM api_tokens WHERE user_id = ? AND revoked_at IS NULL",
                Args::new().int(user_id),
            )
            .await
    }

    /// 一次撤销该用户的全部有效令牌。密码泄漏、「下线所有令牌」走这里。
    pub async fn revoke_all_api_tokens(&self, user_id: i64) -> Result<u64> {
        self.db
            .execute(
                "UPDATE api_tokens SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
                Args::new().text(now_ts()).int(user_id),
            )
            .await
    }
}
