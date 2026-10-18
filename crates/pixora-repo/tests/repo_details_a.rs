//! 相册 / 任务 / 令牌 / 验证码 / 邮件五个仓储模块的 SQLite 集成测试。
//!
//! 与 `sqlite_smoke.rs` 同一套做法：真建库、真跑迁移、真发 SQL。
//! 这里刻意不 mock——这五个模块的绝大部分语义（唯一约束、外键置空、
//! 文本时间戳的字典序比较、UPSERT 的方言分流）只有在真库上才验得出来。

use std::sync::Arc;

use chrono::Duration;

use pixora_repo::db::{Db, DbDriver};
use pixora_repo::model::{job_status, now, status, ts, visibility};
use pixora_repo::repo::blobs::NewBlob;
use pixora_repo::repo::verifications::purpose;
use pixora_repo::repo::{PageRequest, Repo};
use pixora_repo::{
    Args, AlbumFilter, NewAlbum, NewApiToken, NewImage, NewJob, NewUser, UserFilter,
};

async fn sqlite_repo() -> (Repo, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let dsn = dir.path().join("pixora-test.db");
    let db = Db::connect(DbDriver::Sqlite, dsn.to_str().unwrap(), 4)
        .await
        .expect("连接 SQLite");
    db.migrate().await.expect("跑迁移");
    (Repo::new(Arc::new(db)), dir)
}

fn sample_user(email: &str, uid: &str) -> NewUser {
    NewUser {
        uid: uid.to_string(),
        email: email.to_string(),
        password_hash: "$argon2id$fake".to_string(),
        role: "user".to_string(),
        status: status::ACTIVE.to_string(),
        quota_bytes: 10 * 1024 * 1024 * 1024,
        email_verified: true,
    }
}

/// 相册、任务、令牌都挂在用户上，且外键是真的开着的（PRAGMA foreign_keys=ON），
/// 所以每个用例都先落一个账号。
async fn seed_user(repo: &Repo, email: &str, uid: &str) -> i64 {
    repo.create_user(&sample_user(email, uid))
        .await
        .expect("建用户")
        .id
}

fn sample_album(owner_id: i64, name: &str, slug: &str) -> NewAlbum {
    NewAlbum {
        owner_id,
        name: name.to_string(),
        slug: slug.to_string(),
        description: None,
        visibility: visibility::PRIVATE.to_string(),
        sort_order: 0,
    }
}

fn sample_job(owner_id: i64, name: &str, size: i64) -> NewJob {
    NewJob {
        owner_id,
        album_id: None,
        sha256: Some(format!("{name}-sha")),
        original_name: name.to_string(),
        size,
    }
}

// ============================================================
// 相册
// ============================================================

