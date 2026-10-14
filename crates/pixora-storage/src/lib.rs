//! Pixora 存储驱动微内核。
//!
//! 核心系统只认识 [`StorageDriver`] 这一个接口，本地磁盘、数据库、GitHub、
//! WebDAV、Telegram、S3 各自是一个实现。新增一种后端等于新增一个实现，
//! 核心代码不动——这就是设计稿里说的微内核插件模式。
//!
//! 关键抽象是 [`Capabilities`]：本地文件系统支持硬链接、能探测容量；
//! 对象存储两者都不支持。上层据此决定「要不要维护镜像目录树」「容量怎么展示」，
//! 而不是到处写 `if driver == "local"`。

pub mod capacity;
pub mod cas;
pub mod database;
pub mod driver;
pub mod error;
pub mod github;
pub mod local;
pub mod registry;
pub mod s3;
pub mod telegram;
pub mod webdav;

pub use capacity::{CapacityProbe, FsKind};
pub use cas::{cas_key, view_rel_path};
pub use database::{DatabaseDriver, SqlDialect};
pub use driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
pub use error::{Result, StorageError};
pub use github::{GitHubConfig, GitHubDriver};
pub use local::LocalDriver;
pub use registry::StorageRegistry;
pub use s3::{S3Config, S3Driver};
pub use telegram::{TelegramConfig, TelegramDriver};
pub use webdav::{WebDavConfig, WebDavDriver};

/// 驱动名常量。与 `storage_backends.driver` 列的取值一一对应。
pub mod names {
    pub const LOCAL: &str = "local";
    pub const DATABASE: &str = "database";
    pub const GITHUB: &str = "github";
    pub const WEBDAV: &str = "webdav";
    pub const TELEGRAM: &str = "telegram";
    pub const S3: &str = "s3";

    /// 界面上的中文名，管理后台的下拉与分布图用。
    pub fn label(driver: &str) -> &'static str {
        match driver {
            LOCAL => "本地磁盘",
            DATABASE => "数据库内",
            GITHUB => "GitHub 仓库",
            WEBDAV => "WebDAV",
            TELEGRAM => "Telegram",
            S3 => "S3 / 阿里云 OSS",
            _ => "未知后端",
        }
    }

    pub fn all() -> &'static [&'static str] {
        &[LOCAL, DATABASE, GITHUB, WEBDAV, TELEGRAM, S3]
    }
}
