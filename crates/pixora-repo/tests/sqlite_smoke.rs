//! SQLite 端到端冒烟测试。
//!
//! 这里跑的是真实建表脚本 + 真实 SQL，任何跨方言的类型或占位符问题都会在这里暴露。
//! PostgreSQL / MySQL 上的同类验证在 tests/dialects.rs：把连接串交给
//! `PIXORA_TEST_DB_DSN` 就会跑，不设则跳过。

use std::sync::Arc;

use pixora_repo::db::{Db, DbDriver};
use pixora_repo::model::{now_ts, status};
use pixora_repo::repo::{PageRequest, Repo};
use pixora_repo::{Args, NewUser, UserFilter};

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

#[tokio::test]
async fn migrations_create_full_schema() {
    let (repo, _dir) = sqlite_repo().await;
    // 表清单与设计稿一致
    for table in [
        "users",
        "user_actions",
        "sessions",
        "email_verifications",
        "disposable_domains",
        "email_templates",
        "mail_outbox",
        "albums",
        "blobs",
        "images",
        "tags",
        "image_tags",
        "upload_jobs",
        "api_tokens",
        "rate_limit_policies",
        "rate_counters",
        "ip_rules",
        "terms_versions",
        "terms_consents",
        "settings",
        "storage_backends",
        "audit_logs",
    ] {
        let total = repo
            .db()
            .count(&format!("SELECT COUNT(*) AS total FROM {table}"), Args::new())
            .await
            .expect(table);
        assert_eq!(total, 0, "{table} 应该是空表");
    }
}

#[tokio::test]
async fn user_roundtrip_covers_every_column_type() {
    let (repo, _dir) = sqlite_repo().await;
    let created = repo
        .create_user(&sample_user("chenyu@example.com", "1001"))
        .await
        .expect("建用户");

    assert_eq!(created.uid, "1001");
    assert_eq!(created.email, "chenyu@example.com");
    assert!(created.email_verified, "布尔列要能正确往返");
    assert!(created.is_active());
    assert!(!created.is_admin());
    assert_eq!(created.used_bytes, 0);
    assert_eq!(created.quota_bytes, 10 * 1024 * 1024 * 1024);

    // 按三个键都能查到同一个人
    let by_id = repo.find_user_by_id(created.id).await.unwrap().unwrap();
    let by_email = repo.find_user_by_email("chenyu@example.com").await.unwrap().unwrap();
    let by_uid = repo.find_user_by_uid("1001").await.unwrap().unwrap();
    assert_eq!(by_id.id, by_email.id);
    assert_eq!(by_email.id, by_uid.id);

    assert!(repo.email_exists("chenyu@example.com").await.unwrap());
    assert!(!repo.email_exists("other@example.com").await.unwrap());
    assert!(repo.uid_exists("1001").await.unwrap());
}

#[tokio::test]
async fn unique_email_conflict_is_recognized() {
    let (repo, _dir) = sqlite_repo().await;
    repo.create_user(&sample_user("dup@example.com", "2001"))
        .await
        .unwrap();
    let err = repo
        .create_user(&sample_user("dup@example.com", "2002"))
        .await
        .expect_err("同邮箱应该被唯一约束拦下");
    assert!(err.is_unique_violation(), "错误类型要能被识别：{err}");
}

#[tokio::test]
async fn status_quota_and_login_bookkeeping() {
    let (repo, _dir) = sqlite_repo().await;
    let user = repo
        .create_user(&sample_user("a@example.com", "3001"))
        .await
        .unwrap();

    repo.update_user_status(
        user.id,
        status::SUSPENDED,
        Some("上传违规内容"),
        None,
        pixora_repo::model::now(),
    )
    .await
    .unwrap();
    let suspended = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert!(suspended.is_suspended());
    assert_eq!(suspended.status_reason.as_deref(), Some("上传违规内容"));
    assert!(suspended.status_changed_at.is_some(), "时间戳要能往返");

    repo.record_login_failure(user.id, 3, None).await.unwrap();
    let failed = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(failed.login_fail_count, 3);

    repo.record_login_success(user.id, Some("203.0.113.18"))
        .await
        .unwrap();
    let ok = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(ok.login_fail_count, 0);
    assert_eq!(ok.last_login_ip.as_deref(), Some("203.0.113.18"));

    repo.add_used_bytes(user.id, 4096).await.unwrap();
    repo.add_used_bytes(user.id, -8192).await.unwrap();
    let clamped = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(clamped.used_bytes, 0, "用量不允许被减成负数");

    repo.update_user_quota(user.id, 2048).await.unwrap();
    let quota = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert_eq!(quota.remaining_quota(), 2048);

    repo.set_totp(user.id, Some("GEZDGNBVGY3TQOJQ"), true)
        .await
        .unwrap();
    let totp = repo.find_user_by_id(user.id).await.unwrap().unwrap();
    assert!(totp.totp_enabled);
    assert_eq!(totp.totp_secret.as_deref(), Some("GEZDGNBVGY3TQOJQ"));
}