#[tokio::test]
async fn album_crud_lookup_and_pagination() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "album@example.com", "7001").await;

    let mut travel = sample_album(owner, "旅行", "travel");
    travel.description = Some("2026 年的两次远行".into());
    let travel = repo.create_album(&travel).await.expect("建相册");

    assert_eq!(travel.name, "旅行");
    assert_eq!(travel.slug, "travel");
    assert_eq!(travel.description.as_deref(), Some("2026 年的两次远行"));
    assert_eq!(travel.image_count, 0, "新相册计数从 0 起");
    assert_eq!(travel.cover_blob_id, None);
    assert!(!travel.is_public());
    assert_eq!(travel.created_at, travel.updated_at);
    assert!(repo.slug_exists(owner, "travel").await.unwrap());
    assert!(!repo.slug_exists(owner, "missing").await.unwrap());

    // 按主键与按 (owner, slug) 都能命中
    let by_id = repo.find_album_by_id(travel.id).await.unwrap().unwrap();
    let by_slug = repo
        .find_album_by_slug(owner, "travel")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_id.id, by_slug.id);
    // slug 只在同一用户名下唯一，别人查不到
    let other = seed_user(&repo, "other@example.com", "7002").await;
    assert!(repo.find_album_by_slug(other, "travel").await.unwrap().is_none());

    // slug 冲突要让调用方能识别
    let err = repo
        .create_album(&sample_album(owner, "旅行副本", "travel"))
        .await
        .expect_err("同一用户下 slug 不能重复");
    assert!(err.is_unique_violation(), "错误类型要能被识别：{err}");

    // 列表：排序按 sort_order，再按 id；公开过滤与关键词过滤各自生效
    let mut public_album = sample_album(owner, "公开的旅行", "public-travel");
    public_album.visibility = visibility::PUBLIC.to_string();
    public_album.sort_order = -1;
    let public_album = repo.create_album(&public_album).await.unwrap();
    repo.create_album(&sample_album(owner, "夜景", "night"))
        .await
        .unwrap();

    let all = repo
        .list_albums(&AlbumFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(all.total, 3);
    assert_eq!(all.items[0].id, public_album.id, "sort_order 小的排前面");
    assert_eq!(all.items[1].id, travel.id);

    let paged = repo
        .list_albums(
            &AlbumFilter {
                owner_id: Some(owner),
                ..Default::default()
            },
            PageRequest::new(Some(2), Some(2), 24, 100),
        )
        .await
        .unwrap();
    assert_eq!(paged.total, 3);
    assert_eq!(paged.items.len(), 1);
    assert_eq!(paged.total_pages(), 2);
    assert_eq!(paged.items[0].name, "夜景");

    let public_only = repo
        .list_albums(
            &AlbumFilter {
                owner_id: Some(owner),
                public_only: true,
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(public_only.total, 1);
    assert!(public_only.items[0].is_public());

    let searched = repo
        .list_albums(
            &AlbumFilter {
                owner_id: Some(owner),
                query: Some("旅行".into()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(searched.total, 2, "名称与描述都参与模糊匹配");

    let full = repo.list_albums_all(owner).await.unwrap();
    assert_eq!(full.len(), 3, "侧边栏要全量");
    assert_eq!(repo.count_albums(owner).await.unwrap(), 3);
    assert_eq!(repo.count_albums(other).await.unwrap(), 0);
}

#[tokio::test]
async fn album_update_cover_and_delete() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "album2@example.com", "7011").await;

    let album = repo
        .create_album(&sample_album(owner, "旧名字", "rename-me"))
        .await
        .unwrap();

    repo.update_album(
        album.id,
        &pixora_repo::repo::albums::AlbumPatch {
            name: Some("新名字".into()),
            description: Some(Some("补上说明".into())),
            visibility: Some(visibility::PUBLIC.to_string()),
            sort_order: Some(5),
            cover_blob_id: None,
        },
    )
    .await
    .unwrap();

    let updated = repo.find_album_by_id(album.id).await.unwrap().unwrap();
    assert_eq!(updated.name, "新名字");
    assert_eq!(updated.description.as_deref(), Some("补上说明"));
    assert!(updated.is_public());
    assert_eq!(updated.sort_order, 5);
    assert_eq!(updated.created_at, album.created_at, "改名不动建立时间");
    assert!(updated.updated_at >= album.updated_at);

    // 封面：先设后清
    let blob = repo
        .insert_blob_if_absent(&NewBlob {
            sha256: "c".repeat(64),
            kind: "thumb".into(),
            size: 2048,
            mime_type: "image/webp".into(),
            ext: "webp".into(),
            width: Some(320),
            height: Some(240),
            storage_driver: "local".into(),
            storage_key: "thumbs/cc.webp".into(),
            backend_id: None,
        })
        .await
        .unwrap();
    repo.set_album_cover(album.id, Some(blob.id)).await.unwrap();
    let with_cover = repo.find_album_by_id(album.id).await.unwrap().unwrap();
    assert_eq!(with_cover.cover_blob_id, Some(blob.id));
    repo.set_album_cover(album.id, None).await.unwrap();
    let no_cover = repo.find_album_by_id(album.id).await.unwrap().unwrap();
    assert_eq!(no_cover.cover_blob_id, None);

    // 删除相册：相册里的图片不删，归属被外键置空
    let image_blob = repo
        .insert_blob_if_absent(&NewBlob {
            sha256: "d".repeat(64),
            kind: "original".into(),
            size: 4096,
            mime_type: "image/jpeg".into(),
            ext: "jpg".into(),
            width: Some(1600),
            height: Some(1200),
            storage_driver: "local".into(),
            storage_key: "originals/dd.jpg".into(),
            backend_id: None,
        })
        .await
        .unwrap();
    let image = repo
        .create_image_reference(
            &NewImage {
                owner_id: owner,
                album_id: Some(album.id),
                blob_id: image_blob.id,
                thumb_blob_id: None,
                original_name: "sunset.jpg".into(),
                title: None,
                visibility: visibility::PRIVATE.to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            image_blob.size,
            &[],
        )
        .await
        .unwrap();
    let counted = repo.find_album_by_id(album.id).await.unwrap().unwrap();
    assert_eq!(counted.image_count, 1, "建引用时增量维护相册计数");

    repo.delete_album(album.id).await.unwrap();
    assert!(repo.find_album_by_id(album.id).await.unwrap().is_none());
    let detached = repo.find_image(image.id).await.unwrap().unwrap();
    assert_eq!(detached.album_id, None, "删相册不删图片，只把归属置空");
}

#[tokio::test]
async fn album_recount_repairs_drift() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "album3@example.com", "7021").await;

    let album = repo
        .create_album(&sample_album(owner, "对账", "recount"))
        .await
        .unwrap();
    let blob = repo
        .insert_blob_if_absent(&NewBlob {
            sha256: "e".repeat(64),
            kind: "original".into(),
            size: 1024,
            mime_type: "image/png".into(),
            ext: "png".into(),
            width: None,
            height: None,
            storage_driver: "local".into(),
            storage_key: "originals/ee.png".into(),
            backend_id: None,
        })
        .await
        .unwrap();
    for name in ["one.png", "two.png"] {
        repo.create_image_reference(
            &NewImage {
                owner_id: owner,
                album_id: Some(album.id),
                blob_id: blob.id,
                thumb_blob_id: None,
                original_name: name.into(),
                title: None,
                visibility: visibility::PRIVATE.to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            blob.size,
            &[],
        )
        .await
        .unwrap();
    }

    // 模拟计数漂移（比如手工改库、或早期版本的 bug）
    repo.db()
        .execute(
            "UPDATE albums SET image_count = 99 WHERE id = ?",
            Args::new().int(album.id),
        )
        .await
        .unwrap();
    let repaired = repo.recount_album_images(album.id).await.unwrap();
    assert_eq!(repaired, 2, "按 images 表重算");

    // 全库对账只改数值不一致的行
    let clean = repo
        .create_album(&sample_album(owner, "干净", "clean"))
        .await
        .unwrap();
    let moved = repo.recount_all_album_images().await.unwrap();
    assert_eq!(moved, 0, "已经一致的相册不该被改");
    let still_two = repo.find_album_by_id(album.id).await.unwrap().unwrap();
    assert_eq!(still_two.image_count, 2);
    assert_eq!(clean.image_count, 0);
}

#[tokio::test]
async fn default_album_is_idempotent() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "default@example.com", "7031").await;

    let first = repo.insert_default_album(owner, "默认相册").await.unwrap();
    let second = repo.insert_default_album(owner, "默认相册").await.unwrap();

    assert_eq!(first.id, second.id, "重复调用返回同一行");
    assert_eq!(first.slug, "default");
    assert_eq!(first.name, "默认相册");
    assert!(!first.is_public());
    assert_eq!(repo.count_albums(owner).await.unwrap(), 1);

    // 每个用户各有自己的默认相册
    let other = seed_user(&repo, "default2@example.com", "7032").await;
    let theirs = repo.insert_default_album(other, "默认相册").await.unwrap();
    assert_ne!(theirs.id, first.id);
    assert_eq!(theirs.owner_id, other);
    assert_eq!(repo.count_albums(owner).await.unwrap(), 1);
}

// ============================================================
// 上传任务
// ============================================================

#[tokio::test]
async fn job_lifecycle_and_queue_depth() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "jobs@example.com", "7101").await;

    let first = repo.create_job(&sample_job(owner, "a.jpg", 1024)).await.unwrap();
    let second = repo.create_job(&sample_job(owner, "b.jpg", 2048)).await.unwrap();
    assert_eq!(first.status, job_status::QUEUED);
    assert!(!first.dedup);
    assert_eq!(first.attempts, 0);
    assert_eq!(first.started_at, None);
    assert_eq!(first.finished_at, None);

    let depth = repo.queue_depth().await.unwrap();
    assert_eq!((depth.queued, depth.running), (2, 0));
    assert_eq!(depth.total(), 2);

    // 开始处理：状态、开始时间、尝试次数一起变
    repo.mark_job_running(first.id).await.unwrap();
    let running = repo.find_job(first.id).await.unwrap().unwrap();
    assert_eq!(running.status, job_status::RUNNING);
    assert!(running.started_at.is_some());
    assert_eq!(running.attempts, 1);
    let depth = repo.queue_depth().await.unwrap();
    assert_eq!((depth.queued, depth.running), (1, 1));

    // 收尾：一个落盘成功，一个失败
    repo.mark_job_done(first.id, 42, "a-sha", false).await.unwrap();
    let done = repo.find_job(first.id).await.unwrap().unwrap();
    assert_eq!(done.status, job_status::DONE);
    assert_eq!(done.image_id, Some(42));
    assert_eq!(done.sha256.as_deref(), Some("a-sha"));
    assert!(!done.dedup);
    assert!(done.finished_at.is_some());

    repo.mark_job_failed(second.id, "磁盘写满").await.unwrap();
    let failed = repo.find_job(second.id).await.unwrap().unwrap();
    assert_eq!(failed.status, job_status::FAILED);
    assert_eq!(failed.error.as_deref(), Some("磁盘写满"));
    assert!(failed.finished_at.is_some());

    let depth = repo.queue_depth().await.unwrap();
    assert_eq!(depth.total(), 0, "终态任务不算队列深度");

    let listed = repo.list_jobs(owner, 10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second.id, "按建立时间倒序");

    // 清理只删终态，排队中的一条都不能碰
    let pending = repo.create_job(&sample_job(owner, "c.jpg", 512)).await.unwrap();
    let removed = repo.purge_jobs_before(now() + Duration::hours(1)).await.unwrap();
    assert_eq!(removed, 2);
    assert!(repo.find_job(first.id).await.unwrap().is_none());
    assert!(repo.find_job(second.id).await.unwrap().is_none());
    assert!(repo.find_job(pending.id).await.unwrap().is_some());
}

