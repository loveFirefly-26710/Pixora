//! 标签仓储。
//!
//! 标签按 `tags`（用户维度词表）+ `image_tags`（引用关系）两张表存，
//! 三种方言共用这一份实现（设计稿只对 MySQL 指定了关联表，这里统一，
//! 好让「按标签筛选」「候选提示」「全局改名」只有一条代码路径）。

use std::collections::HashMap;

use crate::args::{Args, WhereBuilder};
use crate::error::Result;
use crate::model::*;
use crate::repo::{like_pattern, Repo};

impl Repo {
    /// 取出或创建词表里的一行，返回标签 id。
    ///
    /// 并发打同一个标签时靠唯一键兜底：先试插入，冲突了就查出来。
    pub async fn ensure_tag(&self, owner_id: i64, name: &str) -> Result<i64> {
        let name = name.trim();
        if let Some(row) = self
            .db
            .fetch_optional(
                "SELECT id FROM tags WHERE owner_id = ? AND name = ?",
                Args::new().int(owner_id).text(name),
            )
            .await?
        {
            return get_i64(&row, "id");
        }

        match self
            .db
            .insert_returning_id(
                "INSERT INTO tags (owner_id, name, created_at) VALUES (?, ?, ?)",
                Args::new().int(owner_id).text(name).text(now_ts()),
            )
            .await
        {
            Ok(id) => Ok(id),
            Err(err) if err.is_unique_violation() => {
                let row = self
                    .db
                    .fetch_one(
                        "SELECT id FROM tags WHERE owner_id = ? AND name = ?",
                        Args::new().int(owner_id).text(name),
                    )
                    .await?;
                get_i64(&row, "id")
            }
            Err(err) => Err(err),
        }
    }

    /// 覆盖式设置一张图的标签集合。
    ///
    /// 全程在同一个事务连接上做：SQLite 只有一个写者，若在事务外再借连接插标签，
    /// 会和本事务的写锁互相等待。
    pub async fn set_image_tags(&self, image_id: i64, owner_id: i64, tags: &[String]) -> Result<()> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(&self.db.sql("DELETE FROM image_tags WHERE image_id = ?"))
            .bind(image_id)
            .execute(&mut *tx)
            .await?;

        for name in normalize_tag_list(tags) {
            let tag_id = ensure_tag_on(&self.db, &mut tx, owner_id, &name).await?;
            let insert = self.db.sql_v(
                "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
                "INSERT IGNORE INTO image_tags (image_id, tag_id) VALUES (?, ?)",
                "INSERT INTO image_tags (image_id, tag_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
            );
            sqlx::query(&insert)
                .bind(image_id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 批量取多张图的标签，一次查完再在内存里分组——
    /// 列表页每页几十张图，逐张查会变成 N+1。
    pub async fn tags_for_images(&self, image_ids: &[i64]) -> Result<HashMap<i64, Vec<String>>> {
        let mut result: HashMap<i64, Vec<String>> = HashMap::new();
        if image_ids.is_empty() {
            return Ok(result);
        }

        for chunk in image_ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT it.image_id AS image_id, t.name AS name \
                   FROM image_tags it JOIN tags t ON t.id = it.tag_id \
                  WHERE it.image_id IN ({placeholders}) ORDER BY t.name ASC"
            );
            let mut args = Args::new();
            for id in chunk {
                args = args.int(*id);
            }
            let rows = self.db.fetch_all(&sql, args).await?;
            for row in rows {
                result
                    .entry(get_i64(&row, "image_id")?)
                    .or_default()
                    .push(get_str(&row, "name")?);
            }
        }
        Ok(result)
    }

    /// 使用次数只数活着的图：回收站里的图也保留着标签链接（恢复时要带回来），
    /// 但点开标签看的是「自己的图」，把墓碑算进来数字会对不上。
    /// 词表列表，带使用次数。`query` 用于输入时的候选提示。
    pub async fn list_tags(&self, owner_id: i64, query: Option<&str>, limit: i64) -> Result<Vec<Tag>> {
        let mut filters = WhereBuilder::new().and("t.owner_id = ?", |args| args.int(owner_id));
        if let Some(query) = query.map(str::trim).filter(|value| !value.is_empty()) {
            let pattern = like_pattern(query);
            filters = filters.and("t.name LIKE ? ESCAPE '\\'", |args| args.text(pattern));
        }
        let sql = format!(
            "SELECT t.id, t.owner_id, t.name, t.created_at, \
                    (SELECT COUNT(*) FROM image_tags it \
                       JOIN images i ON i.id = it.image_id \
                      WHERE it.tag_id = t.id AND i.deleted_at IS NULL) AS usage \
               FROM tags t{} ORDER BY usage DESC, t.name ASC LIMIT ?",
            filters.sql_string()
        );
        let rows = self
            .db
            .fetch_all(&sql, filters.args().int(limit.clamp(1, 500)))
            .await?;
        rows.iter().map(Tag::from_row).collect()
    }

