//! 安装向导（`/api/v1/install/*`）。
//!
//! **未安装时可用，安装完成后一律 404**：安装接口能在生产环境里重新跑一次，
//! 等于给了一个「重设管理员邮箱与密码」的公开入口。所以每个 handler 第一件事
//! 都是问 `settings.install.completed`，装完就整体下线。
//!
//! 与领域层的关系要说明白：`pixora-core` 里的 `terms` / `install` 两个模块
//! 目前只有占位实现（见 `crates/pixora-core/src/install.rs`），
//! 所以这几个 handler 直接调用仓储与 `Settings` 完成同样的语义——
//! 「创建第一个管理员」与「写站点配置」本身没有比仓储更进一步的业务规则。
//! 等 `pixora_core::install` 补齐后，这里应当收敛回领域调用（接口形状不变）。
//!
//! 数据库那一步有一个诚实的限制：**运行中的进程不会换库**。配置只走环境变量
//! （ADR-0003），所以向导做的是「连通性 + 建表」验证，并把「要重启才生效」
//! 明确写在返回的 message 里，而不是假装已经切过去了。

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use pixora_core::settings::keys;
use pixora_core::install::DbInput;
use pixora_core::{AppError, AppState};
use pixora_crypto::password;
use pixora_repo::db::DbDriver;
use pixora_repo::model::{role, status};
use pixora_repo::repo::users::{NewUser, UserFilter};
use pixora_repo::{Args, PageRequest};

use crate::http::{finish, request_context};

use super::{bad_request, field_opt_bool, field_opt_i64, field_opt_str, field_str, not_found, PeerAddr};

/// 已安装就不再提供向导接口。
async fn ensure_not_installed(state: &AppState) -> Result<(), Response> {
    match state.settings.is_installed().await {
        Ok(true) => Err(not_found("安装向导（本实例已完成安装）")),
        Ok(false) => Ok(()),
        Err(err) => Err(crate::http::error_response(err)),
    }
}

/// 数一下库里有多少张表。
///
/// 只用于向导界面上的「已检测到 N 张表」提示，查不出来就当 0——
/// 这一步失败不该拦住安装。
async fn count_tables(state: &AppState) -> i64 {
    let sql = match state.db_driver {
        DbDriver::Sqlite => {
            "SELECT COUNT(*) AS total FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
        }
        DbDriver::Postgres => {
            "SELECT COUNT(*) AS total FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')"
        }
        DbDriver::MySql => {
            "SELECT COUNT(*) AS total FROM information_schema.tables WHERE table_schema = DATABASE()"
        }
    };
    state
        .repo
        .db()
        .count(sql, Args::new())
        .await
        .unwrap_or(0)
}

/// 数据库驱动的对外名字。
fn driver_name(driver: DbDriver) -> &'static str {
    match driver {
        DbDriver::Postgres => "postgres",
        DbDriver::MySql => "mysql",
        DbDriver::Sqlite => "sqlite",
    }
}

/// `GET /api/v1/install/status` — 向导前端据此决定从第几步开始。
pub async fn install_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let _rectx = request_context(&state, &headers, peer.addr()).await;
    if let Err(resp) = ensure_not_installed(&state).await {
        return resp;
    }

    // 步骤：库连不上 → 1；库里没有管理员 → 2；有管理员但没走完 → 3
    let db_ok = state.repo.db().ping().await.is_ok();
    let admin_count = state
        .repo
        .list_users(
            &UserFilter {
                role: Some(role::ADMIN.to_string()),
                ..Default::default()
            },
            PageRequest::new(Some(1), Some(1), 1, 1),
        )
        .await
        .map(|page| page.total)
        .unwrap_or(0);
    let step = if !db_ok {
        1
    } else if admin_count == 0 {
        2
    } else {
        3
    };

    // 数据目录的来源如实转述：界面上要显示「这份配置是从哪来的」
    let env_source = if std::env::var("PIXORA_DATA_DIR").is_ok() {
        "PIXORA_DATA_DIR"
    } else {
        "默认值（./data）"
    };

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "installed": false,
        "step": step,
        "env": {
            "data_dir": state.config.data_dir.display().to_string(),
            "env_source": env_source,
            "db_driver": driver_name(state.db_driver),
            "db_dsn_set": !state.config.db_dsn.trim().is_empty(),
            "master_key_set": state.config.master_key.is_some(),
            "master_key_source": state.config.key_source.describe(),
        },
        "tables": count_tables(&state).await,
    })))
}

