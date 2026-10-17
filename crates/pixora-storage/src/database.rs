//! 数据库内存储驱动：字节直接进库。
//!
//! 适合小图或「不想碰文件系统」的场景。代价要说清楚：读一个大图要把它整份
//! 读进内存再发给客户端，所以这条路只适合中小尺寸；`Capabilities::streaming`
//! 声明为支持是因为写侧可以流式收，读侧则受数据库驱动本身限制。
//!
//! 这里刻意不依赖 `pixora-repo`：基础设施层不该反向依赖仓储层，
//! 因此占位符改写这份小逻辑在本文件里自带一份（有单测）。

use async_trait::async_trait;
use sqlx::{AnyPool, Row};
use tokio::io::AsyncRead;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

/// 该驱动要认识的方言。与 `pixora_repo::Dialect` 取值一致，但独立声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Postgres,
    MySql,
    Sqlite,
}

impl SqlDialect {
    /// PostgreSQL 需要 `$1..$n`，其余两种方言用 `?`。
    fn prepare(self, sql: &str) -> String {
        if self != SqlDialect::Postgres {
            return sql.to_string();
        }
        let mut out = String::with_capacity(sql.len() + 8);
        let mut index = 0usize;
        for ch in sql.chars() {
            if ch == '?' {
                index += 1;
                out.push('$');
                out.push_str(&index.to_string());
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// 「插入或覆盖」的写法。
    fn upsert_sql(self) -> &'static str {
        match self {
            SqlDialect::MySql => {
                "INSERT INTO storage_objects (`key`, content, size, updated_at) VALUES (?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE content = VALUES(content), size = VALUES(size), \
                                         updated_at = VALUES(updated_at)"
            }
            _ => {
                "INSERT INTO storage_objects (key, content, size, updated_at) VALUES (?, ?, ?, ?) \
                 ON CONFLICT (key) DO UPDATE SET content = ?, size = ?, updated_at = ?"
            }
        }
    }

    /// `key` 是 MySQL 的保留字，标识符要加反引号；另外两种方言直接写。
    fn key_col(self) -> &'static str {
        match self {
            SqlDialect::MySql => "`key`",
            _ => "key",
        }
    }
}

/// 数据库后端。
pub struct DatabaseDriver {
    pool: AnyPool,
    dialect: SqlDialect,
    instance: String,
}

impl DatabaseDriver {
    pub fn new(pool: AnyPool, dialect: SqlDialect) -> Self {
        Self {
            pool,
            dialect,
            instance: crate::names::DATABASE.to_string(),
        }
    }

    /// 挂多个数据库后端时区分实例标识。
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    fn error(context: &str, err: sqlx::Error) -> StorageError {
        StorageError::Database(format!("{context}：{err}"))
    }
}

#[async_trait]
impl StorageDriver for DatabaseDriver {
    fn name(&self) -> &'static str {
        crate::names::DATABASE
    }

    fn instance_key(&self) -> String {
        self.instance.clone()
    }

    async fn put(
        &self,
        key: &str,
        mut reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        _mime: &str,
    ) -> Result<StoredObject> {
        use tokio::io::AsyncReadExt;

        // 写侧流式收进内存：一张图几十兆以内是可控的，而且不少数据库驱动
        // 对「边收边写 BLOB」的支持并不一致，统一成「收完再写」更可预期。
        let mut content = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut content).await?;
        if content.len() as u64 != size {
            return Err(StorageError::Malformed {
                driver: crate::names::DATABASE,
                message: format!("接收字节数 {} 与声明的 {size} 不一致", content.len()),
            });
        }

        let sql = self.dialect.prepare(self.dialect.upsert_sql());
        let updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let mut query = sqlx::query(&sql)
            .bind(key)
            .bind(&content)
            .bind(size as i64)
            .bind(&updated_at);
        if self.dialect != SqlDialect::MySql {
            query = query
                .bind(&content)
                .bind(size as i64)
                .bind(&updated_at);
        }
        query
            .execute(&self.pool)
            .await
            .map_err(|err| Self::error("写入对象", err))?;

        Ok(StoredObject {
            key: key.to_string(),
            size,
            driver: crate::names::DATABASE,
        })
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let sql = self.dialect.prepare(&format!(
            "SELECT content FROM storage_objects WHERE {} = ?",
            self.dialect.key_col()
        ));
        let row = sqlx::query(&sql)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| Self::error("读取对象", err))?;
        let Some(row) = row else {
            return Err(StorageError::NotFound(key.to_string()));
        };
        let content: Vec<u8> = row
            .try_get("content")
            .map_err(|err| StorageError::Database(format!("解析对象内容：{err}")))?;
        Ok(Box::new(std::io::Cursor::new(content)))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let sql = self.dialect.prepare(&format!(
            "DELETE FROM storage_objects WHERE {} = ?",
            self.dialect.key_col()
        ));
        sqlx::query(&sql)
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|err| Self::error("删除对象", err))?;
        Ok(())
    }

    async fn stat(&self, key: &str) -> Result<ObjectInfo> {
        let sql = self.dialect.prepare(&format!(
            "SELECT size, updated_at, LENGTH(content) AS verify_len FROM storage_objects WHERE {} = ?",
            self.dialect.key_col()
        ));
        let row = sqlx::query(&sql)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| Self::error("查询对象", err))?;
        let Some(row) = row else {
            return Err(StorageError::NotFound(key.to_string()));
        };
        let size: i64 = row
            .try_get("size")
            .map_err(|err| StorageError::Database(format!("解析对象大小：{err}")))?;
        let updated: Option<String> = row.try_get("updated_at").ok();
        Ok(ObjectInfo {
            key: key.to_string(),
            size: size.max(0) as u64,
            modified: updated.and_then(|raw| chrono::DateTime::parse_from_rfc3339(&raw).ok())
                .map(|value| value.with_timezone(&chrono::Utc)),
            content_type: None,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::blob_backed()
    }

    async fn health(&self) -> Result<String> {
        let sql = self.dialect.prepare("SELECT COUNT(*) AS total FROM storage_objects");
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|err| Self::error("连通性检查", err))?;
        let total: i64 = row.try_get("total").unwrap_or(0);
        Ok(format!("连通，库内已有 {total} 个对象"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_placeholders_are_renumbered() {
        let sql = SqlDialect::Postgres.prepare("SELECT * FROM t WHERE a = ? AND b = ?");
        assert_eq!(sql, "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn other_dialects_keep_question_marks() {
        let sql = "SELECT * FROM t WHERE a = ?";
        assert_eq!(SqlDialect::Sqlite.prepare(sql), sql);
        assert_eq!(SqlDialect::MySql.prepare(sql), sql);
    }

    #[test]
    fn upsert_variants_differ_by_dialect() {
        assert!(SqlDialect::MySql.upsert_sql().contains("ON DUPLICATE KEY UPDATE"));
        assert!(SqlDialect::Postgres.upsert_sql().contains("ON CONFLICT"));
        assert!(SqlDialect::Sqlite.upsert_sql().contains("ON CONFLICT"));
    }
}
