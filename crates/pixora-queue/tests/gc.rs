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
        install_token: "test-install-token".into(),
        install_token_source: "test".into(),
        workers: 1,
        base_url: Some("http://127.0.0.1:8080".to_string()),
        trust_proxy: 0,
        web_dir: None,
        accel_prefix: None,
        secure_cookies: false,
        samesite: pixora_core::config::SameSite::Lax,
        dev_return_codes: false,
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

    let removed = gc::collect_blobs(&state, 60, 10, 0).await.expect("回收").removed;
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

    let removed = gc::collect_blobs(&state, 60, 10, 0).await.expect("回收").removed;
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

    let removed = gc::collect_blobs(&state, 60, 10, 0).await.expect("回收").removed;
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
    let first = gc::collect_blobs(&state, 60, 2, 0).await.expect("回收").removed;
    assert_eq!(first, 2, "单轮上限要生效");
    let second = gc::collect_blobs(&state, 60, 2, 0).await.expect("回收").removed;
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

                upload_ip: None,

                upload_agent: None,
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

#[tokio::test]
async fn deleting_a_user_does_not_strand_physical_files() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    // 一个用户、一张图：引用计数为 1
    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "2002".to_string(),
            email: "cascade@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let (blob_id, path) = seed_blob(&state, 'e').await;
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

                upload_ip: None,

                upload_agent: None,
            },
            21,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        1
    );

    // 删除账号：images 随外键级联消失，引用计数也得跟着还回去。
    // 漏了这一步计数就停在 1，GC 永远等不到 ref_count = 0，
    // 文件谁也收不走——删账号本来就是为了把盘腾出来
    let blobs = state.repo.delete_user(user.id).await.unwrap();
    assert_eq!(blobs.len(), 1, "要返回这个账号用过的实体，调用方据此回收文件");
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        0,
        "引用计数随账号一起还回去"
    );
    assert_eq!(
        gc::reconcile(&state).await.unwrap(),
        0,
        "删账号这条路径不该留下需要对账的偏差"
    );

    // 归零 + 过期 → 回收，物理文件不再滞留
    age_blob(&state, blob_id, 3).await;
    let removed = gc::collect_blobs(&state, 60, 10, 0).await.unwrap().removed;
    assert_eq!(removed, 1);
    assert!(!path.exists(), "删账号不该把物理文件永久留在盘上");
}

/// 建一个用户 + 一个真实文件 + 相册里的图片引用，并把镜像链接建好。
/// 返回 (用户, 相册, blob_id, 物理文件, 镜像链接路径)。
async fn seed_album_image(
    state: &Arc<AppState>,
    marker: char,
    uid: &str,
) -> (
    pixora_repo::model::User,
    pixora_repo::model::Album,
    i64,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: uid.to_string(),
            email: format!("{uid}@example.com"),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let album = state
        .repo
        .create_album(&pixora_repo::repo::albums::NewAlbum {
            owner_id: user.id,
            name: "旅行".to_string(),
            slug: "旅行".to_string(),
            description: None,
            visibility: "private".to_string(),
            sort_order: 0,
        })
        .await
        .unwrap();

    let (blob_id, path) = seed_blob(state, marker).await;
    let blob = state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap();
    state
        .repo
        .create_image_reference(
            &pixora_repo::repo::images::NewImage {
                owner_id: user.id,
                album_id: Some(album.id),
                blob_id,
                thumb_blob_id: None,
                original_name: "in-album.png".to_string(),
                title: None,
                visibility: "private".to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            21,
            &[],
        )
        .await
        .unwrap();

    // 镜像树：与上传路径一致，给本地驱动的实体建硬链接
    state
        .link_view_for(
            &blob.storage_driver,
            blob.backend_id,
            &blob.storage_key,
            &user.uid,
            &album.slug,
            &blob.sha256,
            &blob.ext,
        )
        .await;
    let view = state
        .config
        .data_dir
        .join("users")
        .join(&user.uid)
        .join(&album.slug)
        .join(format!("{}.{}", blob.sha256, blob.ext));
    (user, album, blob_id, path, view)
}

#[tokio::test]
async fn deleting_an_album_clears_its_mirror_directory() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let (user, album, blob_id, path, view) = seed_album_image(&state, 'm', "3003").await;
    assert!(view.exists(), "上传进相册后镜像树里应当有链接");
    let album_dir = view.parent().unwrap().to_path_buf();

    // 删相册 = 相册与其中的图片一起进回收站：镜像目录与链接要收掉，
    // 但物理文件留着（保留期内要能整份恢复）
    let moved = state
        .delete_album(&pixora_core::images::Viewer::User(user.clone()), album.id)
        .await
        .unwrap();
    assert_eq!(moved, 1, "应当带着 1 张图片一起进回收站");
    assert!(!view.exists(), "进回收站后镜像链接不该还在");
    assert!(
        !album_dir.exists(),
        "空了的相册目录也该收掉，别在磁盘上留空壳"
    );
    assert!(path.exists(), "物理文件要留着——回收站里能恢复");
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        0,
        "进回收站的图片引用计数归零（不占配额）"
    );
    assert!(
        state.repo.find_album_by_id(album.id).await.unwrap().is_none(),
        "回收站里的相册不出现在正常查询里"
    );
}