#[tokio::test]
async fn job_rate_limit_counters_skip_dedup_and_other_owners() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "rate@example.com", "7111").await;
    let stranger = seed_user(&repo, "stranger@example.com", "7112").await;

    // 真正落盘的新文件
    let fresh = repo.create_job(&sample_job(owner, "fresh.jpg", 300)).await.unwrap();
    repo.mark_job_done(fresh.id, 1, "fresh-sha", false).await.unwrap();
    // 秒传命中：服务器早有这份字节，不算新文件
    let deduped = repo.create_job(&sample_job(owner, "dup.jpg", 700)).await.unwrap();
    repo.mark_job_done(deduped.id, 2, "dup-sha", true).await.unwrap();
    // 失败的不算
    let failed = repo.create_job(&sample_job(owner, "bad.jpg", 500)).await.unwrap();
    repo.mark_job_failed(failed.id, "校验和不符").await.unwrap();
    // 排队中的不算
    repo.create_job(&sample_job(owner, "queued.jpg", 900)).await.unwrap();
    // 别人的不算
    let other = repo.create_job(&sample_job(stranger, "theirs.jpg", 50)).await.unwrap();
    repo.mark_job_done(other.id, 3, "theirs-sha", false).await.unwrap();

    let since = now() - Duration::minutes(5);
    assert_eq!(
        repo.count_new_files_since(owner, since).await.unwrap(),
        1,
        "只算 status=done 且 dedup=false 的"
    );
    assert_eq!(repo.sum_bytes_since(owner, since).await.unwrap(), 300);
    assert_eq!(repo.count_new_files_since(stranger, since).await.unwrap(), 1);

    // 窗口往后挪，什么都不该落进来；这也顺带验证时间戳按字典序比较
    let future = now() + Duration::hours(1);
    assert_eq!(repo.count_new_files_since(owner, future).await.unwrap(), 0);
    assert_eq!(repo.sum_bytes_since(owner, future).await.unwrap(), 0);
}

