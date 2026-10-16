//! 跨域支持：把前端托管在别处时，让浏览器肯发这几条请求。
//!
//! 默认什么都不做——`PIXORA_CORS_ORIGINS` 为空时一个 `Access-Control-*` 头都不发，
//! 与从前完全一致。配了才启用，行为只有三条：
//!
//!   1. 来源在白名单里才回 `Access-Control-Allow-Origin`（回的是请求里的那个来源，
//!      不是 `*`：带凭据的跨域不允许通配，回显才能让浏览器放行）；
//!   2. 预检（`OPTIONS` + `Access-Control-Request-Method`）就地答掉，不进业务逻辑；
//!   3. 其它请求照常走，只是在响应上补那几个头。
//!
//! 白名单之外的来源不发头（浏览器自己会拦），预检则直接 403 并留一条日志——
//! 「跨域没配好」这种故障从前端看只有一句 `failed to fetch`，日志里得说清是
//! 哪个来源被拒了。
//!
//! ## 关于 Cookie
//!
//! 跨站要用会话就必须 `SameSite=None; Secure`（见 `Config::samesite`），
//! 这里不管 Cookie 本身，只负责让请求过得去、响应能被读到。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use pixora_core::AppState;

/// 预检结果的缓存时间。改配置后最多一小时内仍按旧结果放行，够用且省一次往返。
const PREFLIGHT_MAX_AGE: &str = "3600";

/// 允许的请求头。写死了这三类：JSON、CSRF 双提交、Token 鉴权。
const ALLOW_HEADERS: &str =
    "content-type, x-csrf-token, authorization, x-api-key, accept, accept-language";

/// 允许的请求方法。`OPTIONS` 也要写上，某些客户端会自己发一次。
const ALLOW_METHODS: &str = "GET, HEAD, POST, PATCH, PUT, DELETE, OPTIONS";

/// 响应里值得被前端读到的头。
///
/// CORS 下前端只能读到几个「安全」响应头，其余的要显式放行。
/// `Retry-After` 是限流提示（前端据此退避），`Content-Disposition` 是下载文件名。
const EXPOSE_HEADERS: &str = "Retry-After, Content-Disposition";

pub async fn cors(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    // 没有 Origin 的请求（同源、curl、服务端互调）压根不涉及 CORS
    let Some(origin) = origin else {
        return next.run(request).await;
    };
    let allowed = state.config.allows_origin(&origin);
    let is_preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);

    if is_preflight {
        if !allowed {
            if !state.config.cors_origins.is_empty() {
                tracing::warn!(
                    origin = %origin,
                    path = %request.uri().path(),
                    "跨域预检被拒：这个来源不在 PIXORA_CORS_ORIGINS 里"
                );
                return forbidden(origin);
            }
            // 没配跨域时按普通请求处理（走到静态资源或直链上）
            return next.run(request).await;
        }
        return preflight(origin);
    }

    let mut response = next.run(request).await;
    // 无论放行与否都要 `Vary: Origin`：让中间缓存知道这份响应与来源相关
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    if allowed {
        apply_allow_origin(response.headers_mut(), &origin);
    }
    response
}

