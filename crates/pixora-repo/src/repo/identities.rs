//! 账号的第三方登录方式（GitHub / Google / QQ / 微信 / 手机号）。
//!
//! 邮箱不在这张表里：它是账号本身（`users.email`），必需、不可解绑。这张表只放
//! 「额外挂了哪些门进来」，`(provider, subject)` 唯一——一个第三方身份只能挂在
//! 一个账号上，否则「用 GitHub 登录」这件事就有歧义了。

use chrono::{DateTime, Utc};
use crate::db::{AnyRow, Db};
use crate::model::{get_i64, get_opt_str, get_str, get_ts, now_ts};
use crate::{Args, RepoError, Result};

/// 支持的提供方。与前端按钮、系统设置里的键一一对应。
pub mod provider {
    pub const GITHUB: &str = "github";
    pub const GOOGLE: &str = "google";
    pub const QQ: &str = "qq";
    pub const WECHAT: &str = "wechat";
    pub const PHONE: &str = "phone";

    /// 全部提供方，顺序就是界面上按钮的顺序。
    pub const ALL: [&str; 5] = [GITHUB, GOOGLE, QQ, WECHAT, PHONE];

    /// 是不是认识的提供方。写接口一律先过这一道，免得库里混进拼错的字符串。
    pub fn is_known(value: &str) -> bool {
        ALL.contains(&value)
    }

    /// 界面上怎么称呼它。
    pub fn label(value: &str) -> &'static str {
        match value {
            GITHUB => "GitHub",
            GOOGLE => "Google",
            QQ => "QQ",
            WECHAT => "微信",
            PHONE => "手机号",
            _ => "未知方式",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UserIdentity {
    pub id: i64,
    pub user_id: i64,
    pub provider: String,
    pub subject: String,
    pub display: Option<String>,
    pub meta: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl UserIdentity {
    const COLUMNS: &'static str = "id, user_id, provider, subject, display, meta, created_at, updated_at";

    fn from_row(row: &AnyRow) -> Result<Self> {
        Ok(Self {
            id: get_i64(row, "id")?,
            user_id: get_i64(row, "user_id")?,
            provider: get_str(row, "provider")?,
            subject: get_str(row, "subject")?,
            display: get_opt_str(row, "display")?,
            meta: get_opt_str(row, "meta")?,
            created_at: get_ts(row, "created_at")?,
            updated_at: get_ts(row, "updated_at")?,
        })
    }
}

impl Db {
    /// 挂一个第三方身份。已经挂在别人身上时返回唯一约束冲突，由服务层翻译。
    pub async fn insert_identity(
        &self,
        user_id: i64,
        provider: &str,
        subject: &str,
        display: Option<&str>,
        meta: Option<&str>,
    ) -> Result<UserIdentity> {
        let stamp = now_ts();
        self.execute(
            "INSERT INTO user_identities (user_id, provider, subject, display, meta, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            Args::new()
                .int(user_id)
                .text(provider)
                .text(subject)
                .opt_text(display.map(str::to_string))
                .opt_text(meta.map(str::to_string))
                .text(&stamp)
                .text(&stamp),
        )
        .await?;
        let row = self
            .fetch_optional(
                "SELECT * FROM user_identities WHERE provider = ? AND subject = ?",
                Args::new().text(provider).text(subject),
            )
            .await?;
        match row {
            Some(row) => UserIdentity::from_row(&row),
            None => Err(RepoError::NotFound(format!(
                "identity {provider}/{subject}"
            ))),
        }
    }

    /// 某个第三方身份挂在哪个账号上。
    pub async fn find_identity(&self, provider: &str, subject: &str) -> Result<Option<UserIdentity>> {
        let row = self
            .fetch_optional(
                "SELECT * FROM user_identities WHERE provider = ? AND subject = ?",
                Args::new().text(provider).text(subject),
            )
            .await?;
        row.as_ref().map(UserIdentity::from_row).transpose()
    }

    /// 一个账号挂着的全部第三方身份。
    pub async fn list_identities(&self, user_id: i64) -> Result<Vec<UserIdentity>> {
        let rows = self
            .fetch_all(
                &format!(
                    "SELECT {cols} FROM user_identities WHERE user_id = ? ORDER BY id",
                    cols = UserIdentity::COLUMNS
                ),
                Args::new().int(user_id),
            )
            .await?;
        rows.iter().map(UserIdentity::from_row).collect()
    }

    /// 一个账号挂着的某个提供方（每类最多一条）。
    pub async fn find_user_identity(
        &self,
        user_id: i64,
        provider: &str,
    ) -> Result<Option<UserIdentity>> {
        let row = self
            .fetch_optional(
                "SELECT * FROM user_identities WHERE user_id = ? AND provider = ?",
                Args::new().int(user_id).text(provider),
            )
            .await?;
        row.as_ref().map(UserIdentity::from_row).transpose()
    }

    /// 解绑。返回删掉的行数（0 表示本来就没绑）。
    pub async fn delete_identity(&self, user_id: i64, provider: &str) -> Result<u64> {
        Ok(self
            .execute(
                "DELETE FROM user_identities WHERE user_id = ? AND provider = ?",
                Args::new().int(user_id).text(provider),
            )
            .await?)
    }

    /// 改展示名（昵称、掩码手机号会随对方平台变化）。
    pub async fn update_identity_display(
        &self,
        id: i64,
        display: Option<&str>,
        meta: Option<&str>,
    ) -> Result<()> {
        self.execute(
            "UPDATE user_identities SET display = ?, meta = ?, updated_at = ? WHERE id = ?",
            Args::new()
                .opt_text(display.map(str::to_string))
                .opt_text(meta.map(str::to_string))
                .text(&now_ts())
                .int(id),
        )
        .await?;
        Ok(())
    }

    /// 某个账号挂了几种登录方式（第三方身份数，不含邮箱与密码）。
    pub async fn count_identities(&self, user_id: i64) -> Result<i64> {
        self.count(
            "SELECT COUNT(*) AS total FROM user_identities WHERE user_id = ?",
            Args::new().int(user_id),
        )
        .await
    }

    /// 一次查一批账号的身份（管理端用户详情、列表用）。
    pub async fn identities_for_users(
        &self,
        user_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<UserIdentity>>> {
        let mut out: std::collections::HashMap<i64, Vec<UserIdentity>> = std::collections::HashMap::new();
        if user_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; user_ids.len()].join(", ");
        let mut args = Args::new();
        for id in user_ids {
            args = args.int(*id);
        }
        let rows = self
            .fetch_all(
                &format!(
                    "SELECT {cols} FROM user_identities WHERE user_id IN ({placeholders}) ORDER BY id",
                    cols = UserIdentity::COLUMNS
                ),
                args,
            )
            .await?;
        for row in &rows {
            let identity = UserIdentity::from_row(row)?;
            out.entry(identity.user_id).or_default().push(identity);
        }
        Ok(out)
    }
}
