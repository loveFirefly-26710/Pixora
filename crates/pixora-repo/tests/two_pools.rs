//! 同一个 SQLite 文件上开两个池的场景。
//!
//! 安装向导会做这件事：向导要验证「用户填的连接串」能不能用，于是另开一个池连过去。
//! 如果两个池会互相锁死，安装向导在 SQLite 上就永远走不完。

use std::sync::Arc;

use pixora_repo::db::{Db, DbDriver};
use pixora_repo::Repo;

#[tokio::test]
async fn two_pools_on_same_sqlite_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.db");
    let dsn = path.to_string_lossy().to_string();

    let first = Db::connect(DbDriver::Sqlite, &dsn, 4).await.expect("第一个池");
    first.migrate().await.expect("迁移");
    let repo = Repo::new(Arc::new(first));

    // 第一个池上做一次写
    repo.db()
        .execute(
            "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?)",
            pixora_repo::Args::new()
                .text("probe")
                .text("1")
                .bool(false)
                .text(pixora_repo::model::now_ts()),
        )
        .await
        .expect("写入");

    // 再开一个池连同一个文件（安装向导的路径）
    let second = Db::connect(DbDriver::Sqlite, &dsn, 2)
        .await
        .expect("第二个池应当能连上");
    let count = second
        .count("SELECT COUNT(*) AS total FROM settings", pixora_repo::Args::new())
        .await
        .expect("第二个池应当能查");
    assert_eq!(count, 1);

    // 第二个池写回第一个池也要能看见
    second
        .execute(
            "INSERT INTO settings (name, value, encrypted, updated_at) VALUES (?, ?, ?, ?)",
            pixora_repo::Args::new()
                .text("probe2")
                .text("2")
                .bool(false)
                .text(pixora_repo::model::now_ts()),
        )
        .await
        .expect("第二个池应当能写");
    let total = repo
        .db()
        .count("SELECT COUNT(*) AS total FROM settings", pixora_repo::Args::new())
        .await
        .expect("第一个池应当能读到新行");
    assert_eq!(total, 2);

    second.close().await;
    repo.db().close().await;
}
