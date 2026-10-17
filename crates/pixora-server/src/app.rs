//! 路由装配。
//!
//! 四块拼在一起：
//!
//!   1. `/api/v1/*` —— 契约里的全部接口。
//!   2. `/upload` —— 兼容旧图床的调用方式（字段名 `image`，响应结构固定）。
//!   3. `/healthz` —— 容器编排与反向代理用。
//!   4. 兜底 —— 先看是不是直链（`/{prefix}/{hash}.{ext}`），不是就当静态资源处理。
//!      SPA 路由回落也在这条兜底里（见 `assets::serve_asset`）。
//!
//! 中间件顺序有意安排：先限并发与体积（挡在最外层，恶意请求不进入业务逻辑），
//! 再超时，再追踪，最后压缩。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::routing::{delete, get, patch, post, put};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use pixora_core::AppState;

use crate::assets;
use crate::direct;

/// 上传请求体的上限。单文件上限在设置里（默认 20MiB），这里留出 multipart
/// 头部与缩略图的空间；再大的请求连业务逻辑都进不去。
const MAX_UPLOAD_BODY: usize = 64 * 1024 * 1024;

pub fn build(state: Arc<AppState>) -> Router {
    let api = api_routes();

    let router = Router::new()
        .route("/healthz", get(crate::routes::public::healthz))
        .nest("/api/v1", api)
        .fallback(fallback)
        // 中间件由外到内：限体积 → 限时 → 追踪 → 压缩
        .layer(RequestBodyLimitLayer::new(MAX_UPLOAD_BODY))
        // 超时返回 408：客户端等的是「服务在忙」而不是 500
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(120),
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY))
        .with_state(state.clone());

    // 全站 IP 规则放在业务层之外：被拉黑的人不该还能浏览广场、拉直链。
    // 再往外套一层安全响应头，连被 IP 规则拦下的响应也带上。
    router
        .layer(axum::middleware::from_fn_with_state(
            state,
            crate::routes::enforce_ip,
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("content-security-policy"),
            // 同一个源要允许：扫码登录的 OAuth 回调是嵌在自己页面里的 iframe。
            HeaderValue::from_static("frame-ancestors 'self'"),
        ))
}

