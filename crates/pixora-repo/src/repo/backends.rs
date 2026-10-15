//! 存储后端实例仓储。
//!
//! 后端配置（driver + label + config）存库，敏感字段（Token、密码）在入库前
//! 由服务层用 AES-256-GCM 加密，这里只当普通文本搬。`config` 列是 JSON 文本。
//!
//! 默认后端只有一行：数据库唯一索引保证，`set_default` 里再用事务把旧的清掉。

use crate::args::Args;
use crate::error::Result;
use crate::model::{now_ts, StorageBackend};
use crate::repo::Repo;

/// 新建后端的入参。`config` 是已经加密好的 JSON 文本。
#[derive(Debug, Clone)]
pub struct NewBackend {
    pub driver: String,
    pub label: String,
    pub config: String,
    pub is_default: bool,
    pub enabled: bool,
    pub capacity_mode: String,
    pub capacity_total: Option<i64>,
}

/// 后端配置的补丁。`None` 表示不动，`Some(None)` 表示清空。
#[derive(Debug, Clone, Default)]
pub struct BackendPatch {
    pub label: Option<String>,
    pub config: Option<String>,
    pub enabled: Option<bool>,
    pub capacity_mode: Option<String>,
    pub capacity_total: Option<Option<i64>>,
}

impl Repo {
    // ---------- 读取 ----------

    /// 全部实例，含被停用的。后台配置页要能把停用中的实例也列出来。
    pub async fn list_backends(&self) -> Result<Vec<StorageBackend>> {
        let sql = format!(
            "SELECT {} FROM storage_backends ORDER BY id ASC",
            StorageBackend::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new()).await?;
        rows.iter().map(StorageBackend::from_row).collect()
    }

    /// 只取启用中的。上传选路与读取回落按这份清单挑后端。
    pub async fn list_enabled_backends(&self) -> Result<Vec<StorageBackend>> {
        let sql = format!(
            "SELECT {} FROM storage_backends WHERE enabled = 1 ORDER BY id ASC",
            StorageBackend::COLUMNS
        );
        let rows = self.db.fetch_all(&sql, Args::new()).await?;
        rows.iter().map(StorageBackend::from_row).collect()
    }

    pub async fn find_backend(&self, id: i64) -> Result<Option<StorageBackend>> {
        let sql = format!(
            "SELECT {} FROM storage_backends WHERE id = ?",
            StorageBackend::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new().int(id)).await?;
        row.as_ref().map(StorageBackend::from_row).transpose()
    }

    /// 当前默认后端。唯一索引保证至多一行；查不到说明实例还没配过。
    pub async fn default_backend(&self) -> Result<Option<StorageBackend>> {
        let sql = format!(
            "SELECT {} FROM storage_backends WHERE is_default = 1 ORDER BY id ASC LIMIT 1",
            StorageBackend::COLUMNS
        );
        let row = self.db.fetch_optional(&sql, Args::new()).await?;
        row.as_ref().map(StorageBackend::from_row).transpose()
    }

    /// 同类型只允许一个实例时用（比如 local）。历史数据里可能有同类型的多行，
    /// 取 id 最小的那个——先建的实例才是配置里一直生效的那份。
    pub async fn find_backend_by_driver(&self, driver: &str) -> Result<Option<StorageBackend>> {
        let sql = format!(
            "SELECT {} FROM storage_backends WHERE driver = ? ORDER BY id ASC LIMIT 1",
            StorageBackend::COLUMNS
        );
        let row = self
            .db
            .fetch_optional(&sql, Args::new().text(driver))
            .await?;
        row.as_ref().map(StorageBackend::from_row).transpose()
    }

    // ---------- 写入 ----------

    /// 登记一个后端实例。`config` 原样落库，加密是服务层的事。
    pub async fn create_backend(&self, new: &NewBackend) -> Result<StorageBackend> {
        let id = self
            .db
            .insert_returning_id(
                "INSERT INTO storage_backends (driver, label, config, is_default, enabled, \
                                               capacity_mode, capacity_total, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                Args::new()
                    .text(&new.driver)
                    .text(&new.label)
                    .text(&new.config)
                    .bool(new.is_default)
                    .bool(new.enabled)
                    .text(&new.capacity_mode)
                    .opt_int(new.capacity_total)
                    .text(now_ts()),
            )
            .await?;

        self.find_backend(id)
            .await?
            .ok_or_else(|| crate::RepoError::NotFound(format!("新建存储后端 {id}")))
    }