/// `POST /api/v1/install/database` — 验证数据库参数并建表。
pub async fn install_database(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let _rectx = request_context(&state, &headers, peer.addr()).await;
    if let Err(resp) = ensure_not_installed(&state).await {
        return resp;
    }

    // 交给领域层：它探的是**当前进程已经连着的那个池**，不会因为另开一个池
    // 与后台任务抢 SQLite 写锁（早期版本就在这里踩过，表现是启动头几秒
    // 连同一个库都会「pool timed out」）。数据库配置只走环境变量，
    // 向导本来也无法真的切换运行中的连接。
    let driver_raw = match field_str(&body, "driver") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let input = DbInput {
        driver: driver_raw.clone(),
        host: field_opt_string(&body, "host").unwrap_or_default(),
        port: body.get("port").and_then(|value| value.as_i64()).unwrap_or(0),
        database: field_opt_string(&body, "database").unwrap_or_default(),
        username: field_opt_string(&body, "username").unwrap_or_default(),
        password: field_opt_string(&body, "password").unwrap_or_default(),
    };

    match state.install_check_database(input).await {
        Ok(message) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "ok": true,
            "message": message,
        }))),
        Err(err) => crate::http::error_response(err),
    }
}

/// 可选的字符串字段。
fn field_opt_string(body: &serde_json::Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `POST /api/v1/install/admin` — 创建第一个管理员。
///
/// 「第一个」是硬条件：向导装完就该下线，但万一没下线，这一步也不能变成
/// 「谁都能再建一个管理员」的入口。
pub async fn install_admin(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    if let Err(resp) = ensure_not_installed(&state).await {
        return resp;
    }

    let email = match field_str(&body, "email") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let email = match pixora_core::auth::normalize_email_input(&email) {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };
    let password_plain = match field_str(&body, "password") {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    if let Err(err) = password::check_strength(&password_plain) {
        // 口令强度不足是客户端错误，转成 AppError 才能带上正确的中文说明与错误码
        return crate::http::error_response(AppError::bad_request(err.to_string()));
    }

    let admins = state
        .repo
        .list_users(
            &UserFilter {
                role: Some(role::ADMIN.to_string()),
                ..Default::default()
            },
            PageRequest::new(Some(1), Some(1), 1, 1),
        )
        .await
        .map(|page| page.total)
        .unwrap_or(0);
    if admins > 0 {
        return bad_request("实例里已经有管理员账号，安装向导不再接受新的管理员创建");
    }
    if state.repo.email_exists(&email).await.unwrap_or(false) {
        return crate::http::error_response(AppError::Conflict("该邮箱已被占用".into()));
    }

    let hash = match password::hash_password(&password_plain) {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err.into()),
    };
    let (default_quota, _) = match state.settings.quota_values().await {
        Ok(value) => value,
        Err(err) => return crate::http::error_response(err),
    };

    // 第一个管理员优先用 `1000` 这个「整百」UID：它是别人的参照（备份脚本、
    // 目录结构都爱拿它举例），被占了才退回随机生成
    let mut created = None;
    for candidate in uid_candidates() {
        match state
            .repo
            .create_user(&NewUser {
                uid: candidate.clone(),
                email: email.clone(),
                password_hash: hash.clone(),
                role: role::ADMIN.to_string(),
                status: status::ACTIVE.to_string(),
                quota_bytes: default_quota,
                email_verified: true,
            })
            .await
        {
            Ok(user) => {
                created = Some(user);
                break;
            }
            Err(err) if err.is_unique_violation() => continue,
            Err(err) => return crate::http::error_response(err.into()),
        }
    }
    let Some(admin) = created else {
        return crate::http::error_response(AppError::internal("UID 分配失败，请重试"));
    };

    // 默认相册：没有它的话，新用户在「我的相册」里看到的是空列表，
    // 上传时也没有默认的目标
    if let Err(err) = state.repo.insert_default_album(admin.id, "默认相册").await {
        return crate::http::error_response(err.into());
    }

    // 站点名在这一步顺带写上（安装向导的第二个输入框就是它）
    if let Some(site_name) = field_opt_str(&body, "site_name") {
        if let Err(err) = state.settings.set(keys::SITE_NAME, &site_name).await {
            return crate::http::error_response(err);
        }
    }

    state
        .audit_detail(
            pixora_core::audit::actions::REGISTER,
            Some(admin.id),
            &admin.uid,
            serde_json::json!({ "email": admin.email, "install": true }),
            rectx.ip.as_deref(),
        )
        .await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "uid": admin.uid,
        "email": admin.email,
    })))
}