/// 预检成功：204 + 全套 `Access-Control-*`。
fn preflight(origin: String) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    apply_allow_origin(headers, &origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(ALLOW_METHODS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOW_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    response
}

fn apply_allow_origin(headers: &mut axum::http::HeaderMap, origin: &str) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    if let Ok(value) = HeaderValue::from_str(EXPOSE_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
}

fn forbidden(origin: String) -> Response {
    let body = format!(
        r#"{{"data":null,"error":{{"code":"FORBIDDEN","message":"来源 {origin} 不在允许的跨域名单里"}}}}"#
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use pixora_core::config::{Config, KeySource, SameSite};
    use pixora_repo::DbDriver;
    use tower::ServiceExt;

    /// 一个跑在临时目录上的实例，`origins` 是逗号分隔的放行来源。
    async fn state_with_origins(origins: &str) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let list: Vec<String> = origins
            .split(',')
            .map(|item| item.trim().trim_end_matches('/').to_string())
            .filter(|item| !item.is_empty())
            .collect();
        let samesite = if list.is_empty() {
            SameSite::Lax
        } else {
            SameSite::None
        };
        let config = Config {
            bind: "127.0.0.1:0".parse().expect("监听地址"),
            db_driver: DbDriver::Sqlite,
            db_dsn: dir
                .path()
                .join("pixora-cors.db")
                .to_string_lossy()
                .into_owned(),
            db_max_connections: 2,
            data_dir: dir.path().to_path_buf(),
            master_key: Some(pixora_crypto::MasterKey::generate().expect("主密钥")),
            key_source: KeySource::Generated(dir.path().join("master.key")),
            workers: 1,
            base_url: Some("https://img.example.com".into()),
            trust_proxy: 0,
            web_dir: None,
            accel_prefix: None,
            secure_cookies: false,
            dev_return_codes: false,
            log_level: "error".into(),
            cors_origins: list,
            samesite,
        };
        let state = AppState::bootstrap(config).await.expect("装配 AppState");
        (state, dir)
    }

    fn app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/api/v1/site", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, cors))
    }

    #[tokio::test]
    async fn no_config_means_no_cors_headers() {
        let (state, _dir) = state_with_origins("").await;
        let request = HttpRequest::builder()
            .uri("/api/v1/site")
            .header("origin", "https://pixora.vercel.app")
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn allowed_origin_gets_credentials_headers() {
        let (state, _dir) = state_with_origins("https://pixora.vercel.app").await;
        let request = HttpRequest::builder()
            .uri("/api/v1/site")
            .header("origin", "https://pixora.vercel.app")
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://pixora.vercel.app"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .unwrap(),
            "true"
        );
        // 缓存要按来源分开，否则放行的那个来源会把别人的响应喂出去
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Origin");
    }

    #[tokio::test]
    async fn preflight_is_answered_without_touching_the_route() {
        let (state, _dir) = state_with_origins("https://pixora.vercel.app").await;
        let request = HttpRequest::builder()
            .method(Method::OPTIONS)
            .uri("/api/v1/site")
            .header("origin", "https://pixora.vercel.app")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-csrf-token")
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert!(headers
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("POST"));
        assert!(headers
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("x-csrf-token"));
    }

    #[tokio::test]
    async fn foreign_origin_preflight_is_rejected_with_a_reason() {
        let (state, _dir) = state_with_origins("https://pixora.vercel.app").await;
        let request = HttpRequest::builder()
            .method(Method::OPTIONS)
            .uri("/api/v1/site")
            .header("origin", "https://evil.example.com")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn cross_site_cookies_switch_to_none_and_secure() {
        // 配了跨域来源就说明前端在别的域名上：Cookie 必须 SameSite=None + Secure，
        // 否则浏览器不会把会话发给跨站的接口，症状是「登录成功了，下一页又掉了」
        let (state, _dir) = state_with_origins("https://pixora.vercel.app").await;
        let attrs = crate::http::cookie_attrs(&state.config);
        assert!(attrs.contains("SameSite=None"), "{attrs}");
        assert!(attrs.contains("Secure"), "{attrs}");

        // 同源部署维持原来的 Lax，且不带 Secure（本地 http 调试要能用）
        let (same_origin, _dir) = state_with_origins("").await;
        let attrs = crate::http::cookie_attrs(&same_origin.config);
        assert!(attrs.contains("SameSite=Lax"), "{attrs}");
        assert!(!attrs.contains("Secure"), "{attrs}");
    }

    #[tokio::test]
    async fn same_origin_requests_pass_through_untouched() {
        let (state, _dir) = state_with_origins("https://pixora.vercel.app").await;
        let request = HttpRequest::builder()
            .uri("/api/v1/site")
            .body(Body::empty())
            .unwrap();
        let response = app(state).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }
}
