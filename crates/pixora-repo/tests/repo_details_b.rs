//! 第二批仓储模块的 SQLite 集成测试：存储后端、IP 规则、限流策略与计数、
//! 统计聚合、服务条款、审计日志。
//!
//! 与 `sqlite_smoke.rs` 同一套搭台方式：临时库 + 真实迁移 + 真实 SQL，
//! 断言尽量落在「跨表一致性」这类只有真跑库才看得见的地方。

use std::sync::Arc;

use chrono::Duration;
use pixora_repo::db::{Db, DbDriver};
use pixora_repo::model::{now, now_ts, status, ts};
use pixora_repo::repo::audit::{AuditEntry, AuditFilter};
use pixora_repo::repo::backends::{BackendPatch, NewBackend};
use pixora_repo::repo::blobs::NewBlob;
use pixora_repo::repo::images::{ImageDeletion, ImageFilter};
use pixora_repo::repo::iprules::{IpRuleFilter, NewIpRule};
use pixora_repo::repo::ratelimit::{kind, scope};
use pixora_repo::repo::terms::NewTermsVersion;
use pixora_repo::repo::{PageRequest, Repo};
use pixora_repo::{Args, NewImage, NewUser, RateLimitPolicy, UserFilter};

// ============================================================
// 搭台
// ============================================================

async fn sqlite_repo() -> (Repo, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("临时目录");
    let dsn = dir.path().join("pixora-details-b.db");
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

fn sample_admin(email: &str, uid: &str) -> NewUser {
    NewUser {
        role: "admin".to_string(),
        ..sample_user(email, uid)
    }
}

fn new_terms(version: &str) -> NewTermsVersion {
    NewTermsVersion {
        version: version.to_string(),
        title: "服务条款".to_string(),
        body: "一、服务说明\n二、账号与验证\n三、上传限制".to_string(),
        summary: Some("上传前请先阅读".to_string()),
        require_reconsent: true,
        effective_at: None,
        created_by: None,
    }
}

/// 直接数一张表的行数：好几处都在断言「UPSERT 没插出第二行」。
async fn row_count(repo: &Repo, table: &str) -> i64 {
    repo.db()
        .count(
            &format!("SELECT COUNT(*) AS total FROM {table}"),
            Args::new(),
        )
        .await
        .expect("计数")
}

/// 一份 blob + 一张图片引用，返回图片 id。统计用例要的是真实的跨表数据。
async fn seed_image(repo: &Repo, owner_id: i64, sha256: String, size: i64) -> i64 {
    let blob = repo
        .insert_blob_if_absent(&NewBlob {
            sha256,
            kind: "original".to_string(),
            size,
            mime_type: "image/png".to_string(),
            ext: "png".to_string(),
            width: Some(1200),
            height: Some(800),
            storage_driver: "local".to_string(),
            storage_key: format!("blobs/{size}.png"),
            backend_id: None,
        })
        .await
        .expect("登记 blob");

    repo.create_image_reference(
        &NewImage {
            owner_id,
            album_id: None,
            blob_id: blob.id,
            thumb_blob_id: None,
            original_name: format!("{}.png", &blob.sha256[..8]),
            title: None,
            visibility: "private".to_string(),
            upload_ip: None,
            upload_agent: None,
        },
        size,
        &[],
    )
    .await
    .expect("建图片引用")
    .id
}

// ============================================================
// 存储后端
// ============================================================

#[tokio::test]
async fn storage_backends_default_switch_and_crud() {
    let (repo, _dir) = sqlite_repo().await;

    let local = repo
        .create_backend(&NewBackend {
            driver: "local".to_string(),
            label: "本地磁盘".to_string(),
            config: r#"{"root":"/data/pixora"}"#.to_string(),
            is_default: true,
            enabled: true,
            capacity_mode: "auto".to_string(),
            capacity_total: None,
        })
        .await
        .expect("建本地后端");
    let s3 = repo
        .create_backend(&NewBackend {
            driver: "s3".to_string(),
            label: "对象存储".to_string(),
            config: r#"{"bucket":"pixora","secret":"enc:v1:xxxx"}"#.to_string(),
            is_default: false,
            enabled: true,
            capacity_mode: "fixed".to_string(),
            capacity_total: Some(1_073_741_824),
        })
        .await
        .expect("建对象存储后端");

    assert!(local.is_default);
    assert!(local.enabled);
    assert_eq!(
        local.config["root"].as_str(),
        Some("/data/pixora"),
        "config 以 JSON 文本往返"
    );
    assert_eq!(repo.count_backends().await.expect("计数"), 2);

    // 列表按 id 升序，含全部实例
    let all = repo.list_backends().await.expect("列表");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, local.id);
    assert_eq!(all[1].id, s3.id);
    assert_eq!(
        repo.default_backend().await.expect("默认后端").unwrap().id,
        local.id
    );
    assert_eq!(
        repo.find_backend_by_driver("s3")
            .await
            .expect("按驱动查")
            .unwrap()
            .id,
        s3.id
    );
    assert!(repo
        .find_backend_by_driver("webdav")
        .await
        .expect("按驱动查")
        .is_none());

    // 切默认：旧的必须自动置假，否则唯一索引会拦下这次更新
    repo.set_default_backend(s3.id).await.expect("切换默认");
    assert_eq!(
        repo.default_backend().await.expect("默认后端").unwrap().id,
        s3.id
    );
    let after = repo.list_backends().await.expect("列表");
    assert_eq!(
        after.iter().filter(|backend| backend.is_default).count(),
        1,
        "默认后端任何时刻只能有一个"
    );
    assert!(
        !after
            .iter()
            .find(|backend| backend.id == local.id)
            .expect("本地后端还在")
            .is_default
    );

    // 改配置：没给的字段保持原样
    repo.update_backend(
        s3.id,
        &BackendPatch {
            label: Some("主对象存储".to_string()),
            enabled: Some(false),
            capacity_total: Some(Some(2048)),
            ..Default::default()
        },
    )
    .await
    .expect("改配置");
    let patched = repo.find_backend(s3.id).await.expect("回读").unwrap();
    assert_eq!(patched.label, "主对象存储");
    assert!(!patched.enabled);
    assert_eq!(patched.capacity_total, Some(2048));
    assert_eq!(patched.driver, "s3", "driver 不在补丁里");
    assert_eq!(
        patched.config["bucket"].as_str(),
        Some("pixora"),
        "config 不在补丁里"
    );

    let enabled = repo.list_enabled_backends().await.expect("启用列表");
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, local.id);

    // 容量口径：Some(None) 表示把上限清空
    repo.set_capacity_mode(s3.id, "auto", None)
        .await
        .expect("改容量口径");
    let cleared = repo.find_backend(s3.id).await.expect("回读").unwrap();
    assert_eq!(cleared.capacity_mode, "auto");
    assert_eq!(cleared.capacity_total, None);

    repo.delete_backend(local.id).await.expect("删后端");
    assert_eq!(repo.count_backends().await.expect("计数"), 1);
    assert!(repo.find_backend(local.id).await.expect("回读").is_none());

    // 连切两次默认：中间态不会残留两个默认行
    repo.delete_backend(s3.id).await.expect("删后端");
    let only = repo
        .create_backend(&NewBackend {
            driver: "database".to_string(),
            label: "库内存储".to_string(),
            config: "{}".to_string(),
            is_default: false,
            enabled: true,
            capacity_mode: "auto".to_string(),
            capacity_total: None,
        })
        .await
        .expect("建后端");
    repo.set_default_backend(only.id).await.expect("第一次切换");
    repo.set_default_backend(only.id).await.expect("再切一次");
    assert_eq!(
        repo.list_backends()
            .await
            .expect("列表")
            .iter()
            .filter(|backend| backend.is_default)
            .count(),
        1
    );
}

