//! Pixora 仓储层。
//!
//! 设计稿把「SQL 方言适配」放在这一层，上层服务只调仓储方法，不写方言 SQL。
//! 具体做法有两条：
//!
//!   1. 用 `sqlx::Any` 驱动跑同一套查询。三种方言共用一份 SQL 文本，
//!      写法统一用 `?` 占位符，交给 [`Db::sql`] 在 PostgreSQL 上改写成 `$1..$n`。
//!      跨方言差异（自增主键回填、队列取任务的锁语义、UPSERT 语法）收敛到
//!      [`Db::sql_v`] 的三份变体里，全库只有个位数处。
//!   2. 时间戳、JSON 载荷、IP 与 CIDR 都用可移植的列类型承载（RFC3339 文本、
//!      JSON 文本、纯字符串），这样解码路径只有一条，也不需要为每种数据库
//!      各写一套日期函数。
//!
//! 换来的是：新增一种数据库要改的地方是可数的，而不是翻倍。

pub mod args;
pub mod db;
pub mod error;
pub mod model;
pub mod repo;

pub use args::{Args, WhereBuilder};
pub use db::{Db, DbDriver, Dialect};
pub use error::{RepoError, Result};
pub use model::*;
pub use repo::{Page, PageRequest, Repo};

// 各模块的入参类型统一从根部出口暴露，调用方不必记住它们在哪个文件里
pub use repo::albums::{AlbumFilter, NewAlbum};
pub use repo::audit::AuditEntry;
pub use repo::images::{ImageFilter, ImagePatch, NewImage};
pub use repo::jobs::NewJob;
pub use repo::tokens::NewApiToken;
pub use repo::users::{NewUser, UserFilter};

/// 时间戳的存储形态：RFC3339、UTC、微秒精度，定长到能直接按字典序比较。
pub const TS_FORMAT_HINT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";
