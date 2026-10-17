//! 连接、方言与 SQL 文本准备。
//!
//! 所有入口查询都经过 [`Db::sql`] 或 [`Db::sql_v`]，这是占位符与方言差异
//! 唯一被处理的地方。

use std::path::Path;
use std::time::Duration;

pub use sqlx::any::AnyRow;
use sqlx::any::{install_default_drivers, AnyPoolOptions};
use sqlx::AnyPool;

use crate::args::Args;
use crate::error::{RepoError, Result};

/// 数据库方言。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
}

impl Dialect {
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::MySql => "mysql",
            Dialect::Sqlite => "sqlite",
        }
    }

    /// 用 `substr` 从 RFC3339 文本里取日期（YYYY-MM-DD）或小时（YYYY-MM-DDTHH）。
    /// 三种方言对 `substr` 的支持一致，统计页的分桶因此只有一份实现。
    pub fn date_expr(self, column: &str) -> String {
        format!("substr({column}, 1, 10)")
    }

    pub fn hour_expr(self, column: &str) -> String {
        format!("substr({column}, 1, 13)")
    }

    /// 求和并且保证结果读得回来整数。
    ///
    /// PostgreSQL 的 `SUM(bigint)` 返回 NUMERIC、MySQL 的 `SUM` 返回 DECIMAL，
    /// `sqlx::Any` 都解不成 i64（报 `Any driver does not support ... Numeric`）；
    /// 包一层 CAST 回到整数。SQLite 的 SUM 本来就是整数，不加料。
    pub fn sum_i64(self, expr: &str) -> String {
        match self {
            Dialect::Postgres => format!("CAST(COALESCE(SUM({expr}), 0) AS BIGINT)"),
            Dialect::MySql => format!("CAST(COALESCE(SUM({expr}), 0) AS SIGNED)"),
            Dialect::Sqlite => format!("COALESCE(SUM({expr}), 0)"),
        }
    }
}

/// 环境变量里写的驱动名。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DbDriver {
    Postgres,
    MySql,
    Sqlite,
}

impl DbDriver {
    pub fn parse(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlite" | "sqlite3" => Ok(Self::Sqlite),
            other => Err(RepoError::Config(format!(
                "不支持的数据库驱动 {other}，可选 postgres / mysql / sqlite"
            ))),
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            Self::Postgres => Dialect::Postgres,
            Self::MySql => Dialect::MySql,
            Self::Sqlite => Dialect::Sqlite,
        }
    }

    /// 从连接串前缀推断驱动。DSN 优先于环境变量时用得上。
    pub fn sniff(dsn: &str) -> Option<Self> {
        let prefix = dsn.split("://").next()?.to_ascii_lowercase();
        match prefix.as_str() {
            "postgres" | "postgresql" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::MySql),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }
}

/// 数据库句柄。克隆代价只有一次 `AnyPool` 的 Arc 拷贝。
#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
    dialect: Dialect,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("dialect", &self.dialect)
            .finish_non_exhaustive()
    }
}

