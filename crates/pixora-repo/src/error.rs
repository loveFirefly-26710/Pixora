use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("数据库错误：{0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("迁移失败：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("数据解码失败（{column}）：{reason}")]
    Decode { column: String, reason: String },

    #[error("配置错误：{0}")]
    Config(String),

    #[error("记录不存在：{0}")]
    NotFound(String),

    #[error("唯一约束冲突：{0}")]
    Conflict(String),

    #[error("数据格式错误：{0}")]
    Format(String),
}

impl RepoError {
    pub fn decode(column: impl Into<String>, reason: impl std::fmt::Display) -> Self {
        Self::Decode {
            column: column.into(),
            reason: reason.to_string(),
        }
    }

    /// 判断一个底层错误是不是唯一约束冲突。三种方言的错误码各不相同，
    /// 这里统一收口，调用方（比如 UID 重试、邮箱查重）不必认识各家的码。
    pub fn is_unique_violation(&self) -> bool {
        match self {
            RepoError::Sqlx(sqlx::Error::Database(db)) => {
                // PostgreSQL 23505 / MySQL 1062 / SQLite 2067（UNIQUE）
                let code = db.code().map(|c| c.to_string()).unwrap_or_default();
                code == "23505"
                    || code == "1062"
                    || code == "2067"
                    || code == "1555"
                    || db.message().contains("UNIQUE constraint failed")
            }
            _ => false,
        }
    }

    /// 是不是「表不存在」这类尚未建库的错误，安装向导据此判断要不要先跑迁移。
    pub fn is_missing_schema(&self) -> bool {
        match self {
            RepoError::Sqlx(sqlx::Error::Database(db)) => {
                let code = db.code().map(|c| c.to_string()).unwrap_or_default();
                let msg = db.message().to_ascii_lowercase();
                // PG 42P01 / MySQL 1146 / SQLite "no such table"
                code == "42P01"
                    || code == "1146"
                    || msg.contains("no such table")
                    || msg.contains("doesn't exist")
            }
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, RepoError>;

/// sqlx 的错误在「查不到行」时也会返回，调用方通常要的是 `Option`。
pub fn is_row_not_found(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::RowNotFound)
}
