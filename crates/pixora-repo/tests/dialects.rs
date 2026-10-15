//! 跨方言验证。
//!
//! 设计稿要求元数据支持 PostgreSQL、MySQL/MariaDB 与 SQLite，但本机开发环境
//! 通常只装得起 SQLite。所以这套用例做成「给了连接串就跑」：
//!
//! ```text
//! PIXORA_TEST_DB_DSN="postgres://pixora:pass@127.0.0.1/pixora, mysql://root:pass@127.0.0.1/pixora"
//! ```
//!
//! 多个连接串用逗号分隔，驱动从连接串前缀识别（`postgres://` / `mysql://` /
//! `sqlite://`）。不设这个变量时整组用例跳过——CI 里没有数据库也不该红。
//!
//! 验证的是「同一份仓储代码在新方言上是否真的跑得动」，所以每一项都必须是
//! 真读写：建表、布尔往返（三个方言的落地类型不同）、UPSERT、按内容哈希去重、
//! 引用计数增减、分页与模糊查询、时间戳的字典序比较。

use std::sync::Arc;

use pixora_repo::db::{Db, DbDriver};
use pixora_repo::model::{now, now_ts, status, visibility};
use pixora_repo::repo::users::NewUser;
use pixora_repo::repo::uploads::NewBlob;
use pixora_repo::repo::{PageRequest, Repo};
use pixora_repo::UserFilter;
use pixora_repo::Args;

/// 从环境变量取连接串清单。空或未设置时返回空列表。
fn targets() -> Vec<(DbDriver, String)> {
    let raw = match std::env::var("PIXORA_TEST_DB_DSN") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Vec::new(),
    };

    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let driver = DbDriver::sniff(entry).unwrap_or_else(|| {
                panic!("连接串 {entry} 的前缀认不出驱动，应形如 postgres:// 或 mysql:// 或 sqlite://")
            });
            (driver, entry.to_string())
        })
        .collect()
}

/// 每个方言都跑一遍的准备：连库 + 迁移，并给每次运行一个独立的库名前缀，
/// 免得同一个库被反复跑脏（PG/MySQL 上不删表，只保证互不干扰）。
async fn prepare(driver: DbDriver, dsn: &str) -> (Repo, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    // SQLite 用临时目录里的文件，避免污染仓库；其它方言用给到的连接串
    let dsn = if driver == DbDriver::Sqlite {
        format!("sqlite://{}", dir.path().join("dialects.db").display())
    } else {
        dsn.to_string()
    };

    let db = Db::connect(driver, &dsn, 4)
        .await
        .unwrap_or_else(|err| panic!("连接 {driver:?} 失败：{err}"));
    db.migrate()
        .await
        .unwrap_or_else(|err| panic!("{driver:?} 迁移失败：{err}"));
    (Repo::new(Arc::new(db)), dir)
}

/// 建一个用户，返回 (repo, uid)。
async fn seed_user(repo: &Repo, suffix: &str, uid: &str) -> pixora_repo::model::User {
    repo.create_user(&NewUser {
        uid: uid.to_string(),
        email: format!("dialect-{suffix}@example.com"),
        password_hash: "$argon2id$placeholder".to_string(),
        role: "user".to_string(),
        status: status::ACTIVE.to_string(),
        quota_bytes: 1024 * 1024,
        email_verified: true,
    })
    .await
    .unwrap_or_else(|err| panic!("建用户失败：{err}"))
}