#[tokio::test]
async fn stale_running_jobs_are_requeued() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "stale@example.com", "7121").await;

    let job = repo.create_job(&sample_job(owner, "stuck.jpg", 1024)).await.unwrap();
    repo.mark_job_running(job.id).await.unwrap();

    let future = now() + Duration::hours(1);
    let stale = repo.list_stale_running(future, 10).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, job.id);

    // 刚开跑的任务不该被回收
    let fresh_cutoff = now() - Duration::hours(1);
    assert_eq!(repo.requeue_stale(fresh_cutoff).await.unwrap(), 0);
    assert_eq!(repo.list_stale_running(future, 10).await.unwrap().len(), 1);

    // 超时后回收：回到 queued 等重新拾取，尝试次数保留
    assert_eq!(repo.requeue_stale(future).await.unwrap(), 1);
    let requeued = repo.find_job(job.id).await.unwrap().unwrap();
    assert_eq!(requeued.status, job_status::QUEUED);
    assert_eq!(requeued.started_at, None);
    assert_eq!(requeued.attempts, 1, "重试次数累计在账上");

    assert!(repo.list_stale_running(future, 10).await.unwrap().is_empty());
    assert_eq!(
        repo.requeue_stale(future).await.unwrap(),
        0,
        "已经回收过的不再重复计数"
    );
    let depth = repo.queue_depth().await.unwrap();
    assert_eq!(depth.queued, 1, "回收后重新出现在队列里");
}