#[tokio::test]
async fn default_album_cannot_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "3004".to_string(),
            email: "keeper@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let album = state
        .repo
        .insert_default_album(user.id, "默认相册")
        .await
        .unwrap();

    let refused = state
        .delete_album(&pixora_core::images::Viewer::User(user), album.id)
        .await;
    assert!(refused.is_err(), "默认相册是上传的落脚点，不该允许删除");
    assert!(
        state.repo.find_album_by_id(album.id).await.unwrap().is_some(),
        "拒绝之后相册要还在"
    );
}

#[tokio::test]
async fn legacy_nested_view_tree_is_cleaned_by_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    // 旧版 bug 的产物：users/users/{uid}/{相册}/…（硬链接到 CAS 正本）
    let legacy = state.config.data_dir.join("users").join("users").join("1000").join("旅行");
    tokio::fs::create_dir_all(&legacy).await.unwrap();
    let link = legacy.join("deadbeef.png");
    tokio::fs::write(&link, b"legacy-link").await.unwrap();

    gc::reconcile(&state).await.unwrap();
    assert!(
        !state.config.data_dir.join("users").join("users").exists(),
        "对账应当把旧版多套一层的镜像树清掉"
    );

    // 正常路径（users/{uid}/…）不受影响，别顺手把好的也删了
    let good = state.config.data_dir.join("users").join("1000").join("旅行");
    tokio::fs::create_dir_all(&good).await.unwrap();
    tokio::fs::write(good.join("keep.png"), b"keep").await.unwrap();
    gc::reconcile(&state).await.unwrap();
    assert!(good.join("keep.png").exists(), "正常镜像树不该被清理动到");
}

/// 软删的墓碑不能把物理文件钉死。
///
/// 老版本里 `images.blob_id` 的外键挡着实体的行不让删，而软删行（deleted_at
/// 非空）会一直留着，于是「最后一张引用删了」之后文件永远回收不掉——
/// 用户看到的就是「删完了磁盘上还有一堆缩略图」。
#[tokio::test]
async fn soft_deleted_rows_do_not_pin_physical_files() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let (blob_id, path) = seed_blob(&state, 't').await;

    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "3005".to_string(),
            email: "tombstone@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let image = state
        .repo
        .create_image_reference(
            &pixora_repo::repo::images::NewImage {
                owner_id: user.id,
                album_id: None,
                blob_id,
                thumb_blob_id: None,
                original_name: "gone.png".to_string(),
                title: None,
                visibility: "private".to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            21,
            &[],
        )
        .await
        .unwrap();

    // 走真实的删除路径：软删行留下、引用计数归零
    state
        .delete_image(&pixora_core::images::Viewer::User(user.clone()), image.id)
        .await
        .unwrap();
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        0,
        "软删之后引用计数应当归零"
    );
    assert!(
        state.repo.find_image(image.id).await.unwrap().is_some(),
        "软删只是打标记，行还在——这正是它挡住外键的地方"
    );

    age_blob(&state, blob_id, 3).await;
    let removed = gc::collect_blobs(&state, 10, 10, 0).await.unwrap().removed;
    assert_eq!(removed, 1, "过宽限期后应当回收");
    assert!(!path.exists(), "物理文件要真删掉");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_none(),
        "实体行也要删掉"
    );
    assert!(
        state.repo.find_image(image.id).await.unwrap().is_none(),
        "墓碑跟着内容一起消失（没有回收站，留着没有意义）"
    );
}