fn api_routes() -> Router<Arc<AppState>> {
    use crate::routes as r;

    Router::new()
        // ---------- 公开 ----------
        .route("/site", get(r::public::site))
        .route("/site/background", get(r::public::site_background))
        .route("/site/avatar", get(r::public::site_avatar))
        .route("/plaza", get(r::public::plaza))
        .route("/random", get(r::public::random_image))
        .route("/terms", get(r::public::terms))
        .route("/terms/consent", post(r::public::terms_consent))
        // ---------- 认证 ----------
        .route("/auth/register/code", post(r::auth::register_code))
        .route("/auth/register", post(r::auth::register))
        .route("/auth/login", post(r::auth::login))
        .route("/auth/logout", post(r::auth::logout))
        .route("/auth/password/code", post(r::auth::password_code))
        // 第三方登录：start 跳去对方站点，callback 带着授权码回来
        .route("/auth/oauth/{provider}/start", get(r::oauth::oauth_start))
        .route(
            "/auth/oauth/{provider}/callback",
            get(r::oauth::oauth_callback),
        )
        .route("/auth/oauth/complete", post(r::oauth::complete_signup))
        // 手机号 + 验证码
        .route("/auth/phone/code", post(r::oauth::phone_code))
        .route("/auth/phone/login", post(r::oauth::phone_login))
        // 登录第二步可以改用邮箱验证码（验证器不在手边时）
        .route("/auth/login/email-code", post(r::auth::login_email_code))
        .route("/auth/password/reset", post(r::auth::password_reset))
        // 邮件里的重置链接：GET 换「给谁设密码」，POST 交新密码（令牌一次性）
        .route("/auth/password/link", get(r::auth::password_link_info))
        .route(
            "/auth/password/link/reset",
            post(r::auth::password_link_reset),
        )
        // ---------- 当前用户 ----------
        .route("/me", get(r::account::me_get).patch(r::account::me_patch))
        .route("/me/password", post(r::account::password_change))
        // 换绑邮箱：先给新地址发码，再带上码提交
        .route("/me/email/code", post(r::account::email_change_code))
        .route("/me/email", post(r::account::email_change))
        .route(
            "/me/identities",
            get(r::oauth::identities_get).post(r::oauth::identity_bind),
        )
        .route("/me/identities/phone/code", post(r::oauth::phone_bind_code))
        .route(
            "/me/identities/{provider}",
            delete(r::oauth::identity_unbind),
        )
        .route("/me/totp", post(r::account::totp))
        .route(
            "/me/recovery-codes",
            get(r::account::recovery_codes_get).post(r::account::recovery_codes_create),
        )
        .route(
            "/me/sessions",
            get(r::account::sessions_list).delete(r::account::sessions_revoke),
        )
        .route("/me/sessions/{id}", delete(r::account::session_revoke))
        .route(
            "/me/tokens",
            get(r::account::tokens_list).post(r::account::tokens_create),
        )
        .route(
            "/me/tokens/{id}",
            patch(r::account::token_rename).delete(r::account::token_revoke),
        )
        // ---------- 相册与图片 ----------
        .route(
            "/albums",
            get(r::catalog::albums_list).post(r::catalog::album_create),
        )
        .route(
            "/albums/{id}",
            patch(r::catalog::album_update).delete(r::catalog::album_delete),
        )
        .route("/images", get(r::catalog::images_list))
        .route(
            "/images/{id}",
            get(r::catalog::image_detail)
                .patch(r::catalog::image_update)
                .delete(r::catalog::image_delete),
        )
        .route("/images/batch", post(r::catalog::images_batch))
        // ---------- 回收站 ----------
        .route("/recycle/images", get(r::recycle::images_list))
        .route("/recycle/images/restore", post(r::recycle::images_restore))
        .route("/recycle/images/purge", post(r::recycle::images_purge))
        .route("/recycle/images/empty", post(r::recycle::images_empty))
        .route("/recycle/albums", get(r::recycle::albums_list))
        .route(
            "/recycle/albums/{id}/restore",
            post(r::recycle::album_restore),
        )
        .route("/recycle/albums/{id}", delete(r::recycle::album_purge))
        .route(
            "/tags",
            get(r::catalog::tags_list).patch(r::catalog::tags_patch),
        )
        .route("/fs/tree", get(r::catalog::fs_tree))
        // ---------- 上传 ----------
        .route("/blobs/exists", post(r::upload::blobs_exists))
        .route("/upload", post(r::upload::upload))
        .route("/queue/{job_id}", get(r::upload::queue_status))
        // ---------- 管理后台 ----------
        .route("/admin/stats", get(r::admin::stats))
        .route(
            "/admin/users",
            get(r::admin::users_list).post(r::admin::user_create),
        )
        .route(
            "/admin/users/{id}",
            get(r::admin::user_detail)
                .patch(r::admin::user_update)
                .delete(r::admin::user_delete),
        )
        .route("/admin/users/{id}/quota", post(r::admin::user_quota))
        .route("/admin/users/{id}/status", post(r::admin::user_status))
        // 管理员代发重置邮件：与「直接设新密码」是两条路，这条让用户自己设
        .route(
            "/admin/users/{id}/password-reset-mail",
            post(r::admin::user_password_reset_mail),
        )
        .route(
            "/admin/ratelimit/users/{uid}",
            put(r::admin::ratelimit_user_put).delete(r::admin::ratelimit_user_delete),
        )
        .route("/admin/ratelimit/logs", get(r::admin::ratelimit_logs))
        // 账号历史（注册 / 换邮箱 / 处置 / 注销 / 登录）：账号删掉也查得到
        .route("/admin/logs", get(r::admin::account_logs))
        .route(
            "/admin/ratelimit",
            get(r::admin::ratelimit_get).put(r::admin::ratelimit_global_put),
        )
        // 契约与前端都写过 `/admin/ratelimit/global`：这里收成同一个处理函数，
        // 两种路径都能存，少一个「服务端返回了无法解析的内容」（那条其实是
        // 请求落到 SPA 静态资源上了）
        .route(
            "/admin/ratelimit/global",
            put(r::admin::ratelimit_global_put),
        )
        .route(
            "/admin/ratelimit/on_breach",
            put(r::admin::ratelimit_breach_put),
        )
        .route("/admin/storage", get(r::admin::storage_get))
        // 重算缩略图：老图按新尺寸重生成一遍（后台队列分批做）
        .route(
            "/admin/storage/thumbs/rebuild",
            post(r::admin::storage_thumbs_rebuild),
        )
        .route("/admin/storage/gc", post(r::admin::storage_gc))
        .route("/admin/storage/default", put(r::admin::storage_default))
        .route("/admin/storage/prefix", put(r::admin::storage_prefix))
        .route("/admin/storage/capacity", put(r::admin::storage_capacity))
        .route("/admin/storage/backends", post(r::admin::backend_create))
        .route(
            "/admin/storage/backends/{id}",
            patch(r::admin::backend_update).delete(r::admin::backend_delete),
        )
        .route(
            "/admin/storage/backends/{id}/test",
            post(r::admin::backend_test),
        )
        .route(
            "/admin/quota",
            get(r::admin::quota_get).put(r::admin::quota_put),
        )
        .route("/admin/ip", get(r::admin::ip_get))
        .route("/admin/ip/rules", post(r::admin::ip_rule_create))
        .route("/admin/ip/rules/{id}", delete(r::admin::ip_rule_delete))
        .route("/admin/ip/policy", put(r::admin::ip_policy_put))
        .route(
            "/admin/settings",
            get(r::admin::settings_get).put(r::admin::settings_put),
        )
        .route(
            "/admin/users/{id}/impersonate",
            post(r::admin::user_impersonate),
        )
        .route("/admin/impersonate/stop", post(r::admin::impersonate_stop))
        .route(
            "/admin/settings/background",
            post(r::admin::background_upload),
        )
        // 站点默认头像：没设过头像的账号（含新注册的）都用它
        .route("/admin/settings/avatar", post(r::admin::avatar_upload))
        .route("/admin/settings/mail/test", post(r::admin::mail_test))
        .route(
            "/admin/login-methods",
            get(r::admin::admin_login_methods_get).put(r::admin::admin_login_methods_put),
        )
        .route(
            "/admin/users/{id}/identities/{provider}",
            delete(r::admin::admin_identity_unbind),
        )
        // 邮件模板：管理员能在后台改文案、恢复内置
        .route("/admin/mail/templates", get(r::admin::mail_templates_get))
        .route(
            "/admin/mail/templates/{name}",
            put(r::admin::mail_template_put).delete(r::admin::mail_template_delete),
        )
        .route("/admin/settings/formats", post(r::admin::formats_put))
        .route(
            "/admin/terms",
            get(r::admin::terms_list).post(r::admin::terms_create),
        )
        .route(
            "/admin/terms/{id}",
            patch(r::admin::terms_update).delete(r::admin::terms_delete),
        )
        .route("/admin/terms/{id}/publish", post(r::admin::terms_publish))
        // ---------- 安装向导 ----------
        .route("/install/status", get(r::install::install_status))
        .route("/install/database", post(r::install::install_database))
        .route("/install/admin", post(r::install::install_admin))
        .route("/install/site", post(r::install::install_site))
        .route("/install/finish", post(r::install::install_finish))
}