    /// 局部修改：只拼有值的列，没给的字段保持原样。
    /// `capacity_total` 的 `Some(None)` 表示把容量上限清空（回到自动探测）。
    pub async fn update_backend(&self, id: i64, patch: &BackendPatch) -> Result<()> {
        let mut sets: Vec<&str> = Vec::new();
        let mut args = Args::new();

        if let Some(label) = &patch.label {
            sets.push("label = ?");
            args = args.text(label);
        }
        if let Some(config) = &patch.config {
            sets.push("config = ?");
            args = args.text(config);
        }
        if let Some(enabled) = patch.enabled {
            sets.push("enabled = ?");
            args = args.bool(enabled);
        }
        if let Some(mode) = &patch.capacity_mode {
            sets.push("capacity_mode = ?");
            args = args.text(mode);
        }
        if let Some(total) = patch.capacity_total {
            sets.push("capacity_total = ?");
            args = args.opt_int(total);
        }

        if sets.is_empty() {
            return Ok(());
        }

        self.db
            .execute(
                &format!(
                    "UPDATE storage_backends SET {} WHERE id = ?",
                    sets.join(", ")
                ),
                args.int(id),
            )
            .await?;
        Ok(())
    }

    /// 删除实例。删的是默认后端时「谁来接任」由服务层决定，这里只负责删行。
    pub async fn delete_backend(&self, id: i64) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM storage_backends WHERE id = ?",
                Args::new().int(id),
            )
            .await?;
        Ok(())
    }

    /// 切换默认后端。
    ///
    /// 唯一索引只允许一行 `is_default` 为真，所以只能「先全部置假、再置真」，
    /// 两步必须在一个事务里：中间失败会留下一个没有任何默认后端的库，
    /// 之后每次上传选路都得出错。
    pub async fn set_default_backend(&self, id: i64) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            &self
                .db
                .sql("UPDATE storage_backends SET is_default = 0 WHERE is_default = 1"),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            &self
                .db
                .sql("UPDATE storage_backends SET is_default = 1 WHERE id = ?"),
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// 容量口径与上限。`auto` 表示按驱动自己探测，`fixed` 才用得上 `total`。
    pub async fn set_capacity_mode(&self, id: i64, mode: &str, total: Option<i64>) -> Result<()> {
        self.db
            .execute(
                "UPDATE storage_backends SET capacity_mode = ?, capacity_total = ? WHERE id = ?",
                Args::new().text(mode).opt_int(total).int(id),
            )
            .await?;
        Ok(())
    }

    pub async fn count_backends(&self) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM storage_backends",
                Args::new(),
            )
            .await
    }
}

impl Repo {
    /// 改一个后端的显示名。只动 `label` 一列。
    pub async fn update_backend_label(&self, id: i64, label: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE storage_backends SET label = ? WHERE id = ?",
                crate::args::Args::new().text(label).int(id),
            )
            .await?;
        Ok(())
    }
}

impl Repo {
    /// 这个后端上还挂着多少实体（原图 + 缩略图都算）。
    ///
    /// 删后端前要问一句：配置行删掉之后驱动就装配不出来了，
    /// 远端驱动连地址和凭据都没了，那些对象既读不出来也删不掉。
    pub async fn count_blobs_for_backend(&self, backend_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM blobs WHERE backend_id = ?",
                crate::args::Args::new().int(backend_id),
            )
            .await
    }

    /// 这个驱动上还挂着多少实体。`backend_id` 为空的历史实体会被这条查出来——
    /// 它们同样指向这个后端（本地与数据库内两种驱动共用一份落点）。
    pub async fn count_blobs_by_driver(&self, driver: &str) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM blobs WHERE storage_driver = ?",
                crate::args::Args::new().text(driver),
            )
            .await
    }
}
