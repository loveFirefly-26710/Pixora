//! 仓储方法集合。
//!
//! 所有实体方法都挂在 [`Repo`] 上，按文件分组：`users.rs` 管用户，`images.rs` 管图片，
//! 以此类推。上层服务只需要持有一个 `Arc<Repo>`，不必逐层传递连接或事务对象。
//!
//! 需要多语句原子性的操作（建引用时同时改计数与配额、切换默认后端、发布条款）
//! 各自写成带事务的方法，事务体就在方法内部——不额外抽一层闭包形的「事务作用域」，
//! 那样会被异步闭包的生命周期绑住，可读性反而更差。
//!
//! 查询一律走 [`crate::db::Db`] 上的 `fetch_* / execute`，SQL 里写 `?` 占位符，
//! 参数用 [`crate::args::Args`] 按顺序给。只有需要按方言分流的语句才用 `sql_v`。

pub mod albums;
pub mod announcements;
pub mod audit;
pub mod backends;
pub mod blobs;
pub mod identities;
pub mod images;
pub mod iprules;
pub mod jobs;
pub mod mail;
pub mod ratelimit;
pub mod recovery;
pub mod sessions;
pub mod settings;
pub mod stats;
pub mod tags;
pub mod terms;
pub mod tokens;
pub mod uploads;
pub mod users;
pub mod verifications;

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::db::Db;

/// 仓储句柄。克隆只是 Arc 拷贝。
#[derive(Clone)]
pub struct Repo {
    db: Arc<Db>,
}

impl Repo {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }
}

// ============================================================
// 分页
// ============================================================

/// 分页请求。页码从 1 开始，对齐界面上的写法。
#[derive(Debug, Clone, Copy)]
pub struct PageRequest {
    pub page: i64,
    pub per_page: i64,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 24,
        }
    }
}

impl PageRequest {
    pub fn new(
        page: Option<i64>,
        per_page: Option<i64>,
        default_per_page: i64,
        max_per_page: i64,
    ) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            per_page: per_page
                .unwrap_or(default_per_page)
                .clamp(1, max_per_page.max(1)),
        }
    }

    pub fn offset(&self) -> i64 {
        (self.page - 1).saturating_mul(self.per_page)
    }
}

/// 分页结果：当前页数据 + 总数，界面要同时显示两者。
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: i64, request: PageRequest) -> Self {
        Self {
            items,
            total,
            page: request.page,
            per_page: request.per_page,
        }
    }

    /// 空页，用于提前返回（比如总数已算出为零）。
    pub fn empty(request: PageRequest) -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            page: request.page,
            per_page: request.per_page,
        }
    }

    pub fn total_pages(&self) -> i64 {
        if self.per_page <= 0 {
            return 0;
        }
        (self.total + self.per_page - 1) / self.per_page
    }

    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            total: self.total,
            page: self.page,
            per_page: self.per_page,
        }
    }
}

// ============================================================
// 通用小工具
// ============================================================

/// LIKE 的模糊匹配串。转义 `%` 与 `_`，避免用户输入改变匹配语义。
/// 配套的 SQL 要写 `ESCAPE '\'`。
pub fn like_pattern(input: &str) -> String {
    let escaped = input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// 前缀匹配串，用于「邮箱以某串开头」这类查询。
pub fn prefix_pattern(input: &str) -> String {
    let escaped = input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

/// 近 N 天的窗口起点，配合 `substr` 分桶使用。
pub fn window_start(days: i64) -> DateTime<Utc> {
    Utc::now() - chrono::Duration::days(days)
}