// ============================================================
// API 令牌
// ============================================================

#[tokio::test]
async fn api_token_lifecycle() {
    let (repo, _dir) = sqlite_repo().await;
    let owner = seed_user(&repo, "tokens@example.com", "7201").await;

    let first = repo
        .create_api_token(&NewApiToken {
            user_id: owner,
            name: "备份脚本".into(),
            token_hash: "hash-first".into(),
            display: "px_live_9f2a…".into(),
            scopes: vec!["upload".into(), "read".into()],
            expires_at: None,
        })
        .await
        .unwrap();
    let second = repo
        .create_api_token(&NewApiToken {
            user_id: owner,
            name: "临时".into(),
            token_hash: "hash-second".into(),
            display: "px_live_11bb…".into(),
            scopes: vec!["read".into()],
            expires_at: Some(now() + Duration::days(30)),
        })
        .await
        .unwrap();

    assert_eq!(first.scopes, vec!["upload".to_string(), "read".to_string()], "JSON 列要能往返");
    assert_eq!(first.display, "px_live_9f2a…");
    assert!(first.is_usable(now()));
    assert!(first.allows("upload"));
    assert!(!second.allows("upload"));

    // 鉴权走哈希查找
    let by_hash = repo
        .find_api_token_by_hash("hash-first")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_hash.id, first.id);
    assert!(repo
        .find_api_token_by_hash("hash-missing")
        .await
        .unwrap()
        .is_none());

    let listed = repo.list_api_tokens(owner).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second.id, "按创建时间倒序");
    assert_eq!(repo.count_api_tokens(owner).await.unwrap(), 2);

    repo.touch_api_token(first.id).await.unwrap();
    let touched = repo.find_api_token_by_hash("hash-first").await.unwrap().unwrap();
    assert!(touched.last_used_at.is_some());

    // 撤销后不再是可用令牌，但行还在（留痕），列表也照列
    repo.revoke_api_token(first.id, owner).await.unwrap();
    let revoked = repo.find_api_token_by_hash("hash-first").await.unwrap().unwrap();
    assert!(revoked.revoked_at.is_some());
    assert!(!revoked.is_usable(now()));
    assert_eq!(repo.count_api_tokens(owner).await.unwrap(), 1);
    assert_eq!(repo.list_api_tokens(owner).await.unwrap().len(), 2);

    // 别人的令牌撤不动
    let stranger = seed_user(&repo, "token-thief@example.com", "7202").await;
    repo.revoke_api_token(second.id, stranger).await.unwrap();
    assert!(repo
        .find_api_token_by_hash("hash-second")
        .await
        .unwrap()
        .unwrap()
        .is_usable(now()));

    // 一次撤销全部有效令牌，只算真正被改动的行
    assert_eq!(repo.revoke_all_api_tokens(owner).await.unwrap(), 1);
    assert_eq!(repo.count_api_tokens(owner).await.unwrap(), 0);
    assert_eq!(repo.revoke_all_api_tokens(owner).await.unwrap(), 0);
    assert!(!repo
        .find_api_token_by_hash("hash-second")
        .await
        .unwrap()
        .unwrap()
        .is_usable(now()));
}