/// 兜底：直链优先，其余交给静态资源。
///
/// 直链的形状是「两级路径 + 第二级是 64 位十六进制哈希加扩展名」。
/// 这个判断必须严格：把前端路由误判成直链会让用户看到 404，
/// 把不存在的直链误判成页面又会让爬虫拿到一堆 HTML。
async fn fallback(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    Query(params): Query<std::collections::HashMap<String, String>>,
    peer: crate::routes::PeerAddr,
    headers: HeaderMap,
) -> axum::response::Response {
    let path = uri.path().trim_start_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();

    if segments.len() == 2 && looks_like_direct_file(segments[1]) {
        return direct::serve_direct(
            State(state),
            Path((segments[0].to_string(), segments[1].to_string())),
            Query(params),
            peer,
            headers,
        )
        .await;
    }

    assets::serve_asset(&state, &headers, &uri).await
}

/// 判断文件名是否是 `{64位十六进制}.{ext}`。
fn looks_like_direct_file(file: &str) -> bool {
    let Some((hash, ext)) = file.rsplit_once('.') else {
        return false;
    };
    hash.len() == 64
        && hash.chars().all(|ch| ch.is_ascii_hexdigit())
        && !ext.is_empty()
        && ext.len() <= 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_link_shape_is_detected() {
        let hash = "a3f9c1b8d2e4f6a7b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2";
        assert!(looks_like_direct_file(&format!("{hash}.webp")));
        assert!(looks_like_direct_file(&format!("{hash}.jpg")));

        // 短哈希 / 非法字符 / 没有扩展名都不是直链
        assert!(!looks_like_direct_file("a3f9c1b8.webp"));
        assert!(!looks_like_direct_file(&format!("{}z.webp", &hash[..63])));
        assert!(!looks_like_direct_file("index.html"));
        assert!(!looks_like_direct_file(&format!("{hash}")));
    }

    #[test]
    fn spa_routes_are_not_mistaken_for_direct_links() {
        assert!(!looks_like_direct_file("albums"));
        assert!(!looks_like_direct_file("main.js"));
        assert!(!looks_like_direct_file("manifest.webmanifest"));
    }
}