/// 回收站：保留期内文件必须留着（恢复还要用），到期才回收。
#[tokio::test]
async fn binned_image_keeps_its_file_until_retention_expires() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let user = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "3006".to_string(),
            email: "bin@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    let (blob_id, path) = seed_blob(&state, 'r').await;
    let image = state
        .repo
        .create_image_reference(
            &pixora_repo::repo::images::NewImage {
                owner_id: user.id,
                album_id: None,
                blob_id,
                thumb_blob_id: None,
                original_name: "binned.png".to_string(),
                title: None,
                visibility: "private".to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            21,
            &[],
        )
        .await
        .unwrap();

    // 删除：进回收站（软删），引用计数归零
    state
        .delete_image(&pixora_core::images::Viewer::User(user.clone()), image.id)
        .await
        .unwrap();
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        0
    );

    // 保留 30 天：还没到期，文件与实体都不该被收走——收走了就恢复不出来
    age_blob(&state, blob_id, 3).await;
    let removed = gc::collect_blobs(&state, 5, 10, 30).await.unwrap().removed;
    assert_eq!(removed, 0, "保留期内的回收站内容不该被回收");
    assert!(path.exists(), "文件要留着给恢复用");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_some(),
        "实体行也要留着"
    );

    // 把墓碑时间推到 31 天前：过期了，这一轮应当回收
    state
        .repo
        .db()
        .execute(
            "UPDATE images SET deleted_at = ? WHERE id = ?",
            Args::new()
                .text(ts(Utc::now() - Duration::days(31)))
                .int(image.id),
        )
        .await
        .unwrap();
    let removed = gc::collect_blobs(&state, 5, 10, 30).await.unwrap().removed;
    assert_eq!(removed, 1, "过了保留期应当回收");
    assert!(!path.exists(), "过期后物理文件要真删掉");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_none(),
        "实体行也要删掉"
    );
}

/// 相册进回收站：相册与图片一起进去，恢复时整份回来。
#[tokio::test]
async fn album_bin_and_restore_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let (user, album, blob_id, path, _view) = seed_album_image(&state, 's', "3007").await;
    let image = state.repo.list_album_live_images(album.id).await.unwrap();
    assert_eq!(image.len(), 1);

    let before_used = state.repo.find_user_by_id(user.id).await.unwrap().unwrap().used_bytes;

    // 删相册 → 进回收站
    let moved = state
        .delete_album(&pixora_core::images::Viewer::User(user.clone()), album.id)
        .await
        .unwrap();
    assert_eq!(moved, 1, "应当带着 1 张图一起进去");
    assert!(
        state.repo.find_album_by_id(album.id).await.unwrap().is_none(),
        "回收站里的相册不出现在正常查询里"
    );
    assert_eq!(
        state.repo.count_deleted_albums(user.id).await.unwrap(),
        1,
        "回收站里应当有 1 个相册"
    );
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        0,
        "图片跟着进回收站，引用计数归零"
    );
    // 回收站里的文件仍然算已用额度：它占着盘，只有彻底删除才还回去
    let after_delete_used = state.repo.find_user_by_id(user.id).await.unwrap().unwrap().used_bytes;
    assert_eq!(after_delete_used, before_used, "进回收站不减少用量");
    assert!(path.exists(), "文件留着（保留期内要能恢复）");

    // 恢复：相册与图片一起回来
    let restored = state
        .restore_album(&pixora_core::images::Viewer::User(user.clone()), album.id)
        .await
        .unwrap();
    assert_eq!(restored, 1, "应当带回 1 张图");
    assert!(
        state.repo.find_album_by_id(album.id).await.unwrap().is_some(),
        "相册回到正常列表"
    );
    assert_eq!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().unwrap().ref_count,
        1,
        "引用计数回到 1"
    );
    let after_restore_used = state.repo.find_user_by_id(user.id).await.unwrap().unwrap().used_bytes;
    assert_eq!(after_restore_used, before_used, "恢复前后用量一致");
    assert_eq!(
        state.repo.list_album_live_images(album.id).await.unwrap().len(),
        1,
        "图片回到相册里"
    );
}