// ============================================================
// 邮箱验证码
// ============================================================

#[tokio::test]
async fn verification_issue_consume_and_attempts() {
    let (repo, _dir) = sqlite_repo().await;
    let email = "verify@example.com";
    let ip = "203.0.113.9";

    let issued = repo
        .create_email_verification(
            email,
            "code-hash-1",
            purpose::REGISTER,
            now() + Duration::minutes(10),
            Some(ip),
        )
        .await
        .unwrap();
    assert_eq!(issued.attempts, 0);
    assert_eq!(issued.consumed_at, None);
    assert_eq!(issued.request_ip.as_deref(), Some(ip));
    assert!(issued.is_usable(now()));

    // 重发：旧记录留着，取最新那条
    let resent = repo
        .create_email_verification(
            email,
            "code-hash-2",
            purpose::REGISTER,
            now() + Duration::minutes(10),
            Some(ip),
        )
        .await
        .unwrap();
    let latest = repo
        .find_latest_verification(email, purpose::REGISTER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, resent.id);
    assert_eq!(latest.code_hash, "code-hash-2");
    assert!(repo.find_verification(issued.id).await.unwrap().is_some(), "旧记录没被删");
    assert!(repo
        .find_latest_verification(email, purpose::RESET)
        .await
        .unwrap()
        .is_none());

    // 校验失败累加次数，返回累加后的值
    assert_eq!(repo.increment_verification_attempts(latest.id).await.unwrap(), 1);
    assert_eq!(repo.increment_verification_attempts(latest.id).await.unwrap(), 2);
    let counted = repo.find_verification(latest.id).await.unwrap().unwrap();
    assert_eq!(counted.attempts, 2);

    // 消费后不可再用；第二次消费必须明确返回 false
    let at = now();
    assert!(repo.consume_verification(latest.id, at).await.unwrap());
    assert!(!repo.consume_verification(latest.id, at).await.unwrap());
    let consumed = repo.find_verification(latest.id).await.unwrap().unwrap();
    assert!(consumed.consumed_at.is_some());
    assert!(!consumed.is_usable(at), "已消费的码立刻失效");
    assert!(!consumed.is_usable(at + Duration::seconds(1)));
}

#[tokio::test]
async fn verification_limits_and_purge() {
    let (repo, _dir) = sqlite_repo().await;
    let email = "limit@example.com";
    let ip = "198.51.100.7";

    let mut ids = Vec::new();
    for _ in 0..3 {
        let row = repo
            .create_email_verification(email, "h", purpose::RESET, now() + Duration::minutes(10), Some(ip))
            .await
            .unwrap();
        ids.push(row.id);
    }
    // 一个已经过期的码：过期不等于「没发过」，发送次数照算
    let expired = repo
        .create_email_verification(email, "h", purpose::RESET, now() - Duration::minutes(1), Some(ip))
        .await
        .unwrap();
    assert!(!expired.is_usable(now()), "过期即不可用");
    assert!(repo
        .find_latest_verification(email, purpose::RESET)
        .await
        .unwrap()
        .unwrap()
        .id
        == expired.id);

    let window = now() - Duration::minutes(5);
    assert_eq!(
        repo.count_recent_verifications_for_email(email, purpose::RESET, window)
            .await
            .unwrap(),
        4
    );

    // 把最早那条挪到窗口外，验证窗口边界是按 created_at 比的
    let long_ago = now() - Duration::hours(3);
    repo.db()
        .execute(
            "UPDATE email_verifications SET created_at = ? WHERE id = ?",
            Args::new().text(ts(long_ago)).int(ids[0]),
        )
        .await
        .unwrap();
    assert_eq!(
        repo.count_recent_verifications_for_email(email, purpose::RESET, window)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        repo.count_recent_verifications_for_email(email, purpose::RESET, long_ago - Duration::minutes(1))
            .await
            .unwrap(),
        4,
        "放长窗口又能看见它"
    );
    assert_eq!(
        repo.count_recent_verifications_for_email(email, purpose::REGISTER, window)
            .await
            .unwrap(),
        0,
        "用途要分开统计"
    );
    assert_eq!(
        repo.count_recent_verifications_for_email("nobody@example.com", purpose::RESET, window)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.count_recent_verifications_for_ip(ip, window).await.unwrap(),
        3,
        "IP 维度跨用途统计"
    );
    assert_eq!(
        repo.count_recent_verifications_for_ip("198.51.100.8", window).await.unwrap(),
        0
    );

    let purged = repo
        .purge_expired_verifications(now() + Duration::minutes(11))
        .await
        .unwrap();
    assert_eq!(purged, 4, "expires_at 早于 cutoff 的全部清掉");
    assert_eq!(
        repo.count_recent_verifications_for_ip(ip, long_ago - Duration::minutes(1))
            .await
            .unwrap(),
        0
    );
}