/// 第一个管理员尝试的 UID：先试整百，再退回随机。
fn uid_candidates() -> Vec<String> {
    let mut list = vec!["1000".to_string()];
    for _ in 0..12 {
        if let Ok(uid) = pixora_crypto::token::new_uid(4) {
            list.push(uid);
        }
    }
    list
}

/// `POST /api/v1/install/site` — 站点地址、直链前缀与 SMTP。
pub async fn install_site(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let _rectx = request_context(&state, &headers, peer.addr()).await;
    if let Err(resp) = ensure_not_installed(&state).await {
        return resp;
    }

    if let Some(base_url) = field_opt_str(&body, "base_url") {
        let base_url = base_url.trim_end_matches('/').to_string();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return bad_request("站点地址要以 http:// 或 https:// 开头");
        }
        // 反代后面自动探测出来的是容器内网地址，直链会全错——所以这里要求显式填写
        if let Err(err) = state.settings.set(keys::SITE_BASE_URL, &base_url).await {
            return crate::http::error_response(err);
        }
    }
    if let Some(prefix) = field_opt_str(&body, "upload_prefix") {
        let prefix = pixora_core::settings::normalize_prefix(&prefix);
        if let Err(err) = state.settings.set(keys::UPLOAD_PREFIX, &prefix).await {
            return crate::http::error_response(err);
        }
    }
    if let Some(plaza_public) = field_opt_bool(&body, "plaza_public") {
        if let Err(err) = state.settings.set_bool(keys::PLAZA_PUBLIC, plaza_public).await {
            return crate::http::error_response(err);
        }
    }

    if let Some(smtp) = body.get("smtp").and_then(|value| value.as_object()) {
        let smtp = serde_json::Value::Object(smtp.clone());
        let writes: [(&str, Option<String>); 5] = [
            (keys::MAIL_SMTP_HOST, field_opt_str(&smtp, "host")),
            (
                keys::MAIL_SMTP_PORT,
                field_opt_i64(&smtp, "port").map(|value| value.to_string()),
            ),
            (keys::MAIL_SMTP_SECURITY, field_opt_str(&smtp, "security")),
            (keys::MAIL_SMTP_USER, field_opt_str(&smtp, "user")),
            (keys::MAIL_FROM_NAME, field_opt_str(&smtp, "from_name")),
        ];
        for (key, value) in writes {
            if let Some(value) = value {
                if let Err(err) = state.settings.set(key, &value).await {
                    return crate::http::error_response(err);
                }
            }
        }
        // 发信地址没给就沿用 SMTP 用户名（企业邮箱里两者通常一致）；
        // 两个都没有就保持原值，不写一个看起来像地址的默认值进去
        let from_email = field_opt_str(&smtp, "from_email").or_else(|| field_opt_str(&smtp, "user"));
        if let Some(from_email) = from_email {
            if let Err(err) = state.settings.set(keys::MAIL_FROM_EMAIL, &from_email).await {
                return crate::http::error_response(err);
            }
        }
        // SMTP 密码是凭据，加密入库；给了才写
        if let Some(secret) = field_opt_str(&smtp, "password") {
            if let Err(err) = state
                .settings
                .set_secret(keys::MAIL_SMTP_PASSWORD, &secret)
                .await
            {
                return crate::http::error_response(err);
            }
        }
        state.reload_mailer().await;
    }

    finish::<serde_json::Value>(Ok(serde_json::json!({ "ok": true })))
}

/// `POST /api/v1/install/finish` — 收尾：标记已安装。
///
/// 标记之后再调任何安装接口都是 404，所以这一步相当于「关掉向导」。
pub async fn install_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    peer: PeerAddr,
) -> Response {
    let _rectx = request_context(&state, &headers, peer.addr()).await;
    if let Err(resp) = ensure_not_installed(&state).await {
        return resp;
    }

    let admin = state
        .repo
        .list_users(
            &UserFilter {
                role: Some(role::ADMIN.to_string()),
                ..Default::default()
            },
            PageRequest::new(Some(1), Some(1), 1, 1),
        )
        .await
        .ok()
        .and_then(|page| page.items.into_iter().next());
    let Some(admin) = admin else {
        return bad_request("还没有管理员账号，请先完成「创建管理员」这一步");
    };

    if let Err(err) = state.settings.mark_installed().await {
        return crate::http::error_response(err);
    }
    // 装完顺手把发信器装起来：站点配置刚写完，SMTP 是否可用要立刻生效
    state.reload_mailer().await;

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "ok": true,
        "admin_uid": admin.uid,
        "login_url": "/login",
    })))
}
