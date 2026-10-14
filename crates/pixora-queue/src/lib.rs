//! Pixora 的进程内队列与后台任务。
//!
//! 设计稿把「API 与 Worker 同进程」当作一条硬性取舍：后台任务跑在进程内的
//! 有界任务池里，不引入 Redis 或独立 worker 容器——少一个进程就少一份内存
//! 和一套运维。这里就是那批任务。
//!
//! 五类任务：
//!
//!   1. **启动清理**：回收上次崩溃留下的临时文件与卡在 running 的上传任务。
//!   2. **邮件重试**：验证码、暂停通知这类邮件丢了用户就得再等一次，
//!      发件箱里的行按 `next_attempt_at` 退避重试。
//!   3. **Blob 垃圾回收**：引用计数归零且过了宽限期的实体，先删物理文件再删行。
//!      宽限期是为了避开「先登记实体、再建引用」之间的时间窗。
//!   4. **对账与清理**：重算 `used_bytes` 与相册计数修正漂移，顺带清过期会话、
//!      旧审计、过期验证码、过期 IP 规则与滑动窗口计数。
//!   5. **缩略图兜底**：默认关闭。开启后为没有缩略图的图片异步补生成
//!      （客户端上传一般都带缩略图，只有 API 上传才会缺）。
//!
//! 所有任务都是「跑一轮就睡」的形状，不持有长事务，重启即恢复。

pub mod gc;
pub mod mail;
pub mod thumbs;

use std::sync::Arc;
use std::time::Duration;

use pixora_core::AppState;

/// 队列参数。
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Blob 回收的宽限期（分钟）
    pub gc_grace_minutes: i64,
    /// 单轮 GC 最多处理多少个实体
    pub gc_batch: i64,
    /// 邮件重试的轮询间隔（秒）
    pub mail_poll_seconds: u64,
    /// 对账间隔（秒）
    pub reconcile_seconds: u64,
    /// 缩略图兜底轮询间隔（秒）
    pub thumb_poll_seconds: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            // 宽限期给足：上传是先写实体再建引用的，太短会误删正在上传的文件
            gc_grace_minutes: 60,
            gc_batch: 200,
            mail_poll_seconds: 30,
            reconcile_seconds: 3600,
            thumb_poll_seconds: 300,
        }
    }
}

/// 启动后台任务。返回的任务句柄在进程退出时被丢弃即可。
pub fn spawn(state: Arc<AppState>, config: QueueConfig) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    // 1. 启动清理
    handles.push(tokio::spawn({
        let state = state.clone();
        async move {
            let removed =
                pixora_core::uploads::purge_stale_temp_files(&state.config.data_dir, 3600).await;
            if removed > 0 {
                tracing::info!(removed, "清理残留的临时上传文件");
            }
            match gc::requeue_stale_jobs(&state).await {
                Ok(0) => {}
                Ok(count) => tracing::warn!(count, "回收了上次启动卡住的上传任务"),
                Err(err) => tracing::warn!(error = %err, "回收卡住任务失败"),
            }
        }
    }));

    // 2. 邮件重试
    handles.push(tokio::spawn({
        let state = state.clone();
        let interval = config.mail_poll_seconds.max(5);
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                match mail::pump(&state, 20).await {
                    Ok(0) => {}
                    Ok(sent) => tracing::info!(sent, "发件箱投递完成"),
                    Err(err) => tracing::warn!(error = %err, "发件箱轮询失败"),
                }
            }
        }
    }));

    // 3. Blob 回收
    handles.push(tokio::spawn({
        let state = state.clone();
        let grace = config.gc_grace_minutes;
        let batch = config.gc_batch;
        async move {
            // 启动后先等一会儿再跑第一轮，避开启动瞬间的请求高峰
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                match gc::collect_blobs(&state, grace, batch).await {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "回收无引用的物理文件"),
                    Err(err) => tracing::warn!(error = %err, "Blob 回收失败"),
                }
                tokio::time::sleep(Duration::from_secs(1800)).await;
            }
        }
    }));

    // 4. 对账与过期清理
    handles.push(tokio::spawn({
        let state = state.clone();
        let interval = config.reconcile_seconds.max(300);
        async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            loop {
                if let Err(err) = gc::reconcile(&state).await {
                    tracing::warn!(error = %err, "对账任务失败");
                }
                if let Err(err) = gc::purge_expired(&state).await {
                    tracing::warn!(error = %err, "过期数据清理失败");
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }));

    // 5. 缩略图兜底（默认关闭，开启后才有意义）
    handles.push(tokio::spawn({
        let state = state.clone();
        let interval = config.thumb_poll_seconds.max(60);
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                match thumbs::backfill(&state, 8).await {
                    Ok(0) => {}
                    Ok(count) => tracing::info!(count, "服务端兜底生成了缩略图"),
                    Err(err) => tracing::warn!(error = %err, "缩略图兜底失败"),
                }
            }
        }
    }));

    handles
}

/// 手动跑一轮对账（管理后台的按钮会调）。
pub async fn reconcile_now(state: &Arc<AppState>) -> Result<u64, pixora_core::AppError> {
    gc::reconcile(state).await
}