// ============================================================
// 邮件
// ============================================================

#[tokio::test]
async fn mail_outbox_delivery_and_retry() {
    let (repo, _dir) = sqlite_repo().await;

    let vars = serde_json::json!({"code": "483920", "ttl": 10});
    let first = repo
        .enqueue_mail("someone@example.com", "verify_code", &vars)
        .await
        .unwrap();
    let second = repo
        .enqueue_mail("someone@example.com", "verify_code", &vars)
        .await
        .unwrap();

    let queued = repo.find_mail(first).await.unwrap().unwrap();
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.attempts, 0);
    assert_eq!(queued.vars["code"], "483920", "JSON 列要能往返");
    assert!(queued.next_attempt_at.is_some());
    assert!(!queued.is_sent());
    assert_eq!(repo.count_pending_mail().await.unwrap(), 2);

    // 到点出队：按 id 升序，先入先发
    let due = repo.due_mails(now() + Duration::seconds(1), 10).await.unwrap();
    assert_eq!(due.len(), 2);
    assert_eq!(due[0].id, first);
    assert_eq!(due[1].id, second);

    repo.mark_mail_sent(first, now()).await.unwrap();
    let sent = repo.find_mail(first).await.unwrap().unwrap();
    assert!(sent.is_sent());
    assert!(sent.sent_at.is_some());
    assert_eq!(repo.count_pending_mail().await.unwrap(), 1);
    assert_eq!(
        repo.due_mails(now() + Duration::seconds(1), 10).await.unwrap().len(),
        1,
        "已发出的不再出队"
    );

    // 暂时性失败：留在队列里，按 next_attempt_at 退避
    let backoff = now() + Duration::hours(1);
    repo.mark_mail_failed(second, "SMTP 连接超时", Some(backoff), false)
        .await
        .unwrap();
    let retried = repo.find_mail(second).await.unwrap().unwrap();
    assert_eq!(retried.status, "queued", "还能重试就保持待发");
    assert_eq!(retried.attempts, 1);
    assert_eq!(retried.error.as_deref(), Some("SMTP 连接超时"));
    assert!(retried.next_attempt_at.is_some());
    assert!(
        repo.due_mails(now() + Duration::seconds(1), 10).await.unwrap().is_empty(),
        "退避期内不出队"
    );
    assert_eq!(
        repo.due_mails(backoff + Duration::seconds(1), 10).await.unwrap().len(),
        1,
        "到点后重新出队"
    );

    // 永久性失败：不再重试
    repo.mark_mail_failed(second, "收件地址不存在", None, true)
        .await
        .unwrap();
    let dead = repo.find_mail(second).await.unwrap().unwrap();
    assert_eq!(dead.status, "failed");
    assert_eq!(dead.attempts, 2);
    assert_eq!(dead.next_attempt_at, None);
    assert!(repo
        .due_mails(now() + Duration::days(1), 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(repo.count_pending_mail().await.unwrap(), 0);

    // 已发的信可以清理，待发的一条都不能删
    let pending = repo
        .enqueue_mail("later@example.com", "verify_code", &vars)
        .await
        .unwrap();
    let purged = repo.purge_sent_mail(now() + Duration::minutes(1)).await.unwrap();
    assert_eq!(purged, 1);
    assert!(repo.find_mail(first).await.unwrap().is_none());
    assert!(repo.find_mail(pending).await.unwrap().is_some());
}

#[tokio::test]
async fn email_templates_upsert_and_disposable_domains() {
    let (repo, _dir) = sqlite_repo().await;

    assert!(repo.find_email_template("verify_code").await.unwrap().is_none());
    repo.upsert_email_template("verify_code", "验证码 {code}", "你的验证码是 {code}")
        .await
        .unwrap();
    let created = repo.find_email_template("verify_code").await.unwrap().unwrap();
    assert_eq!(created.subject, "验证码 {code}");
    assert_eq!(created.body, "你的验证码是 {code}");

    // 同名再写就是覆盖，不新增行
    repo.upsert_email_template("verify_code", "【Pixora】验证码", "新的正文")
        .await
        .unwrap();
    let updated = repo.find_email_template("verify_code").await.unwrap().unwrap();
    assert_eq!(updated.subject, "【Pixora】验证码");
    assert_eq!(updated.body, "新的正文");
    assert!(updated.updated_at >= created.updated_at);

    repo.upsert_email_template("suspended", "账号已暂停", "原因：{reason}")
        .await
        .unwrap();
    let templates = repo.list_email_templates().await.unwrap();
    assert_eq!(templates.len(), 2);
    assert_eq!(templates[0].name, "suspended", "按名字升序");

    // 一次性域名：批量、幂等、批内去重
    assert!(!repo.disposable_domain_exists("mailinator.com").await.unwrap());
    let batch = vec![
        "mailinator.com".to_string(),
        "tempmail.xyz".to_string(),
        "  mailinator.com  ".to_string(),
        "".to_string(),
    ];
    assert_eq!(repo.insert_disposable_domains(&batch, "builtin").await.unwrap(), 2);
    assert_eq!(repo.count_disposable_domains().await.unwrap(), 2);
    assert!(repo.disposable_domain_exists("mailinator.com").await.unwrap());

    // 重复导入不报错也不重复计数
    assert_eq!(repo.insert_disposable_domains(&batch, "builtin").await.unwrap(), 0);
    assert_eq!(repo.count_disposable_domains().await.unwrap(), 2);
    assert_eq!(repo.insert_disposable_domains(&[], "builtin").await.unwrap(), 0);

    // 超过一批的上限时按块切分，块与块之间也要接着去重
    let many: Vec<String> = (0..450).map(|index| format!("bulk{index}.test")).collect();
    assert_eq!(repo.insert_disposable_domains(&many, "bulk").await.unwrap(), 450);
    assert_eq!(repo.count_disposable_domains().await.unwrap(), 452);
    assert_eq!(repo.insert_disposable_domains(&many, "bulk").await.unwrap(), 0);
}

// ============================================================
// 收尾：确认这批用例没有把别的表带脏
// ============================================================

#[tokio::test]
async fn seeded_entities_do_not_leak_into_other_owners() {
    let (repo, _dir) = sqlite_repo().await;
    let alice = seed_user(&repo, "alice@example.com", "7301").await;
    let bob = seed_user(&repo, "bob@example.com", "7302").await;

    repo.create_album(&sample_album(alice, "Alice 的相册", "alice"))
        .await
        .unwrap();
    let job = repo.create_job(&sample_job(alice, "alice.jpg", 10)).await.unwrap();
    repo.mark_job_done(job.id, 1, "sha", false).await.unwrap();
    repo.create_api_token(&NewApiToken {
        user_id: alice,
        name: "alice-token".into(),
        token_hash: "alice-hash".into(),
        display: "px_live_0000…".into(),
        scopes: vec!["upload".into()],
        expires_at: None,
    })
    .await
    .unwrap();

    assert!(repo.list_jobs(bob, 10).await.unwrap().is_empty());
    assert!(repo.list_api_tokens(bob).await.unwrap().is_empty());
    assert_eq!(repo.count_albums(bob).await.unwrap(), 0);
    assert_eq!(
        repo.count_new_files_since(bob, now() - Duration::minutes(1)).await.unwrap(),
        0
    );
    let everyone = repo
        .list_users(&UserFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(everyone.total, 2);
}