    /// 广场的标签云：跨用户的公开标签按出现次数排序。
    ///
    /// `plaza_album_ids` 是管理员限制的首页展示相册；给了就只数这些相册里的图，
    /// 否则标签云会出现「点了没结果」的标签——图不在广场范围内。
    pub async fn plaza_tags(&self, limit: i64, plaza_album_ids: &[i64]) -> Result<Vec<Tag>> {
        let mut args = Args::new();
        let mut scope = String::new();
        if !plaza_album_ids.is_empty() {
            let placeholders = vec!["?"; plaza_album_ids.len()].join(", ");
            scope = format!(
                " AND i.album_id IN ({placeholders}) \
                  AND EXISTS (SELECT 1 FROM albums pa WHERE pa.id = i.album_id \
                              AND pa.visibility = 'public' AND pa.deleted_at IS NULL)"
            );
            for id in plaza_album_ids {
                args = args.int(*id);
            }
        }
        let sql = format!(
            "SELECT 0 AS id, 0 AS owner_id, t.name AS name, t.created_at AS created_at, \
                    COUNT(DISTINCT it.image_id) AS usage \
               FROM tags t \
               JOIN image_tags it ON it.tag_id = t.id \
               JOIN images i ON i.id = it.image_id \
              WHERE i.visibility = 'public' AND i.deleted_at IS NULL{scope} \
              GROUP BY t.name ORDER BY usage DESC, t.name ASC LIMIT ?"
        );
        let rows = self
            .db
            .fetch_all(&sql, args.int(limit.clamp(1, 100)))
            .await?;
        rows.iter().map(Tag::from_row).collect()
    }

    /// 全局改名：把词表里的一行改名。同名目标已存在时合并（引用转移后删旧行）。
    pub async fn rename_tag(&self, owner_id: i64, from: &str, to: &str) -> Result<()> {
        let from = from.trim();
        let to = to.trim();
        if from.is_empty() || to.is_empty() || from == to {
            return Ok(());
        }

        let source = self
            .db
            .fetch_optional(
                "SELECT id FROM tags WHERE owner_id = ? AND name = ?",
                Args::new().int(owner_id).text(from),
            )
            .await?;
        let Some(source) = source else {
            return Ok(());
        };
        let source_id = get_i64(&source, "id")?;
        let target_id = self.ensure_tag(owner_id, to).await?;

        // 把引用先挪到目标标签，再删掉源标签。冲突的 (image_id, tag_id) 直接忽略，
        // 因为两张图原本各自带 from / to 是合法的，合并后只剩一个标签。
        let move_sql = self.db.sql_v(
            "INSERT INTO image_tags (image_id, tag_id) \
             SELECT image_id, ? FROM image_tags WHERE tag_id = ? ON CONFLICT DO NOTHING",
            "INSERT IGNORE INTO image_tags (image_id, tag_id) \
             SELECT image_id, ? FROM image_tags WHERE tag_id = ?",
            "INSERT INTO image_tags (image_id, tag_id) \
             SELECT image_id, ? FROM image_tags WHERE tag_id = ? ON CONFLICT DO NOTHING",
        );
        self.db
            .execute(&move_sql, Args::new().int(target_id).int(source_id))
            .await?;
        self.db
            .execute("DELETE FROM tags WHERE id = ?", Args::new().int(source_id))
            .await?;
        Ok(())
    }

    /// 全局删除：删词表行，引用连同消失。
    pub async fn delete_tag(&self, owner_id: i64, name: &str) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM tags WHERE owner_id = ? AND name = ?",
                Args::new().int(owner_id).text(name.trim()),
            )
            .await?;
        Ok(())
    }

    /// 清理没有任何引用的空标签，定期跑一次。
    pub async fn purge_unused_tags(&self) -> Result<u64> {
        self.db
            .execute(
                "DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM image_tags it WHERE it.tag_id = tags.id)",
                Args::new(),
            )
            .await
    }

    pub async fn count_tags(&self, owner_id: i64) -> Result<i64> {
        self.db
            .count(
                "SELECT COUNT(*) AS total FROM tags WHERE owner_id = ?",
                Args::new().int(owner_id),
            )
            .await
    }
}

/// 在给定事务连接上取出或创建标签。
pub(crate) async fn ensure_tag_on(
    db: &crate::db::Db,
    conn: &mut sqlx::AnyConnection,
    owner_id: i64,
    name: &str,
) -> Result<i64> {
    let lookup = db.sql("SELECT id FROM tags WHERE owner_id = ? AND name = ?");
    if let Some(row) = sqlx::query(&lookup)
        .bind(owner_id)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?
    {
        return get_i64(&row, "id");
    }

    let insert = db.sql_v(
        "INSERT INTO tags (owner_id, name, created_at) VALUES (?, ?, ?) RETURNING id",
        "INSERT INTO tags (owner_id, name, created_at) VALUES (?, ?, ?)",
        "INSERT INTO tags (owner_id, name, created_at) VALUES (?, ?, ?) RETURNING id",
    );
    let created_at = now_ts();

    if db.dialect() == crate::db::Dialect::MySql {
        sqlx::query(&insert)
            .bind(owner_id)
            .bind(name)
            .bind(&created_at)
            .execute(&mut *conn)
            .await?;
        let row = sqlx::query(&db.sql("SELECT LAST_INSERT_ID() AS id"))
            .fetch_one(&mut *conn)
            .await?;
        return get_i64(&row, "id");
    }

    let row = sqlx::query(&insert)
        .bind(owner_id)
        .bind(name)
        .bind(&created_at)
        .fetch_one(&mut *conn)
        .await?;
    get_i64(&row, "id")
}

/// 归一化标签列表：去空白、去重、过滤超长与空串，保持用户输入的顺序。
pub fn normalize_tag_list(tags: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tag in tags {
        for part in tag.split([',', '，']) {
            let name = part.trim();
            if name.is_empty() || name.chars().count() > 32 {
                continue;
            }
            if seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    out.truncate(20);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_dedupes_and_trims() {
        let tags = vec![
            "风景, 旅行".to_string(),
            "风景".to_string(),
            "   ".to_string(),
            "a".repeat(40),
            "夜景".to_string(),
        ];
        assert_eq!(
            normalize_tag_list(&tags),
            vec!["风景".to_string(), "旅行".to_string(), "夜景".to_string()]
        );
    }
}