/// 后端配置被删掉之后，指向它的实体仍然要能收掉。
///
/// 这不是假想的情况：管理员在「存储管理」里删掉一个后端行之后，那个驱动就
/// 装配不出来了（远端驱动连地址带凭据都在配置行里）。以前这种实体会让 GC
/// 每轮都失败、行与字节永远留着——「数据库内」那种字节就留在库里，
/// 用户会看到「删了还在」。
///
/// 本地与数据库内两种驱动的落点与配置无关，所以这两种必须照常收掉。
#[tokio::test]
async fn blobs_left_by_a_deleted_backend_are_still_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;

    // 数据库内驱动的实体：字节在 storage_objects 里，backend_id 指向一个不存在的后端
    let sha = "d".repeat(64);
    let key = format!("blobs/{}/{}.png", &sha[..2], sha);
    state
        .repo
        .db()
        .execute(
            "INSERT INTO storage_objects (key, content, size, updated_at) VALUES (?, ?, ?, ?)",
            Args::new().text(&key).blob(b"in-db-bytes".to_vec()).int(11).text(now_ts()),
        )
        .await
        .unwrap();
    let blob = state
        .repo
        .insert_blob_if_absent(&NewBlob {
            sha256: sha.clone(),
            kind: "original".to_string(),
            size: 11,
            mime_type: "image/png".to_string(),
            ext: "png".to_string(),
            width: None,
            height: None,
            storage_driver: pixora_storage::names::DATABASE.to_string(),
            storage_key: key.clone(),
            backend_id: Some(4242), // 已经不存在的后端
        })
        .await
        .unwrap();
    age_blob(&state, blob.id, 3).await;

    let outcome = gc::collect_blobs(&state, 5, 10, 30).await.unwrap();
    assert_eq!(outcome.removed, 1, "孤儿实体应当被收掉");

    let left = state
        .repo
        .db()
        .count(
            "SELECT COUNT(*) AS total FROM storage_objects WHERE key = ?",
            Args::new().text(&key),
        )
        .await
        .unwrap();
    assert_eq!(left, 0, "库里的字节也要跟着删掉");
    assert!(
        state.repo.find_blob_by_id(blob.id).await.unwrap().is_none(),
        "实体行要删掉"
    );

    // 本地驱动同理：后端行没了，文件仍然在数据目录下，按 key 直接删
    let (local_id, local_path) = seed_blob(&state, 'e').await;
    state
        .repo
        .db()
        .execute(
            "UPDATE blobs SET backend_id = 4242 WHERE id = ?",
            Args::new().int(local_id),
        )
        .await
        .unwrap();
    age_blob(&state, local_id, 3).await;
    let outcome = gc::collect_blobs(&state, 5, 10, 30).await.unwrap();
    assert_eq!(outcome.removed, 1, "后端行没了的本地实体也应当被收掉");
    assert!(!local_path.exists(), "文件要真删掉");
}

/// 彻底删除（回收站里清掉）要当场把文件收走，而不是留一行「待回收」等人点。
///
/// 「彻底删除」的意思就是立刻销毁：引用归零之后没有任何人还需要这份字节。
/// 以前这一步只把墓碑删掉，物理文件要等下一轮 GC；存储分布页因此会出现一行
/// 「待回收」，还得管理员手动点一下——那是把本该自动的事推给了人。
#[tokio::test]
async fn purging_an_image_reclaims_the_file_right_away() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let (user, album, blob_id, path, _view) = seed_album_image(&state, 'f', "4001").await;
    let viewer = pixora_core::images::Viewer::User(user.clone());

    // 先删进回收站：文件必须还在（保留期内要能恢复）
    let image = state.repo.list_album_live_images(album.id).await.unwrap()[0].clone();
    state.delete_image(&viewer, image.id).await.unwrap();
    assert!(path.exists(), "进回收站的文件要留着");

    // 再彻底删除：文件与实体行都应当当场消失，不需要跑 GC
    let purged = state.purge_images(&viewer, &[image.id]).await.unwrap();
    assert_eq!(purged, 1);
    assert!(!path.exists(), "彻底删除要当场删掉物理文件");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_none(),
        "实体行也要一起删掉"
    );
}

/// 只删掉自己的那份引用时，别人的引用还在 → 文件不能动。
#[tokio::test]
async fn purging_one_reference_keeps_the_shared_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(&dir).await;
    let (user, album, blob_id, path, _view) = seed_album_image(&state, 'g', "4002").await;
    let viewer = pixora_core::images::Viewer::User(user.clone());

    // 另一个人引用同一份实体（内容寻址：磁盘上只有一份）
    let other = state
        .repo
        .create_user(&pixora_repo::repo::users::NewUser {
            uid: "4003".to_string(),
            email: "other@example.com".to_string(),
            password_hash: "$argon2id$placeholder".to_string(),
            role: "user".to_string(),
            status: "active".to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .unwrap();
    state
        .repo
        .create_image_reference(
            &pixora_repo::repo::images::NewImage {
                owner_id: other.id,
                album_id: None,
                blob_id,
                thumb_blob_id: None,
                original_name: "shared.png".to_string(),
                title: None,
                visibility: "private".to_string(),

                upload_ip: None,

                upload_agent: None,
            },
            21,
            &[],
        )
        .await
        .unwrap();

    let image = state.repo.list_album_live_images(album.id).await.unwrap()[0].clone();
    state.delete_image(&viewer, image.id).await.unwrap();
    let purged = state.purge_images(&viewer, &[image.id]).await.unwrap();
    assert_eq!(purged, 1);

    assert!(path.exists(), "别人还引用着这份实体，文件不能删");
    assert!(
        state.repo.find_blob_by_id(blob_id).await.unwrap().is_some(),
        "实体行也要留着"
    );
}