impl Db {
    /// 连接数据库。SQLite 的每条连接都会补上必要的 pragma。
    pub async fn connect(driver: DbDriver, dsn: &str, max_connections: u32) -> Result<Self> {
        install_default_drivers();
        let dsn = normalize_dsn(driver, dsn)?;

        let mut options = AnyPoolOptions::new()
            .max_connections(max_connections.max(1))
            .acquire_timeout(Duration::from_secs(15))
            .idle_timeout(Some(Duration::from_secs(600)));

        if driver == DbDriver::Sqlite {
            // 外键默认是关的，而 sqlx 的连接串不接受 foreign_keys 参数，
            // 所以每条新连接都要自己开一次。级联删除、SET NULL 全靠它。
            options = options.after_connect(|conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    for pragma in [
                        "PRAGMA journal_mode=WAL",
                        "PRAGMA foreign_keys=ON",
                        "PRAGMA busy_timeout=15000",
                        "PRAGMA synchronous=NORMAL",
                    ] {
                        conn.execute(pragma).await?;
                    }
                    Ok(())
                })
            });
        }

        let pool = options.connect(&dsn).await?;
        Ok(Self {
            pool,
            dialect: driver.dialect(),
        })
    }

    /// 跑迁移。PostgreSQL 与 MySQL 上 sqlx 会加锁，多实例同时启动不会打架。
    pub async fn migrate(&self) -> Result<()> {
        match self.dialect {
            Dialect::Postgres => sqlx::migrate!("../../migrations/postgres").run(&self.pool).await?,
            Dialect::MySql => sqlx::migrate!("../../migrations/mysql").run(&self.pool).await?,
            Dialect::Sqlite => sqlx::migrate!("../../migrations/sqlite").run(&self.pool).await?,
        }
        Ok(())
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub fn is_sqlite(&self) -> bool {
        self.dialect == Dialect::Sqlite
    }

    /// 把 `?` 占位符改写成当前方言的写法（PostgreSQL 需要 `$1..$n`）。
    ///
    /// 单个 `?` 之外还要跳过字符串常量与行注释里的问号——SQL 里写
    /// `WHERE ext = 'a?b'` 这种字面量不该被当成参数。
    pub fn sql(&self, template: &str) -> String {
        rewrite_placeholders(self.dialect, template)
    }

    /// 三份变体里挑一份，再走一次占位符改写。
    pub fn sql_v(&self, postgres: &str, mysql: &str, sqlite: &str) -> String {
        let chosen = match self.dialect {
            Dialect::Postgres => postgres,
            Dialect::MySql => mysql,
            Dialect::Sqlite => sqlite,
        };
        self.sql(chosen)
    }

    /// 取一行的通用入口：查不到返回 None 而不是错误。
    ///
    /// 参数走 [`Args`]，SQL 里统一写 `?`，驱动前会按方言改写占位符。
    pub async fn fetch_optional(&self, sql: &str, args: Args) -> Result<Option<AnyRow>> {
        let sql = self.sql(sql);
        match sqlx::query_with(&sql, args.take())
            .fetch_optional(&self.pool)
            .await
        {
            Ok(row) => Ok(row),
            Err(sqlx::Error::RowNotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// 需要「不存在就是错误」时用这个。
    pub async fn fetch_one(&self, sql: &str, args: Args) -> Result<AnyRow> {
        self.fetch_optional(sql, args)
            .await?
            .ok_or_else(|| RepoError::NotFound("查询单行".into()))
    }

    /// 取多行。
    pub async fn fetch_all(&self, sql: &str, args: Args) -> Result<Vec<AnyRow>> {
        let sql = self.sql(sql);
        let rows = sqlx::query_with(&sql, args.take())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// 执行写操作，返回受影响行数。
    pub async fn execute(&self, sql: &str, args: Args) -> Result<u64> {
        let sql = self.sql(sql);
        let result = sqlx::query_with(&sql, args.take())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// 取一个名为 `total` 的计数列。
    pub async fn count(&self, sql: &str, args: Args) -> Result<i64> {
        let row = self.fetch_one(sql, args).await?;
        crate::model::get_i64(&row, "total")
    }

    /// 只关心「有没有行」。
    pub async fn exists(&self, sql: &str, args: Args) -> Result<bool> {
        Ok(self.fetch_optional(sql, args).await?.is_some())
    }

    /// 插入并回填自增主键。
    ///
    /// 这是全库唯一需要按方言分流的写操作模式：PostgreSQL 与 SQLite 用
    /// `RETURNING id` 一步到位，MySQL 只能先插再查 `LAST_INSERT_ID()`。
    pub async fn insert_returning_id(&self, sql: &str, args: Args) -> Result<i64> {
        match self.dialect {
            Dialect::MySql => {
                // LAST_INSERT_ID() 是连接级的：插入与取 id 必须在同一条连接上。
                // 走连接池会落到另一条连接，读回来是 0（建账号、建相册时就看不出来
                // 是 id 错了，症状是「记录不存在：新建相册 0」）。
                let mut conn = self.pool.acquire().await?;
                let prepared = self.sql(sql);
                let affected = sqlx::query_with(&prepared, args.take())
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
                if affected == 0 {
                    return Err(RepoError::NotFound("插入未生效".into()));
                }
                let row = sqlx::query("SELECT LAST_INSERT_ID() AS id")
                    .fetch_one(&mut *conn)
                    .await?;
                crate::model::get_i64(&row, "id")
            }
            _ => {
                let sql = format!("{} RETURNING id", sql.trim_end().trim_end_matches(';'));
                let row = self.fetch_one(&sql, args).await?;
                crate::model::get_i64(&row, "id")
            }
        }
    }

    /// 上一条 INSERT 的自增主键。只有 MySQL 需要。
    pub async fn last_insert_id(&self) -> Result<i64> {
        match self.dialect {
            Dialect::MySql => {
                let row = self.fetch_one("SELECT LAST_INSERT_ID() AS id", Args::new()).await?;
                crate::model::get_i64(&row, "id")
            }
            other => Err(RepoError::Config(format!(
                "{} 不需要 last_insert_id，请用 INSERT ... RETURNING",
                other.as_str()
            ))),
        }
    }

    /// 健康检查，用于启动日志和 /healthz。
    pub async fn ping(&self) -> Result<()> {
        self.execute("SELECT 1", Args::new()).await?;
        Ok(())
    }

    /// 关闭连接池，等待在途查询结束。
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// SQLite 连接串补参数：外键必须显式打开，否则级联删除形同虚设；
/// WAL 是设计稿指定的写入模式；busy_timeout 防止并发写直接报错。
fn normalize_dsn(driver: DbDriver, dsn: &str) -> Result<String> {
    let dsn = dsn.trim();
    if dsn.is_empty() {
        return Err(RepoError::Config("数据库连接串为空".into()));
    }
    if driver != DbDriver::Sqlite {
        return Ok(dsn.to_string());
    }

    let (base, existing_query) = match dsn.split_once('?') {
        Some((base, query)) => (base, Some(query.to_string())),
        None => (dsn, None),
    };

    let path_part = if let Some(rest) = base.strip_prefix("sqlite://") {
        rest.to_string()
    } else if let Some(rest) = base.strip_prefix("sqlite:") {
        rest.to_string()
    } else {
        base.to_string()
    };

    // 落盘目录要先存在，sqlx 只负责建文件不负责建目录
    let file_path = path_part.trim_start_matches('/');
    if !file_path.is_empty() && file_path != ":memory:" {
        if let Some(parent) = Path::new(file_path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    RepoError::Config(format!("无法创建数据库目录 {}：{e}", parent.display()))
                })?;
            }
        }
    }

    let mut params: Vec<String> = Vec::new();
    // 只有 mode=rwc 走连接串（库文件不存在就建）；其余 pragma 由 after_connect 施加，
    // 因为 sqlx 的连接串解析只认少数几个参数，foreign_keys 不在其中。
    let defaults = ["mode=rwc"];
    let existing = existing_query.unwrap_or_default();
    for default in defaults {
        let key = default.split('=').next().unwrap_or_default();
        if !existing
            .split('&')
            .any(|pair| pair.split('=').next() == Some(key))
        {
            params.push(default.to_string());
        }
    }
    if !existing.is_empty() {
        params.insert(0, existing);
    }

    // Windows 盘符路径要写成 sqlite:///C:/...，否则盘符会被当成 host
    let prefix = if path_part.starts_with("//") {
        "sqlite:".to_string()
    } else if path_part.starts_with('/')
        || path_part
            .chars()
            .nth(1)
            .map(|c| c == ':')
            .unwrap_or(false)
    {
        "sqlite:///".to_string()
    } else {
        "sqlite://".to_string()
    };

    Ok(format!("{prefix}{path_part}?{}", params.join("&")))
}

/// PostgreSQL 用 `$1..$n`，其余两种方言保留 `?`。
///
/// 另外替 MySQL 把 LIKE 里的 `ESCAPE '\'` 去掉：反斜杠在 MySQL 的字符串
/// 字面量里自己就是转义符，写 `ESCAPE '\'` 会把结束引号转义掉、直接报语法错误；
/// 而 MySQL 的 LIKE 默认就拿反斜杠当转义符，删掉这个子句语义不变
/// （`like_pattern()` 产出的 `\%` / `\_` 仍然按字面匹配）。
fn rewrite_placeholders(dialect: Dialect, sql: &str) -> String {
    if dialect == Dialect::MySql {
        return sql.replace(r"ESCAPE '\'", "");
    }
    if dialect != Dialect::Postgres {
        return sql.to_string();
    }

    let mut out = String::with_capacity(sql.len() + 16);
    let mut index = 0usize;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                // 字符串常量原样拷贝，'' 是转义的单引号
                out.push(ch);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == '\'' {
                        if chars.peek() == Some(&'\'') {
                            out.push(chars.next().expect("peek 过一定存在"));
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                // 行注释拷到行尾，里面的问号不是占位符
                out.push(ch);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '?' => {
                index += 1;
                out.push('$');
                out.push_str(&index.to_string());
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_placeholders_are_numbered() {
        let sql = rewrite_placeholders(Dialect::Postgres, "SELECT * FROM t WHERE a = ? AND b = ?");
        assert_eq!(sql, "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn other_dialects_keep_question_marks() {
        let sql = "SELECT * FROM t WHERE a = ?";
        assert_eq!(rewrite_placeholders(Dialect::MySql, sql), sql);
        assert_eq!(rewrite_placeholders(Dialect::Sqlite, sql), sql);
    }

    #[test]
    fn question_marks_in_literals_are_left_alone() {
        let sql = rewrite_placeholders(
            Dialect::Postgres,
            "SELECT '?' AS q, a FROM t WHERE note = 'a?b' AND x = ? -- trailing ? \n",
        );
        assert_eq!(
            sql,
            "SELECT '?' AS q, a FROM t WHERE note = 'a?b' AND x = $1 -- trailing ? \n"
        );
    }

    #[test]
    fn escaped_quotes_inside_literals() {
        let sql = rewrite_placeholders(Dialect::Postgres, "SELECT * FROM t WHERE n = 'it''s?' AND k = ?");
        assert_eq!(sql, "SELECT * FROM t WHERE n = 'it''s?' AND k = $1");
    }

    #[test]
    fn sqlite_dsn_gets_create_mode() {
        let normalized = normalize_dsn(DbDriver::Sqlite, "data/pixora.db").unwrap();
        assert!(normalized.starts_with("sqlite://data/pixora.db?"), "{normalized}");
        assert!(normalized.contains("mode=rwc"));
    }

    #[test]
    fn sqlite_dsn_keeps_explicit_params() {
        let normalized =
            normalize_dsn(DbDriver::Sqlite, "sqlite://data/x.db?journal_mode=DELETE").unwrap();
        assert!(normalized.contains("journal_mode=DELETE"));
        assert!(normalized.contains("mode=rwc"));
    }

    #[test]
    fn sqlite_absolute_windows_path_is_three_slash() {
        let normalized = normalize_dsn(DbDriver::Sqlite, "E:/data/pixora.db").unwrap();
        assert!(normalized.starts_with("sqlite:///E:/data/pixora.db?"), "{normalized}");
    }

    #[test]
    fn driver_sniffing() {
        assert_eq!(DbDriver::sniff("postgres://x"), Some(DbDriver::Postgres));
        assert_eq!(DbDriver::sniff("mysql://x"), Some(DbDriver::MySql));
        assert_eq!(DbDriver::sniff("sqlite://x"), Some(DbDriver::Sqlite));
        assert_eq!(DbDriver::sniff("data/x.db"), None);
        assert!(DbDriver::parse("mariadb").is_ok());
        assert!(DbDriver::parse("oracle").is_err());
    }
}
