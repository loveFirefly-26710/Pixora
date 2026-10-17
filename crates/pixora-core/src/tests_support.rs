//! 测试脚手架：装一个只跑 SQLite + 本地目录的 `AppState`。
//!
//! 只在 `cfg(test)` 下编译。抽出来是因为不止一个模块要建实例——
//! 各自抄一份的话，`Config` 一加字段就要满仓找一遍。

use std::sync::Arc;

use tempfile::TempDir;

use crate::config::{Config, KeySource};
use crate::AppState;
use pixora_repo::model::User;
use pixora_repo::repo::users::NewUser;

/// 装一个跑在临时目录上的实例。数据库、数据目录、主密钥都在 `dir` 里。
///
/// 同源 Cookie、不联网：多数用例的默认环境。
pub async fn test_state(dir: &TempDir) -> Arc<AppState> {
    let config = test_config(dir);
    let state = AppState::bootstrap(config).await.expect("装配 AppState");
    // 测试不联网：发信前的收件域名校验（MX 查询）默认关掉。哪条测试要验它，
    // 自己在用例里打开——查询真实 DNS 的用例在这里是少数
    state
        .settings
        .set_bool(crate::settings::keys::ANTISPAM_MX, false)
        .await
        .expect("关掉 MX 校验");
    state
}

/// 一份跑在临时目录上的配置。前后端同源，Cookie 用默认的 `Lax`。
pub fn test_config(dir: &TempDir) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().expect("监听地址"),
        db_driver: pixora_repo::DbDriver::Sqlite,
        db_dsn: dir
            .path()
            .join("pixora-test.db")
            .to_string_lossy()
            .into_owned(),
        db_max_connections: 2,
        data_dir: dir.path().to_path_buf(),
        master_key: Some(pixora_crypto::MasterKey::generate().expect("主密钥")),
        key_source: KeySource::Generated(dir.path().join("master.key")),
        workers: 1,
        base_url: None,
        trust_proxy: 0,
        web_dir: None,
        accel_prefix: None,
        secure_cookies: false,
        samesite: crate::config::SameSite::Lax,
        dev_return_codes: false,
        log_level: "warn".into(),
    }
}

/// 造一个已激活的普通用户。多数测试只关心它的 id。
pub async fn make_user(state: &AppState, email: &str, uid: &str) -> User {
    state
        .repo
        .create_user(&NewUser {
            uid: uid.to_string(),
            email: email.to_string(),
            password_hash: "$argon2id$fake".to_string(),
            role: pixora_repo::model::role::USER.to_string(),
            status: pixora_repo::model::status::ACTIVE.to_string(),
            quota_bytes: 1024 * 1024,
            email_verified: true,
        })
        .await
        .expect("建用户")
}