// ============================================================
// IP 规则
// ============================================================

#[tokio::test]
async fn ip_rules_activity_hits_and_cleanup() {
    let (repo, _dir) = sqlite_repo().await;
    let admin = repo
        .create_user(&sample_admin("admin@example.com", "1000"))
        .await
        .expect("建管理员");
    let at = now();

    let allow = repo
        .create_ip_rule(&NewIpRule {
            cidr: "203.0.113.0/24".to_string(),
            kind: "allow".to_string(),
            scope: "all".to_string(),
            note: Some("办公网".to_string()),
            is_auto: false,
            expires_at: None,
            created_by: Some(admin.id),
        })
        .await
        .expect("建放行规则");
    assert!(allow.is_allow());
    assert!(!allow.is_expired(at), "没有失效时间的规则永不过期");
    assert_eq!(allow.hits, 0);

    let future = repo
        .create_ip_rule(&NewIpRule {
            cidr: "198.51.100.0/24".to_string(),
            kind: "deny".to_string(),
            scope: "login".to_string(),
            note: Some("临时封禁：撞库".to_string()),
            is_auto: true,
            expires_at: Some(at + Duration::hours(2)),
            created_by: None,
        })
        .await
        .expect("建两小时后失效的规则");

    let expired = repo
        .create_ip_rule(&NewIpRule {
            cidr: "192.0.2.0/24".to_string(),
            kind: "deny".to_string(),
            scope: "all".to_string(),
            note: Some("已经过期的自动封禁".to_string()),
            is_auto: true,
            expires_at: Some(at - Duration::hours(1)),
            created_by: None,
        })
        .await
        .expect("建已过期的规则");
    assert!(expired.is_expired(at));

    // 鉴权前加载的清单：过期项要滤掉，deny 与 allow 都要在
    let active = repo.active_rules(at).await.expect("生效规则");
    let active_ids: Vec<i64> = active.iter().map(|rule| rule.id).collect();
    assert_eq!(active.len(), 2, "{active_ids:?}");
    assert!(active_ids.contains(&allow.id));
    assert!(active_ids.contains(&future.id));
    assert!(!active_ids.contains(&expired.id));

    // 自动封禁去重：同网段同类型只认已经生效的那条
    let duplicate = repo
        .create_ip_rule(&NewIpRule {
            cidr: "198.51.100.0/24".to_string(),
            kind: "deny".to_string(),
            scope: "login".to_string(),
            note: None,
            is_auto: true,
            expires_at: Some(at + Duration::hours(2)),
            created_by: None,
        })
        .await
        .expect("再建一条同网段规则");
    let found = repo
        .find_active_rule("198.51.100.0/24", "deny", at)
        .await
        .expect("查生效规则")
        .expect("应该命中已有规则");
    assert_eq!(found.id, future.id, "去重要认最早的那条，不重复加");
    assert!(
        repo.find_active_rule("198.51.100.0/24", "allow", at)
            .await
            .expect("查生效规则")
            .is_none(),
        "kind 不同不算同一条规则"
    );
    assert!(
        repo.find_active_rule("192.0.2.0/24", "deny", at)
            .await
            .expect("查生效规则")
            .is_none(),
        "过期规则不算生效"
    );

    // 命中计数累加
    repo.bump_ip_rule_hits(allow.id).await.expect("记一次命中");
    repo.bump_ip_rule_hits(allow.id).await.expect("记一次命中");
    let bumped = repo
        .find_active_rule("203.0.113.0/24", "allow", at)
        .await
        .expect("查生效规则")
        .expect("规则还在");
    assert_eq!(bumped.hits, 2);

    // 过滤条件
    let deny = repo
        .list_ip_rules(
            &IpRuleFilter {
                kind: Some("deny".to_string()),
                ..Default::default()
            },
            50,
        )
        .await
        .expect("按类型列表");
    assert_eq!(deny.len(), 3, "两小时的 + 过期的 + 重复的");
    assert_eq!(deny[0].id, duplicate.id, "按创建时间倒序，最后建的排最前");

    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter {
            scope: Some("login".to_string()),
            ..Default::default()
        })
        .await
        .expect("按作用域计数"),
        2
    );
    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter {
            is_auto: Some(true),
            ..Default::default()
        })
        .await
        .expect("按来源计数"),
        3
    );
    let by_note = repo
        .list_ip_rules(
            &IpRuleFilter {
                query: Some("撞库".to_string()),
                ..Default::default()
            },
            50,
        )
        .await
        .expect("按关键词列表");
    assert_eq!(by_note.len(), 1);
    assert_eq!(by_note[0].cidr, "198.51.100.0/24");
    assert!(
        repo.list_ip_rules(
            &IpRuleFilter {
                query: Some("%".to_string()),
                ..Default::default()
            },
            50,
        )
        .await
        .expect("通配符不当通配符用")
        .is_empty(),
        "query 里的 % 要按字面量匹配"
    );

    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter {
            active_only: true,
            ..Default::default()
        })
        .await
        .expect("只数生效中的"),
        3,
        "放行的 + 两小时的 + 重复的"
    );
    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter::default())
            .await
            .expect("全部"),
        4
    );
    assert_eq!(
        repo.count_auto_rules(at).await.expect("自动规则数"),
        2,
        "过期的自动规则不算"
    );

    // 清理过期规则
    assert_eq!(repo.purge_expired_ip_rules(at).await.expect("清理过期"), 1);
    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter::default())
            .await
            .expect("全部"),
        3
    );

    repo.delete_ip_rule(duplicate.id).await.expect("删除规则");
    assert_eq!(
        repo.count_ip_rules(&IpRuleFilter::default())
            .await
            .expect("全部"),
        2
    );
}

