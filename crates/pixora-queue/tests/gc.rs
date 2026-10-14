//! Blob 回收的行为验证。
//!
//! `gc::collect_blobs` 是全库唯一会删物理文件的地方，而「引用计数没配对就删文件」
//! 是这套设计里最不能犯的错——同一份文件可能被多个用户引用，删错一次就是别人的图
//! 消失。所以这个文件专门验证三条边界：
//!
//!   1. 引用归零 + 过了宽限期 → 文件与数据库行都被清掉；
//!   2. 还有引用（ref_count > 0）→ 无论多老都不许动；
//!   3. 刚登记的实体（宽限期内）→ 不动，避开「先写实体、再建引用」的时间窗。

use std::sync::Arc;

use chrono::{Duration, Utc};
use pixora_core::config::KeySource;
use pixora_core::{AppState, Config};
use pixora_queue::gc;
use pixora_repo::model::{now_ts, ts};
use pixora_repo::repo::uploads::NewBlob;
use pixora_repo::{Args, DbDriver};

/// 装配一个只跑 SQLite + 本地目录的实例。
async fn test_state(dir: &tempfile::TempDir) -> Arc<AppState> {
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_driver: DbDriver::Sqlite,
        db_dsn: dir.path().join("gc.db").to_string_lossy().into_owned(),
        db_max_connections: 2,
        data_dir: data_dir.clone(),
        master_key: Some(pixora_crypto::MasterKey::generate().unwrap()),
        key_source: KeySource::Env,
        workers: 1,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        trust_proxy: 0,
        web_dir: None,
        accel_prefix: None,
        secure_cookies: false,
        log_level: "warn".to_string(),
    };
    AppState::bootstrap(config).await.expect("装配测试实例")
}

/// 登记一个实体，把文件真的写到磁盘上，并返回 (blob_id, 文件路径)。
async fn seed_blob(state: &Arc<AppState>, marker: char) -> (i64, std::path::PathBuf) {
    let sha = marker.to_string().repeat(64);
    let key = format!("blobs/{}/{}.png", &sha[..2], sha);
    let path = state.config.data_dir.join(&key);
    tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
    tokio::fs::write(&path, b"fake-png-bytes-for-gc").await.unwrap();

    let blob = state
        .repo
        .insert_blob_if_absent(&NewBlob {
            sha256: sha,
            kind: "original".to_string(),
            size: 21,
            mime_type: "image/png".to_string(),
            ext: "png".to_string(),
            width: None,
            height: None,
            storage_driver: "local".to_string(),
            storage_key: key,
            backend_id: None,
        })
        .await
        .unwrap();
    (blob.id, path)
}

/// 把实体的登记时间往前推，模拟「早就过期了」。
async fn age_blob(state: &Arc<AppState>, blob_id: i64, hours: i64) {
    let stamp = ts(Utc::now() - Duration::hours(hours));
    state
        .repo
        .db()
        .execute(
            "UPDATE blobs SET created_at = ? WHERE id = ?",
            Args::new().text(stamp).int(blob_id),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn unreferenced_blob_past_grace_period_is_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    let (blob_id, path) = seed_blob(&state, 'a').await;
    age_blob(&state, blob_id, 3).await;
    assert!(path.exists(), "准备阶段文件应当存在");

    let removed = gc::collect_blobs(&state, 60, 10).await.expect("回收");
    assert_eq!(removed, 1, "无引用且过期的实体应当被回收");
    assert!(!path.exists(), "物理文件必须被删掉");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_none(),
        "数据库行也要一起删掉，否则会留下指向不存在文件的记录"
    );
}

#[tokio::test]
async fn referenced_blob_is_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    let (blob_id, path) = seed_blob(&state, 'b').await;
    age_blob(&state, blob_id, 12).await;
    // 加一个引用：这张图还被某个人用着
    state.repo.inc_blob_ref(blob_id).await.unwrap();

    let removed = gc::collect_blobs(&state, 60, 10).await.expect("回收");
    assert_eq!(removed, 0, "还有引用的实体绝不能被回收");
    assert!(path.exists(), "别人的图不能消失");
    assert_eq!(
        state
            .repo
            .find_blob_by_id(blob_id)
            .await
            .unwrap()
            .unwrap()
            .ref_count,
        1
    );
}

#[tokio::test]
async fn fresh_blob_within_grace_period_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    // 刚登记、引用还没建起来：这正是宽限期要保护的时间窗
    let (blob_id, path) = seed_blob(&state, 'c').await;

    let removed = gc::collect_blobs(&state, 60, 10).await.expect("回收");
    assert_eq!(removed, 0, "宽限期内的实体不动");
    assert!(path.exists());
    assert!(state.repo.find_blob_by_id(blob_id).await.unwrap().is_some());
}

#[tokio::test]
async fn batch_limit_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    for marker in ['d', 'e', 'f'] {
        let (blob_id, _) = seed_blob(&state, marker).await;
        age_blob(&state, blob_id, 5).await;
    }

    // 单轮只处理 2 个：剩下的留给下一轮，避免一次 GC 把 IO 打满
    let first = gc::collect_blobs(&state, 60, 2).await.expect("回收");
    assert_eq!(first, 2, "单轮上限要生效");
    let second = gc::collect_blobs(&state, 60, 2).await.expect("回收");
    assert_eq!(second, 1, "下一轮把剩下的收掉");
    assert_eq!(state.repo.count_blobs().await.unwrap(), 0);
}

#[tokio::test]
async fn reconcile_fixes_drifted_counters() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    // 造一个用户和一张图，然后把用量故意改错，模拟「某次异常退出留下的漂移」
    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "1001".to_string(),
            email: "reconcile@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let (blob_id, _) = seed_blob(&state, '9').await;
    state
        .repo
        .create_image_reference(
            &pixora_repo::repo::images::NewImage {
                owner_id: user.id,
                album_id: None,
                blob_id,
                thumb_blob_id: None,
                original_name: "a.png".to_string(),
                title: None,
                visibility: "private".to_string(),
            },
            21,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        state.repo.find_user_by_id(user.id).await.unwrap().unwrap().used_bytes,
        21
    );

    // 制造漂移
    state.repo.add_used_bytes(user.id, 9999).await.unwrap();
    let corrected = gc::reconcile(&state).await.expect("对账");
    assert!(corrected >= 1, "对账应当修正至少一行");
    assert_eq!(
        state.repo.find_user_by_id(user.id).await.unwrap().unwrap().used_bytes,
        21,
        "用量要按已存文件重算回来"
    );
}

#[test]
fn timestamps_use_the_same_rfc3339_shape_as_the_rest_of_the_system() {
    // 这个测试文件里的时间比较依赖文本字典序，锁定一下形态
    let stamp = now_ts();
    assert!(stamp.ends_with('Z'), "{stamp}");
    assert_eq!(stamp.len(), 27, "{stamp}");
}
