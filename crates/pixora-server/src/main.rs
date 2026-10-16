//! Pixora 单二进制入口。
//!
//! 启动顺序：日志 → 配置 → 数据库（连接 + 迁移）→ 存储驱动装配 → 后台任务 → 监听。
//! 输出一行启动摘要把关键事实写清楚（数据目录、数据库、驱动、监听地址、主密钥来源），
//! 排障时第一眼看的就是它。
//!
//! 用 `mimalloc` 换掉系统分配器：图床频繁申请释放缓冲区，glibc 的 malloc
//! 在长时间运行后容易碎片化，换分配器能把常驻内存再压一截（设计稿的建议）。

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod assets;
mod cors;
mod direct;
mod http;
mod routes;

use std::sync::Arc;

use pixora_core::{AppState, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // 容器 HEALTHCHECK 用的轻量探活：连一次 /healthz，成功就退 0。
    // 不建数据库连接池，也不装日志框架，开销可忽略。
    if std::env::args().any(|arg| arg == "--health-probe") {
        std::process::exit(health_probe().await);
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            // 配置错就别装日志框架了，直接把原因打到 stderr
            eprintln!("Pixora 启动失败：{err}");
            std::process::exit(2);
        }
    };

    init_tracing(&config.log_level);

    if let Err(err) = run(config).await {
        tracing::error!(error = %err, "服务异常退出");
        std::process::exit(1);
    }
}

async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let bind = config.bind;
    let data_dir = config.data_dir.clone();
    let db_driver = config.db_driver;
    let key_source = config.key_source.describe();
    let weak_key = config.key_source.is_weak_setup();
    let base_url_missing = config.base_url.is_none();
    let web_dir = config.web_dir.clone();

    // 跨域与跨站 Cookie：配了才提，没配时保持沉默（自托管默认不需要）
    let cors_origins = config.cors_origins.clone();
    let samesite = config.samesite;

    let state = AppState::bootstrap(config).await?;

    // 后台任务：API 与 Worker 同进程，不引入 Redis 或独立 worker 容器
    let tasks = pixora_queue::spawn(state.clone(), pixora_queue::QueueConfig::default());

    let app = app::build(state.clone());

    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;

    let asset_note = match web_dir.as_ref() {
        Some(dir) => format!("读盘：{}", dir.display()),
        None if assets::embedded_available() => "已嵌入二进制".to_string(),
        None => "缺失（前端还没构建？）".to_string(),
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %local_addr,
        data_dir = %data_dir.display(),
        db = db_driver.dialect().as_str(),
        storage_default = %state
            .storage
            .registry()
            .default_key()
            .unwrap_or_else(|| "未装配".into()),
        master_key = %key_source,
        web = %asset_note,
        "Pixora 已启动"
    );

    if base_url_missing {
        tracing::warn!(
            "未设置 PIXORA_BASE_URL：直链会按请求推断；反向代理场景下必须显式设置，否则直链地址是错的"
        );
    }
    if !cors_origins.is_empty() {
        tracing::info!(
            origins = %cors_origins.join(", "),
            samesite = samesite.as_str(),
            "已放行跨域前端：接口会按来源回 Access-Control-* 头，Cookie 用 SameSite={}",
            samesite.as_str()
        );
        if base_url_missing {
            tracing::warn!(
                "配了跨域来源但没配 PIXORA_BASE_URL：直接链与站点资产地址会指向内网，务必补上"
            );
        }
    }
    if weak_key {
        tracing::warn!(
            "主密钥是自动生成的（{}）：单机自托管够用，生产环境建议改用 PIXORA_MASTER_KEY 注入并纳入备份",
            key_source
        );
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // 停掉后台任务：给它们一点时间把手上的活收尾
    for task in tasks {
        task.abort();
    }
    state.shutdown().await;
    tracing::info!("Pixora 已停止");
    Ok(())
}

/// 探活：读 `PIXORA_BIND` 拼出本地探测地址，请求 `/healthz`。
async fn health_probe() -> i32 {
    let bind = std::env::var("PIXORA_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let (host, port) = match bind.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.to_string()),
        None => ("127.0.0.1".to_string(), "8080".to_string()),
    };
    // 监听 0.0.0.0 / [::] 时探本地回环
    let host = if host.is_empty() || host == "0.0.0.0" || host == "[::]" || host == "::" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    let url = format!("http://{host}:{port}/healthz");

    match tokio::time::timeout(std::time::Duration::from_secs(3), reqwest::get(&url)).await {
        Ok(Ok(response)) if response.status().is_success() => 0,
        _ => 1,
    }
}

/// 等 Ctrl+C 或 SIGTERM。容器里 `docker stop` 发的是后者。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("无法监听 Ctrl+C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法监听 SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到中断信号，正在停止"),
        _ = terminate => tracing::info!("收到终止信号，正在停止"),
    }
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

/// 让集成测试与外部工具复用路由装配。
#[allow(dead_code)]
pub fn router(state: Arc<AppState>) -> axum::Router {
    app::build(state)
}
