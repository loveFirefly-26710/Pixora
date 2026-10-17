//! 系统设置仓储。
//!
//! 键值对存库、改完即时生效，不重启。敏感值（SMTP 密码、第三方校验密钥）
//! 以 `encrypted = true` 标记，`value` 列里放的是密文——
//! 加解密在服务层做，仓储层只管搬运。

use crate::args::Args;
use crate::error::Result;
use crate::model::*;
use crate::repo::Repo;

impl Repo {
    pub async fn get_setting(&self, name: &str) -> Result<Option<Setting>> {
        let row = self
            .db
            .fetch_optional(
                "SELECT name, value, encrypted, updated_at FROM settings WHERE name = ?",
                Args::new().text(name),
            )
            .await?;
        row.as_ref().map(Setting::from_row).transpose()
    }

    /// 读一个设置项的原值。读不到就是 `None`，调用方负责给默认值。
    pub async fn get_setting_value(&self, name: &str) -> Result<Option<String>> {
        Ok(self.get_setting(name).await?.and_then(|item| item.value))
    }

    /// 读字符串设置，缺省时返回 `default`。
    pub async fn setting_or(&self, name: &str, default: &str) -> Result<String> {
        Ok(self
            .get_setting_value(name)
            .await?
            .unwrap_or_else(|| default.to_string()))
    }

    /// 读整数设置，解析失败按缺省处理——配置项被手工改坏时不应该让服务起不来。
    pub async fn setting_i64(&self, name: &str, default: i64) -> Result<i64> {
        Ok(self
            .get_setting_value(name)
            .await?
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .unwrap_or(default))
    }

    pub async fn setting_bool(&self, name: &str, default: bool) -> Result<bool> {
        Ok(self
            .get_setting_value(name)
            .await?
            .map(|raw| matches!(raw.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(default))
    }

    /// 写一个设置项。值传 `None` 即清空。
    pub async fn set_setting(
        &self,
        name: &str,
        value: Option<&str>,
        encrypted: bool,
    ) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET value = ?, encrypted = ?, updated_at = ?",
            "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE value = VALUES(value), encrypted = VALUES(encrypted), updated_at = VALUES(updated_at)",
            "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET value = ?, encrypted = ?, updated_at = ?",
        );
        let now = now_ts();
        let args = match self.db.dialect() {
            crate::db::Dialect::MySql => Args::new()
                .text(name)
                .opt_text(value.map(str::to_string))
                .bool(encrypted)
                .text(&now),
            _ => Args::new()
                .text(name)
                .opt_text(value.map(str::to_string))
                .bool(encrypted)
                .text(&now)
                .opt_text(value.map(str::to_string))
                .bool(encrypted)
                .text(&now),
        };
        self.db.execute(&sql, args).await?;
        Ok(())
    }

    pub async fn set_setting_plain(&self, name: &str, value: &str) -> Result<()> {
        self.set_setting(name, Some(value), false).await
    }

    /// 批量写入。安装向导一次写十几项，走同一个循环即可。
    pub async fn set_settings(&self, items: &[(String, String)]) -> Result<()> {
        for (name, value) in items {
            self.set_setting_plain(name, value).await?;
        }
        Ok(())
    }

    pub async fn list_settings(&self) -> Result<Vec<Setting>> {
        let rows = self
            .db
            .fetch_all(
                "SELECT name, value, encrypted, updated_at FROM settings ORDER BY name ASC",
                Args::new(),
            )
            .await?;
        rows.iter().map(Setting::from_row).collect()
    }

    pub async fn delete_setting(&self, name: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM settings WHERE name = ?", Args::new().text(name))
            .await?;
        Ok(())
    }

    /// 安装向导是否已经走完。没有这一项就说明是全新实例。
    pub async fn is_installed(&self) -> Result<bool> {
        self.setting_bool("install.completed", false).await
    }

    // ---------- 配置类二进制资产 ----------

    /// 存一份配置资产（目前只有站点背景图）。
    ///
    /// 复用 `storage_objects` 这张表：它本来就是「字节进库」的落点，
    /// 三种方言的列类型都按大对象给（LONGBLOB / BYTEA / BLOB），
    /// 而 `settings.value` 在 MySQL 上是 TEXT（64KB），放不下图片。
    /// 键名带 `config:` 前缀，与图片实体的 CAS 键区分开——GC 只扫 blobs 表，
    /// 不会碰这里。
    pub async fn put_asset(&self, key: &str, content: &[u8]) -> Result<()> {
        let sql = self.db.sql_v(
            "INSERT INTO storage_objects (key, content, size, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (key) DO UPDATE SET content = ?, size = ?, updated_at = ?",
            "INSERT INTO storage_objects (`key`, content, size, updated_at) VALUES (?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE content = VALUES(content), size = VALUES(size), \
                                     updated_at = VALUES(updated_at)",
            "INSERT INTO storage_objects (key, content, size, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (key) DO UPDATE SET content = ?, size = ?, updated_at = ?",
        );
        let now = now_ts();
        let size = content.len() as i64;
        let args = match self.db.dialect() {
            crate::db::Dialect::MySql => Args::new()
                .text(key)
                .blob(content.to_vec())
                .int(size)
                .text(&now),
            _ => Args::new()
                .text(key)
                .blob(content.to_vec())
                .int(size)
                .text(&now)
                .blob(content.to_vec())
                .int(size)
                .text(&now),
        };
        self.db.execute(&sql, args).await?;
        Ok(())
    }

    /// 读一份配置资产，返回 (字节, 更新时间戳串)。
    pub async fn get_asset(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        let row = self
            .db
            .fetch_optional(
                &self.db.sql_v(
                    "SELECT content, updated_at FROM storage_objects WHERE key = ?",
                    "SELECT content, updated_at FROM storage_objects WHERE `key` = ?",
                    "SELECT content, updated_at FROM storage_objects WHERE key = ?",
                ),
                Args::new().text(key),
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some((
            crate::model::get_bytes(&row, "content")?,
            crate::model::get_str(&row, "updated_at")?,
        )))
    }

    /// 这份资产什么时候更新的（用于给直链加版本号，改完立刻失效旧缓存）。
    pub async fn asset_version(&self, key: &str) -> Result<Option<String>> {
        let row = self
            .db
            .fetch_optional(
                &self.db.sql_v(
                    "SELECT updated_at FROM storage_objects WHERE key = ?",
                    "SELECT updated_at FROM storage_objects WHERE `key` = ?",
                    "SELECT updated_at FROM storage_objects WHERE key = ?",
                ),
                Args::new().text(key),
            )
            .await?;
        row.as_ref().map(|row| crate::model::get_str(row, "updated_at")).transpose()
    }

    pub async fn delete_asset(&self, key: &str) -> Result<u64> {
        self.db
            .execute(
                &self.db.sql_v(
                    "DELETE FROM storage_objects WHERE key = ?",
                    "DELETE FROM storage_objects WHERE `key` = ?",
                    "DELETE FROM storage_objects WHERE key = ?",
                ),
                Args::new().text(key),
            )
            .await
    }
}