#[tokio::test]
async fn listing_and_filters() {
    let (repo, _dir) = sqlite_repo().await;
    for (index, (email, uid)) in [
        ("admin@example.com", "1000"),
        ("chenyu@example.com", "1001"),
        ("design@studio.io", "2044"),
        ("photo@agency.cn", "3087"),
    ]
    .iter()
    .enumerate()
    {
        let mut new = sample_user(email, uid);
        new.role = if index == 0 { "admin" } else { "user" }.to_string();
        new.status = if index == 3 {
            status::PENDING.to_string()
        } else {
            status::ACTIVE.to_string()
        };
        repo.create_user(&new).await.unwrap();
    }

    let all = repo
        .list_users(&UserFilter::default(), PageRequest::new(None, None, 24, 100))
        .await
        .unwrap();
    assert_eq!(all.total, 4);
    assert_eq!(all.items.len(), 4);
    assert!(all.items[0].is_admin(), "按 id 升序，管理员在最前");

    let search = repo
        .list_users(
            &UserFilter {
                query: Some("studio".into()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(search.total, 1);
    assert_eq!(search.items[0].email, "design@studio.io");

    let by_uid = repo
        .list_users(
            &UserFilter {
                query: Some("3087".into()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(by_uid.total, 1);

    let pending = repo
        .list_users(
            &UserFilter {
                status: Some(status::PENDING.into()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(pending.total, 1);
    assert_eq!(pending.items[0].uid, "3087");

    let paged = repo
        .list_users(&UserFilter::default(), PageRequest::new(Some(2), Some(2), 24, 100))
        .await
        .unwrap();
    assert_eq!(paged.total, 4);
    assert_eq!(paged.items.len(), 2);
    assert_eq!(paged.items[0].uid, "2044");
    assert_eq!(paged.total_pages(), 2);

    let sum = repo.sum_allocated_quota(None).await.unwrap();
    assert_eq!(sum, 4 * 10 * 1024 * 1024 * 1024);
}

#[tokio::test]
async fn user_actions_are_recorded() {
    let (repo, _dir) = sqlite_repo().await;
    let user = repo
        .create_user(&sample_user("spam@tempmail.xyz", "4120"))
        .await
        .unwrap();
    let detail = serde_json::json!({"from": "active", "to": "suspended"});
    repo.insert_user_action(
        user.id,
        None,
        "suspend",
        Some("上传违规内容，违反服务条款第 3 条"),
        &detail,
        false,
        pixora_repo::model::now(),
    )
    .await
    .unwrap();

    let actions = repo.list_user_actions(user.id, 10).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action_label(), "暂停");
    assert_eq!(actions[0].detail["to"], "suspended", "JSON 列要能往返");
    assert!(!actions[0].mail_sent);
}

#[tokio::test]
async fn transaction_and_ping() {
    let (repo, _dir) = sqlite_repo().await;
    repo.db().ping().await.expect("ping");

    let mut tx = repo.db().pool().begin().await.unwrap();
    let sql = repo
        .db()
        .sql("INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?)");
    sqlx::query(&sql)
        .bind("site.name")
        .bind("Pixora")
        .bind(false)
        .bind(now_ts())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let row = repo
        .db()
        .fetch_one(
            "SELECT value FROM settings WHERE name = ?",
            Args::new().text("site.name"),
        )
        .await
        .unwrap();
    let value: String = sqlx::Row::try_get(&row, "value").unwrap();
    assert_eq!(value, "Pixora");
}

#[tokio::test]
async fn first_admin_creation_is_atomic() {
    let (repo, _dir) = sqlite_repo().await;
    let mut first = sample_user("first@example.com", "9001");
    first.role = "admin".to_string();
    let mut second = sample_user("second@example.com", "9002");
    second.role = "admin".to_string();

    let (a, b) = tokio::join!(
        repo.create_first_admin(&first),
        repo.create_first_admin(&second)
    );
    let a = a.expect("第一个请求");
    let b = b.expect("第二个请求");
    assert_eq!(
        usize::from(a.is_some()) + usize::from(b.is_some()),
        1,
        "并发创建安装管理员只能有一个成功"
    );
    let admins = repo
        .list_users(
            &UserFilter {
                role: Some("admin".to_string()),
                ..Default::default()
            },
            PageRequest::new(Some(1), Some(10), 10, 10),
        )
        .await
        .unwrap();
    assert_eq!(admins.total, 1, "库里只能有一个安装管理员");
}