// ============================================================
// 频率限制
// ============================================================

#[tokio::test]
async fn rate_limit_policy_fallback_upsert_and_effective() {
    let (repo, _dir) = sqlite_repo().await;
    let admin = repo
        .create_user(&sample_admin("admin@example.com", "1000"))
        .await
        .expect("建管理员");
    let user = repo
        .create_user(&sample_user("chenyu@example.com", "1001"))
        .await
        .expect("建用户");

    // 没配过：回内置默认值而不是报错
    let defaults = repo.global_policy().await.expect("全局默认");
    assert_eq!(defaults.id, 0, "内置默认不是库里的一行");
    assert_eq!(
        defaults.api_per_minute,
        RateLimitPolicy::defaults().api_per_minute
    );

    // 第一次写是插入，第二次写必须是更新同一行
    let mut global = RateLimitPolicy::defaults();
    global.api_per_minute = 500;
    global.note = Some("站内默认".to_string());
    repo.upsert_global_policy(&global)
        .await
        .expect("写全局默认");
    let stored = repo.global_policy().await.expect("全局默认");
    assert_eq!(stored.api_per_minute, 500);
    assert_eq!(stored.note.as_deref(), Some("站内默认"));
    assert_ne!(stored.id, 0);

    global.api_per_minute = 700;
    repo.upsert_global_policy(&global)
        .await
        .expect("再写全局默认");
    assert_eq!(
        repo.global_policy().await.expect("全局默认").api_per_minute,
        700
    );
    assert_eq!(
        row_count(&repo, "rate_limit_policies").await,
        1,
        "全局行只有一行"
    );

    // 没有覆盖时跟着全局走
    assert!(repo.user_policy(user.id).await.expect("用户策略").is_none());
    assert_eq!(
        repo.effective_policy(&user)
            .await
            .expect("生效策略")
            .api_per_minute,
        700
    );

    // 管理员豁免
    let admin_policy = repo.effective_policy(&admin).await.expect("生效策略");
    assert!(admin_policy.unlimited, "管理员走豁免");
    assert_eq!(admin_policy.api_per_minute, 700, "数值字段仍按回退链给出");

    // 用户覆盖：重复写不插第二行，且优先生效
    let mut custom = RateLimitPolicy::defaults();
    custom.api_per_minute = 30;
    custom.upload_per_day = 100;
    repo.upsert_user_policy(user.id, &custom)
        .await
        .expect("写用户覆盖");
    custom.api_per_minute = 45;
    repo.upsert_user_policy(user.id, &custom)
        .await
        .expect("再写用户覆盖");
    let stored_user = repo
        .user_policy(user.id)
        .await
        .expect("用户策略")
        .expect("有覆盖");
    assert_eq!(stored_user.api_per_minute, 45);
    assert_eq!(stored_user.user_id, Some(user.id));
    assert_eq!(
        row_count(&repo, "rate_limit_policies").await,
        2,
        "一个全局行 + 一个用户行"
    );
    assert_eq!(
        repo.effective_policy(&user)
            .await
            .expect("生效策略")
            .upload_per_day,
        100,
        "用户覆盖优先于全局默认"
    );
    assert_eq!(repo.count_custom_policies().await.expect("覆盖数"), 1);

    // 管理页列表：一行一个用户，带上有没有覆盖
    let page = repo
        .list_user_policies(
            &UserFilter::default(),
            PageRequest::new(None, None, 24, 100),
        )
        .await
        .expect("用户策略列表");
    assert_eq!(page.total, 2);
    let user_row = page
        .items
        .iter()
        .find(|row| row.user.id == user.id)
        .expect("用户行");
    assert!(user_row.policy.is_some());
    assert_eq!(user_row.policy.as_ref().unwrap().api_per_minute, 45);
    assert_eq!(user_row.hits_24h, 0);
    assert!(user_row.user.rate_custom);
    let admin_row = page
        .items
        .iter()
        .find(|row| row.user.id == admin.id)
        .expect("管理员行");
    assert!(admin_row.policy.is_none());
    assert!(!admin_row.user.rate_custom);

    // 近 24 小时的限流命中来自审计日志
    for _ in 0..3 {
        repo.insert_audit(&AuditEntry::new("rate_limited").actor(Some(user.id)))
            .await
            .expect("写审计");
    }
    assert_eq!(repo.count_rate_hits_24h(user.id).await.expect("命中数"), 3);

    let filtered = repo
        .list_user_policies(
            &UserFilter {
                query: Some(user.email.clone()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按邮箱过滤");
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.items[0].hits_24h, 3);

    // 恢复默认
    repo.delete_user_policy(user.id).await.expect("删覆盖");
    assert!(repo.user_policy(user.id).await.expect("用户策略").is_none());
    assert_eq!(
        repo.effective_policy(&user)
            .await
            .expect("生效策略")
            .api_per_minute,
        700
    );
}

#[tokio::test]
async fn rate_limit_counters_accumulate_in_window() {
    let (repo, _dir) = sqlite_repo().await;
    let user = repo
        .create_user(&sample_user("uploader@example.com", "2001"))
        .await
        .expect("建用户");
    let subject = user.id.to_string();
    let at = now();
    // 窗口起点由服务层对齐，这里照分钟与当天 0 点对齐
    let minute = at - Duration::seconds(at.timestamp() % 60);
    let day = at - Duration::seconds(at.timestamp() % 86_400);

    // 还没计数时读回 0，不是错误
    assert_eq!(
        repo.read_counter(scope::USER, &subject, kind::API_MINUTE, minute)
            .await
            .expect("读计数"),
        0
    );

    assert_eq!(
        repo.bump_counter(scope::USER, &subject, kind::API_MINUTE, minute, 1, 0)
            .await
            .expect("加一次"),
        1
    );
    assert_eq!(
        repo.bump_counter(scope::USER, &subject, kind::API_MINUTE, minute, 1, 0)
            .await
            .expect("再加一次"),
        2
    );
    assert_eq!(
        repo.bump_counter(scope::USER, &subject, kind::API_MINUTE, minute, 3, 0)
            .await
            .expect("加三次"),
        5,
        "返回的是累加后的值"
    );
    assert_eq!(
        repo.read_counter(scope::USER, &subject, kind::API_MINUTE, minute)
            .await
            .expect("读计数"),
        5
    );
    assert_eq!(
        row_count(&repo, "rate_counters").await,
        1,
        "同一个窗口只占一行"
    );

    // 字节维度单独累计
    for size in [1500i64, 2500, 1000] {
        repo.bump_counter(scope::USER, &subject, kind::UPLOAD_BYTES_DAY, day, 0, size)
            .await
            .expect("加字节");
    }
    assert_eq!(
        repo.read_counter_bytes(scope::USER, &subject, kind::UPLOAD_BYTES_DAY, day)
            .await
            .expect("读字节"),
        5000
    );
    assert_eq!(
        repo.sum_bytes_today(user.id, day).await.expect("今日字节"),
        5000
    );

    // 别的用户、别的窗口互不影响
    assert_eq!(
        repo.sum_bytes_today(user.id + 999, day)
            .await
            .expect("今日字节"),
        0
    );
    assert_eq!(
        repo.read_counter_bytes(
            scope::USER,
            &subject,
            kind::UPLOAD_BYTES_DAY,
            day - Duration::days(1)
        )
        .await
        .expect("读字节"),
        0
    );
    assert_eq!(
        repo.read_counter(scope::TOKEN, &subject, kind::API_MINUTE, minute)
            .await
            .expect("读计数"),
        0,
        "scope 不同就是另一个窗口"
    );
}

#[tokio::test]
async fn rate_limit_counters_purge_finished_windows() {
    let (repo, _dir) = sqlite_repo().await;
    let at = now();

    let finished = at - Duration::hours(3);
    let live = at - Duration::minutes(5);
    repo.bump_counter(scope::IP, "10.0.0.1", kind::API_HOUR, finished, 4, 0)
        .await
        .expect("写旧窗口");
    repo.bump_counter(scope::IP, "10.0.0.2", kind::API_HOUR, live, 1, 0)
        .await
        .expect("写当前窗口");

    assert_eq!(
        repo.purge_counters_before(at - Duration::hours(1))
            .await
            .expect("清理旧窗口"),
        1,
        "只清掉窗口已经结束的那一行"
    );
    assert_eq!(
        repo.read_counter(scope::IP, "10.0.0.1", kind::API_HOUR, finished)
            .await
            .expect("读计数"),
        0
    );
    assert_eq!(
        repo.read_counter(scope::IP, "10.0.0.2", kind::API_HOUR, live)
            .await
            .expect("读计数"),
        1
    );
}

// ============================================================
// 统计
// ============================================================

#[tokio::test]
async fn storage_stats_dedup_and_admin_summary() {
    let (repo, _dir) = sqlite_repo().await;
    let chen = repo
        .create_user(&sample_user("chenyu@example.com", "3001"))
        .await
        .expect("建用户");
    let li = repo
        .create_user(&sample_user("li@example.com", "3002"))
        .await
        .expect("建用户");

    // 两张不同内容，第三人上传其中一份（去重命中同一个 blob）
    seed_image(&repo, chen.id, "a".repeat(64), 1000).await;
    let second = seed_image(&repo, chen.id, "b".repeat(64), 2000).await;
    seed_image(&repo, li.id, "a".repeat(64), 1000).await;

    let stats = repo.storage_stats(5).await.expect("存储面板");
    assert_eq!(stats.image_count, 3);
    assert_eq!(stats.total_bytes, 3000, "占用：两份 blob（去重后只有两份）");
    assert_eq!(
        stats.logical_bytes, 4000,
        "逻辑占用：三张图各自引用的大小之和"
    );
    assert_eq!(
        stats.dedup_saved_bytes, 1000,
        "被多引用的那一份就是去重省下的"
    );
    assert_eq!(stats.average_size, 1500, "(1000 + 2000) / 2");
    assert_eq!(stats.binned_count, 0, "没有进回收站的图");
    assert_eq!(stats.pending_count, 0, "实体都还被引用着");
    assert!(
        stats
            .drivers
            .iter()
            .any(|item| item.driver == "local" && item.count == 3 && item.bytes == 3000),
        "张数按图片数算（3 张），占用按实体算（两份 blob 3000 字节）：{:?}",
        stats.drivers
    );
    assert!(
        stats
            .mimes
            .iter()
            .any(|item| item.mime == "image/png" && item.count == 2),
        "{:?}",
        stats.mimes
    );
    assert_eq!(stats.recent.len(), 3);
    assert_eq!(stats.recent[0].sha256.len(), 64);

    // 趋势与分时都要有今天的桶
    let today = now_ts()[..10].to_string();
    let trend = repo.upload_trend(30).await.expect("趋势");
    assert!(
        trend
            .iter()
            .any(|point| point.bucket == today && point.count == 3),
        "{trend:?}"
    );
    assert!(trend.iter().all(|point| point.bucket.len() == 10));
    let hourly = repo.upload_hourly_today().await.expect("分时");
    assert!(
        hourly.iter().any(|point| point.bucket.starts_with(&today)),
        "{hourly:?}"
    );
    assert!(hourly.iter().all(|point| point.bucket.len() == 13));

    let admin = repo.admin_stats().await.expect("管理端汇总");
    assert_eq!(admin.user_total, 2);
    assert_eq!(admin.user_new_this_month, 2);
    assert_eq!(admin.allocated_quota, 2 * 10 * 1024 * 1024 * 1024);
    assert_eq!(admin.used_bytes, 4000, "每个用户按自己引用的图片大小记用量");
    assert_eq!(admin.image_total, 3);
    assert_eq!(admin.storage_bytes, 3000);
    assert!(admin.register_open, "开关由服务层覆盖，仓储先给 true");

    // 软删一张：逻辑占用降下去，物理占用要等 GC
    let image = repo.find_image(second).await.expect("查图").expect("图在");
    repo.delete_image_reference(
        &ImageDeletion {
            image_id: image.id,
            owner_id: image.owner_id,
            album_id: image.album_id,
            blob_id: image.blob_id,
            thumb_blob_id: image.thumb_blob_id,
            size: 2000,
                    by_album: false,
        },
        now(),
    )
    .await
    .expect("软删");

    let after = repo.storage_stats(10).await.expect("存储面板");
    assert_eq!(after.image_count, 2, "活着的图少了一张");
    assert_eq!(after.binned_count, 1, "那张进了回收站");
    assert_eq!(
        after.logical_bytes, 4000,
        "回收站里的图仍然算占用：它占着磁盘，也占着额度"
    );
    assert_eq!(after.total_bytes, 3000, "软删不动物理文件");
    assert_eq!(after.dedup_saved_bytes, 1000, "被多引用的那份仍然是省下的");
    let trend_after = repo.upload_trend(30).await.expect("趋势");
    assert!(
        trend_after
            .iter()
            .any(|point| point.bucket == today && point.count == 2),
        "{trend_after:?}"
    );
}

// ============================================================
// 服务条款
// ============================================================

#[tokio::test]
async fn terms_publish_archive_consent_and_restrict() {
    let (repo, _dir) = sqlite_repo().await;
    let chen = repo
        .create_user(&sample_user("chenyu@example.com", "4001"))
        .await
        .expect("建用户");
    let li = repo
        .create_user(&sample_user("li@example.com", "4002"))
        .await
        .expect("建用户");

    // 还没发布过任何版本：视为无需同意
    assert!(repo.active_terms().await.expect("生效版本").is_none());
    assert!(repo.has_consented_active(chen.id).await.expect("同意状态"));

    let v1 = repo.create_terms(&new_terms("v1.0")).await.expect("建草稿");
    assert_eq!(v1.status, "draft");
    assert!(v1.published_at.is_none());
    assert!(v1.require_reconsent);
    assert_eq!(
        repo.find_terms(v1.id).await.expect("查条款").unwrap().body,
        v1.body
    );
    assert_eq!(
        repo.find_terms_by_version("v1.0")
            .await
            .expect("按版本号查")
            .unwrap()
            .id,
        v1.id
    );

    // 发布：没指定生效时间就按发布时刻生效
    let published_at = now();
    repo.publish_terms(v1.id, published_at).await.expect("发布");
    let active = repo
        .active_terms()
        .await
        .expect("生效版本")
        .expect("有生效版本");
    assert_eq!(active.id, v1.id);
    assert_eq!(active.status, "active");
    assert!(active.published_at.is_some());
    let effective = active.effective_at.expect("生效时间");
    assert_eq!(effective.timestamp(), published_at.timestamp());
    assert!(!repo.has_consented_active(chen.id).await.expect("同意状态"));

    // 记录同意：重复提交不报错，也只留一条
    repo.record_consent(
        chen.id,
        &active,
        Some("203.0.113.9"),
        Some("Mozilla/5.0"),
        now(),
    )
    .await
    .expect("记同意");
    assert!(repo.has_consented_active(chen.id).await.expect("同意状态"));
    assert!(!repo.has_consented_active(li.id).await.expect("同意状态"));
    assert_eq!(repo.count_consents(v1.id).await.expect("同意数"), 1);

    let consent = repo
        .find_consent(chen.id, v1.id)
        .await
        .expect("查同意")
        .expect("有同意记录");
    assert_eq!(consent.version, "v1.0", "版本号冗余存一份");
    assert_eq!(consent.ip.as_deref(), Some("203.0.113.9"));
    assert_eq!(consent.user_agent.as_deref(), Some("Mozilla/5.0"));

    repo.record_consent(chen.id, &active, Some("203.0.113.9"), None, now())
        .await
        .expect("重复同意不该报错");
    assert_eq!(repo.count_consents(v1.id).await.expect("同意数"), 1);
    assert_eq!(
        repo.list_consents(chen.id).await.expect("同意列表").len(),
        1
    );

    // 发布第二版：旧的自动归档，active 始终只有一行
    let v2 = repo.create_terms(&new_terms("v2.0")).await.expect("建草稿");
    repo.publish_terms(v2.id, now()).await.expect("发布");
    assert_eq!(
        repo.active_terms().await.expect("生效版本").unwrap().id,
        v2.id
    );
    assert_eq!(
        repo.find_terms(v1.id)
            .await
            .expect("查旧版")
            .unwrap()
            .status,
        "archived"
    );
    assert!(
        !repo.has_consented_active(chen.id).await.expect("同意状态"),
        "同意过旧版不等于同意新版"
    );

    let rows = repo.list_terms().await.expect("条款列表");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].version.id, v2.id, "按创建时间倒序");
    assert_eq!(rows[0].consent_count, 0);
    assert_eq!(rows[0].user_total, 2);
    assert_eq!(rows[1].version.id, v1.id);
    assert_eq!(rows[1].consent_count, 1);

    // 同意新版后再看列表
    repo.record_consent(chen.id, &v2, None, None, now())
        .await
        .expect("同意新版");
    let rows = repo.list_terms().await.expect("条款列表");
    assert_eq!(rows[0].consent_count, 1);
    assert!(repo.has_consented_active(chen.id).await.expect("同意状态"));

    // 草稿可以改、可以删
    let v3 = repo.create_terms(&new_terms("v3.0")).await.expect("建草稿");
    let mut revised = new_terms("v3.0");
    revised.title = "服务条款（修订）".to_string();
    revised.body = "一、服务说明（修订）".to_string();
    revised.require_reconsent = false;
    repo.update_terms(v3.id, &revised).await.expect("改草稿");
    let updated = repo.find_terms(v3.id).await.expect("查条款").unwrap();
    assert_eq!(updated.title, "服务条款（修订）");
    assert!(!updated.require_reconsent);
    assert_eq!(updated.status, "draft", "改内容不动状态");

    // 开关单独也能改
    repo.set_require_reconsent(v3.id, true)
        .await
        .expect("改开关");
    assert!(
        repo.find_terms(v3.id)
            .await
            .expect("查条款")
            .unwrap()
            .require_reconsent
    );

    repo.delete_terms(v3.id).await.expect("删草稿");
    assert!(repo.find_terms(v3.id).await.expect("查条款").is_none());
    assert_eq!(repo.list_terms().await.expect("条款列表").len(), 2);

    // 归档接口：下架后 active 为空
    repo.archive_terms(v2.id).await.expect("归档");
    assert!(repo.active_terms().await.expect("生效版本").is_none());
    assert!(repo.has_consented_active(chen.id).await.expect("同意状态"));

    // 发布不存在的版本要报错，且不能把原来的 active 弄丢
    repo.publish_terms(v2.id, now()).await.expect("重新发布");
    assert!(repo.publish_terms(9999, now()).await.is_err());
    assert_eq!(
        repo.active_terms().await.expect("生效版本").unwrap().id,
        v2.id,
        "发布失败不该回滚掉原本的生效版本"
    );

    // 删除是真删：版本行与它的同意记录在同一个事务里一起消失。
    // 同意记录挂在版本行上——版本没了，留下的记录也只是孤儿，
    // 所以两者一起删，删掉多少条由返回值给出来写审计。
    let removed = repo.delete_terms(v1.id).await.expect("删归档版本");
    assert_eq!(removed, 1, "连带删掉的那条同意记录");
    assert!(repo.find_terms(v1.id).await.expect("查条款").is_none());
    assert_eq!(repo.count_consents(v1.id).await.expect("剩余同意记录"), 0);

    let removed = repo.delete_terms(v2.id).await.expect("删生效版本");
    assert_eq!(removed, 1, "新版那条同意记录也一起删");
    assert!(repo.find_terms(v2.id).await.expect("查条款").is_none());
    assert_eq!(repo.list_terms().await.expect("条款列表").len(), 0);
}