#[tokio::test]
async fn same_repository_code_runs_on_every_configured_dialect() {
    let targets = targets();
    if targets.is_empty() {
        eprintln!("跳过：未设置 PIXORA_TEST_DB_DSN");
        return;
    }

    for (index, (driver, dsn)) in targets.into_iter().enumerate() {
        let (repo, _dir) = prepare(driver, &dsn).await;
        let suffix = format!("{}-{}", driver.dialect().as_str(), std::process::id());
        let uid = format!("9{:03}", index);

        // ---- 1. 写入与读取：布尔、整数、可空字符串在三个方言里的落地类型不同 ----
        let user = seed_user(&repo, &suffix, &uid).await;
        let read_back = repo.find_user_by_id(user.id).await.unwrap().unwrap();
        assert!(read_back.email_verified, "{driver:?}：布尔列要能往返");
        assert_eq!(read_back.status, status::ACTIVE);
        assert_eq!(read_back.quota_bytes, 1024 * 1024);

        // ---- 2. 唯一约束冲突要能被识别（UID 重复） ----
        let duplicate = NewUser {
            uid: uid.clone(),
            email: format!("dialect-dup-{suffix}@example.com"),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: status::ACTIVE.to_string(),
            quota_bytes: 1,
            email_verified: true,
        };
        let err = repo
            .create_user(&duplicate)
            .await
            .expect_err("UID 重复应当被唯一约束拦下");
        assert!(
            err.is_unique_violation(),
            "{driver:?}：唯一冲突要能被识别，实际是 {err}"
        );

        // ---- 3. UPSERT：设置表里同一个键写两次不能插出第二行 ----
        repo.set_setting_plain("dialect.probe", "one").await.unwrap();
        repo.set_setting_plain("dialect.probe", "two").await.unwrap();
        let value = repo.get_setting_value("dialect.probe").await.unwrap();
        assert_eq!(value.as_deref(), Some("two"), "{driver:?}：UPSERT 要覆盖而不是新增");

        // ---- 4. 内容寻址：同一个哈希登记两次只留一行，且带 ref_count 语义 ----
        let sha = format!("{:064x}", index + 1);
        let blob = repo
            .insert_blob_if_absent(&NewBlob {
                sha256: sha.clone(),
                kind: "original".to_string(),
                size: 2048,
                mime_type: "image/png".to_string(),
                ext: "png".to_string(),
                width: Some(64),
                height: Some(64),
                storage_driver: "local".to_string(),
                storage_key: format!("blobs/00/{sha}.png"),
                backend_id: None,
            })
            .await
            .unwrap();
        let again = repo
            .insert_blob_if_absent(&NewBlob {
                sha256: sha.clone(),
                kind: "original".to_string(),
                size: 2048,
                mime_type: "image/png".to_string(),
                ext: "png".to_string(),
                width: None,
                height: None,
                storage_driver: "local".to_string(),
                storage_key: format!("blobs/00/{sha}.png"),
                backend_id: None,
            })
            .await
            .unwrap();
        assert_eq!(blob.id, again.id, "{driver:?}：同内容实体必须复用同一行");
        assert_eq!(
            repo.count_blobs().await.unwrap() >= 1,
            true,
            "{driver:?}：实体计数"
        );

        // 引用计数增减与归零判断
        repo.inc_blob_ref(blob.id).await.unwrap();
        repo.inc_blob_ref(blob.id).await.unwrap();
        let after_inc = repo.find_blob_by_id(blob.id).await.unwrap().unwrap();
        assert_eq!(after_inc.ref_count, 2, "{driver:?}：引用计数加一");
        repo.dec_blob_ref(blob.id).await.unwrap();
        repo.dec_blob_ref(blob.id).await.unwrap();
        repo.dec_blob_ref(blob.id).await.unwrap(); // 再减一次不该变成负数
        let after_dec = repo.find_blob_by_id(blob.id).await.unwrap().unwrap();
        assert_eq!(after_dec.ref_count, 0, "{driver:?}：引用计数减到 0 就停住");

        // GC 候选要能按时间窗口筛出来
        let candidates = repo
            .list_gc_candidates(now() + chrono::Duration::minutes(1), 10)
            .await
            .unwrap();
        assert!(
            candidates.iter().any(|item| item.id == blob.id),
            "{driver:?}：无引用实体应当出现在 GC 候选里"
        );

        // ---- 5. 批量哈希预检：占位符改写是这一处最容易出错的地方 ----
        // 此时实体还在、但没有任何活着的引用（第 4 步把计数减到了 0，
        // 图片是下一步才建的）：预检必须报「不存在」——
        // 用户删光图片后重传同一批时，靠的就是这个判定，不能只看 blobs 表
        let before_image = repo
            .existing_hashes(user.id, &[sha.clone(), format!("{:064x}", 9999)])
            .await
            .unwrap();
        assert!(
            before_image.exist.is_empty(),
            "{driver:?}：没有活着的引用时预检不该命中"
        );
        assert_eq!(before_image.missing.len(), 2, "{driver:?}：预检漏报");

        // ---- 6. 相册与图片引用：多表 JOIN、可空外键、CAS 路径 ----
        let album = repo
            .insert_default_album(user.id, "默认相册")
            .await
            .unwrap();
        let image = repo
            .create_image_reference(
                &pixora_repo::repo::images::NewImage {
                    owner_id: user.id,
                    album_id: Some(album.id),
                    blob_id: blob.id,
                    thumb_blob_id: None,
                    original_name: "IMG_0001.png".to_string(),
                    title: Some("测试图".to_string()),
                    visibility: visibility::PUBLIC.to_string(),

                    upload_ip: None,

                    upload_agent: None,
                },
                2048,
                &["风景".to_string()],
            )
            .await
            .unwrap();

        // 有了活着的引用，预检这才该命中（同一个哈希，前后两种结论）
        let after_image = repo
            .existing_hashes(user.id, &[sha.clone(), format!("{:064x}", 9999)])
            .await
            .unwrap();
        assert_eq!(
            after_image.exist,
            vec![sha.clone()],
            "{driver:?}：有活引用时预检命中"
        );
        assert_eq!(after_image.missing.len(), 1, "{driver:?}：预检漏报");

        // 别人家的图不算数：预检按调用者自己的图库判
        let other = repo
            .existing_hashes(user.id + 9999, &[sha.clone()])
            .await
            .unwrap();
        assert!(
            other.exist.is_empty(),
            "{driver:?}：别的账号的图不该让本账号的预检命中"
        );

        let view = repo.find_image_view(image.id).await.unwrap().unwrap();
        assert_eq!(view.sha256, sha, "{driver:?}：视图里的哈希");
        assert_eq!(view.tags, vec!["风景".to_string()], "{driver:?}：标签关联表");
        assert_eq!(view.album_name.as_deref(), Some("默认相册"));

        // 按标签筛 + 模糊查询：走的是子查询与 LIKE ESCAPE，方言差异最容易暴露
        let by_tag = repo
            .list_images(
                &pixora_repo::repo::images::ImageFilter {
                    tag: Some("风景".to_string()),
                    public_only: true,
                    ..Default::default()
                },
                PageRequest::new(Some(1), Some(10), 10, 50),
            )
            .await
            .unwrap();
        assert_eq!(by_tag.total, 1, "{driver:?}：按标签筛选");

        let by_query = repo
            .list_images(
                &pixora_repo::repo::images::ImageFilter {
                    query: Some("测试".to_string()),
                    ..Default::default()
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(by_query.total, 1, "{driver:?}：标题模糊查询");

        // ---- 7. 时间戳的字典序 = 时间序：按天分桶靠 substr ----
        let trend = repo.upload_trend(7).await.unwrap();
        assert!(
            trend.iter().any(|point| point.count >= 1),
            "{driver:?}：趋势分桶应当统计到刚上传的这张图"
        );

        // ---- 8. 分页与计数 ----
        let users = repo
            .list_users(&UserFilter::default(), PageRequest::new(Some(1), Some(5), 20, 100))
            .await
            .unwrap();
        assert!(users.total >= 1, "{driver:?}：用户列表总数");

        // ---- 9. 汇总查询（SUM/COALESCE 在各家返回类型不同） ----
        let used = repo.sum_used_bytes().await.unwrap();
        assert!(used >= 2048, "{driver:?}：用量汇总 {used}");
        let allocated = repo.sum_allocated_quota(None).await.unwrap();
        assert!(allocated >= 1024 * 1024, "{driver:?}：配额汇总");

        // ---- 10. 时间戳写入与回读（文本形态，三方言语义一致） ----
        let stamp = now_ts();
        repo.db()
            .execute(
                "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?)",
                Args::new()
                    .text(format!("dialect.ts.{suffix}"))
                    .text(stamp.clone())
                    .bool(false)
                    .text(stamp.clone()),
            )
            .await
            .unwrap();
        let stored = repo
            .get_setting_value(&format!("dialect.ts.{suffix}"))
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some(stamp.as_str()));

        // 结束前清掉这个方言上的痕迹（SQLite 直接删文件，PG/MySQL 删行）
        let _ = repo
            .db()
            .execute(
                "DELETE FROM images WHERE id = ?",
                Args::new().int(image.id),
            )
            .await;
        let _ = repo.delete_blob(blob.id).await;
        let _ = repo.delete_album(album.id).await;
        let _ = repo.delete_user(user.id).await;
        repo.db().close().await;

        eprintln!("{driver:?} 方言验证通过");
    }
}