// ============================================================
// 审计
// ============================================================

#[tokio::test]
async fn audit_insert_filter_count_and_purge() {
    let (repo, _dir) = sqlite_repo().await;
    let chen = repo
        .create_user(&sample_user("chenyu@example.com", "5001"))
        .await
        .expect("建用户");
    let li = repo
        .create_user(&sample_user("li@example.com", "5002"))
        .await
        .expect("建用户");

    let first = repo
        .insert_audit(
            &AuditEntry::new("login_fail")
                .actor(Some(chen.id))
                .ip(Some("203.0.113.7".to_string()))
                .detail(serde_json::json!({"reason": "wrong_password"})),
        )
        .await
        .expect("写审计");
    assert!(first > 0);
    repo.insert_audit(&AuditEntry::new("login_ok").actor(Some(chen.id)))
        .await
        .expect("写审计");
    repo.insert_audit(
        &AuditEntry::new("admin_view_user")
            .actor(Some(li.id))
            .target("1001"),
    )
    .await
    .expect("写审计");
    for _ in 0..3 {
        repo.insert_audit(&AuditEntry::new("rate_limited").actor(Some(chen.id)))
            .await
            .expect("写审计");
    }

    // 一条 30 小时前的流水，用来验证时间窗口过滤
    let long_ago = now() - Duration::hours(30);
    repo.db()
        .execute(
            "INSERT INTO audit_logs (actor_id, action, target, detail, ip, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            Args::new()
                .int(chen.id)
                .text("rate_limited")
                .opt_text(None)
                .text("{}")
                .text("10.0.0.9")
                .text(ts(long_ago)),
        )
        .await
        .expect("插旧流水");

    let all = repo
        .list_audit(
            &AuditFilter::default(),
            PageRequest::new(None, None, 50, 100),
        )
        .await
        .expect("全部流水");
    assert_eq!(all.total, 7, "六条刚写的 + 一条 30 小时前的");
    assert_eq!(all.items.len(), 7);
    assert_eq!(all.items[0].action, "rate_limited", "最近的排最前");

    let by_actor = repo
        .list_audit(
            &AuditFilter {
                actor_id: Some(li.id),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按操作人过滤");
    assert_eq!(by_actor.total, 1);
    assert_eq!(by_actor.items[0].target.as_deref(), Some("1001"));

    let by_action = repo
        .list_audit(
            &AuditFilter {
                action: Some("rate_limited".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按动作过滤");
    assert_eq!(by_action.total, 4);

    let by_prefix = repo
        .list_audit(
            &AuditFilter {
                action_prefix: Some("login".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按前缀过滤");
    assert_eq!(by_prefix.total, 2);

    let recent = repo
        .list_audit(
            &AuditFilter {
                since: Some(now() - Duration::hours(1)),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按时间过滤");
    assert_eq!(recent.total, 6, "30 小时前那条不在窗口里");
    assert!(
        recent
            .items
            .iter()
            .all(|item| item.action != "login_ok" || item.ip.is_none()),
        "没有 IP 的行也要能解出来"
    );

    // JSON 详情要能往返
    let detail = repo
        .list_audit(
            &AuditFilter {
                action: Some("login_fail".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .expect("按动作过滤");
    assert_eq!(
        detail.items[0].detail["reason"].as_str(),
        Some("wrong_password")
    );
    assert_eq!(detail.items[0].ip.as_deref(), Some("203.0.113.7"));

    // 计数
    assert_eq!(
        repo.count_audit_action_since(Some(chen.id), "rate_limited", now() - Duration::hours(1))
            .await
            .expect("按人计数"),
        3
    );
    assert_eq!(
        repo.count_audit_action_since(None, "rate_limited", now() - Duration::hours(48))
            .await
            .expect("不限定人"),
        4
    );
    assert_eq!(
        repo.count_rate_limited_since(now() - Duration::hours(1))
            .await
            .expect("全站限流次数"),
        3
    );
    assert_eq!(
        repo.count_action_for_ips(
            "login_fail",
            &["203.0.113.7".to_string()],
            now() - Duration::hours(1)
        )
        .await
        .expect("按 IP 计数"),
        1
    );
    assert_eq!(
        repo.count_action_for_ips(
            "login_fail",
            &["10.0.0.9".to_string()],
            now() - Duration::hours(1)
        )
        .await
        .expect("按 IP 计数"),
        0
    );
    assert_eq!(
        repo.count_action_for_ips("login_fail", &[], now() - Duration::hours(1))
            .await
            .expect("空 IP 列表"),
        0
    );

    let summary = repo
        .count_audit_by_action(now() - Duration::hours(1))
        .await
        .expect("按动作汇总");
    assert_eq!(summary.len(), 4);
    assert_eq!(summary[0].0, "rate_limited", "次数多的排前面");
    assert_eq!(summary[0].1, 3);
    let counts: std::collections::HashMap<String, i64> = summary.into_iter().collect();
    assert_eq!(counts["login_fail"], 1);
    assert_eq!(counts["login_ok"], 1);
    assert_eq!(counts["admin_view_user"], 1);

    // 清理旧流水
    assert_eq!(
        repo.purge_audit_before(now() - Duration::hours(1))
            .await
            .expect("清理"),
        1
    );
    assert_eq!(
        repo.list_audit(&AuditFilter::default(), PageRequest::default())
            .await
            .expect("全部流水")
            .total,
        6
    );
    assert!(
        repo.count_audit_by_action(now() - Duration::hours(48))
            .await
            .expect("按动作汇总")
            .iter()
            .all(|(action, count)| action != "rate_limited" || *count == 3),
        "旧流水被清掉后计数一起变"
    );
}

// ============================================================
// 删除账号
// ============================================================

/// 删账号要把从属数据一起带走，并且只还自己那一份引用计数。
///
/// 这是删账号最容易漏的两处：外键级联删行不会动 `blobs.ref_count`（计数停在
/// 原地，GC 就永远收不走文件），而共享实体上挂着别人的引用，删多了会连带
/// 把别人还在用的文件回收掉。
#[tokio::test]
async fn deleting_a_user_takes_its_rows_and_only_its_own_refs() {
    let (repo, _dir) = sqlite_repo().await;
    let chen = repo
        .create_user(&sample_user("chenyu@example.com", "4001"))
        .await
        .expect("建用户");
    let li = repo
        .create_user(&sample_user("li@example.com", "4002"))
        .await
        .expect("建用户");

    // 同一份文件被两个人引用（内容寻址下很常见），另有一份只属于 chen
    seed_image(&repo, chen.id, "c".repeat(64), 500).await;
    seed_image(&repo, li.id, "c".repeat(64), 500).await;
    let private = seed_image(&repo, chen.id, "d".repeat(64), 700).await;
    let shared = repo
        .find_blob_by_sha(&"c".repeat(64), "original")
        .await
        .expect("查共享实体")
        .expect("共享实体在");

    // 顺手给 chen 建一个相册与一条令牌，级联是否成立看它们
    let album = repo
        .insert_default_album(chen.id, "默认相册")
        .await
        .expect("相册");
    repo.create_api_token(&pixora_repo::repo::tokens::NewApiToken {
        user_id: chen.id,
        name: "冒烟令牌".to_string(),
        token_hash: "f".repeat(64),
        display: "px_live_abcdef12".to_string(),
        scopes: vec!["upload".to_string()],
        expires_at: None,
    })
    .await
    .expect("令牌");

    let returned = repo.delete_user(chen.id).await.expect("删账号");
    assert_eq!(returned.len(), 2, "返回这个账号用过的两个实体");

    // 只属于 chen 的实体：计数归零，等回收
    assert_eq!(
        repo.find_blob_by_id(private).await.unwrap().unwrap().ref_count,
        0,
        "自己的引用还回去"
    );
    // 共享实体：还剩 li 那一份引用，文件不能被收走
    assert_eq!(
        repo.find_blob_by_id(shared.id).await.unwrap().unwrap().ref_count,
        1,
        "别人还在用的实体，计数只减自己那一份"
    );

    // 从属数据随外键级联消失
    assert!(repo.find_user_by_id(chen.id).await.unwrap().is_none());
    assert!(
        repo.find_album_by_id(album.id).await.unwrap().is_none(),
        "相册随账号一起删"
    );
    assert!(
        repo.list_api_tokens(chen.id).await.unwrap().is_empty(),
        "令牌随账号一起删"
    );
    assert_eq!(
        repo.count_images(&ImageFilter {
            owner_id: Some(chen.id),
            ..Default::default()
        })
        .await
        .unwrap(),
        0,
        "图片引用随账号一起删"
    );
    // 另一个人的东西一件都不能少
    assert_eq!(
        repo.count_images(&ImageFilter {
            owner_id: Some(li.id),
            ..Default::default()
        })
        .await
        .unwrap(),
        1,
        "另一个人的图还在"
    );
    assert!(repo.find_user_by_id(li.id).await.unwrap().is_some());
}

/// 删相册时，里面的图片改挂到默认相册——图片永远该有个归属。
///
/// 早先靠外键 `ON DELETE SET NULL`，结果是图片散在「未分类」里；那个状态
/// 已经不对外了，所以删之前先把它们收进默认相册。
#[tokio::test]
async fn deleting_an_album_reattaches_its_images_to_the_default_one() {
    let (repo, _dir) = sqlite_repo().await;
    let chen = repo
        .create_user(&sample_user("chenyu@example.com", "4101"))
        .await
        .expect("建用户");
    let fallback = repo
        .insert_default_album(chen.id, "默认相册")
        .await
        .expect("默认相册");
    let other = repo
        .create_album(&pixora_repo::repo::albums::NewAlbum {
            owner_id: chen.id,
            name: "自建相册".to_string(),
            slug: "mine".to_string(),
            description: None,
            visibility: "private".to_string(),
            sort_order: 1,
        })
        .await
        .expect("自建相册");

    let image_id = seed_image(&repo, chen.id, "e".repeat(64), 300).await;
    // 先把它挪进自建相册
    repo.move_images(&[image_id], chen.id, Some(other.id))
        .await
        .expect("移进自建相册");
    assert_eq!(
        repo.find_image(image_id).await.unwrap().unwrap().album_id,
        Some(other.id)
    );

    repo.delete_album(other.id).await.expect("删相册");
    assert_eq!(
        repo.find_image(image_id).await.unwrap().unwrap().album_id,
        Some(fallback.id),
        "删相册后图片该回到默认相册，而不是没有归属"
    );
    // 默认相册的计数跟着涨：它不是靠外键改的，得自己维护
    assert_eq!(
        repo.find_album_by_id(fallback.id).await.unwrap().unwrap().image_count,
        1,
        "默认相册的计数要与事实对上"
    );
}
