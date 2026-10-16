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
            // 宽限期是「登记实体 → 建引用」那道时间窗的余量，不是「文件该留多久」。
            // 上传请求里这两步相隔毫秒，两分钟是几十万倍的余量；再长只会让
            // 「删完图还显示占着盘」看起来像没删掉。
            gc_grace_minutes: 2,
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
    let state = state;

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
                // 有「引用已归零、还在宽限期」的，下一轮提前到 60 秒后：
                // 让「刚传完就删」的文件在宽限期一到就被收走，而不是再等整个扫描周期
                let mut wait = Duration::from_secs(300);
                let retention = state.settings.retention_days().await.unwrap_or(0);
                match gc::collect_blobs(&state, grace, batch, retention).await {
                    Ok(outcome) => {
                        if outcome.removed > 0 {
                            tracing::info!(removed = outcome.removed, "回收无引用的物理文件");
                        }
                        if outcome.pending_grace > 0 {
                            wait = Duration::from_secs(60);
                        }
                    }
                    Err(err) => tracing::warn!(error = %err, "Blob 回收失败"),
                }
                // 定时扫描保证「没人催也会回收」；删除类操作会踢一下通知，
                // 让刚失去引用的文件当场就走一轮，而不是等下一个整点窗口。
                // 回收本身仍然是「只在这里删物理文件」这一条不变量。
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = state.wait_gc_kick() => {}
                }
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
                match state.attach_orphan_images().await {
                    Ok(0) => {}
                    Ok(moved) => tracing::info!(moved, "把没有归属的图片收进默认相册"),
                    Err(err) => tracing::warn!(error = %err, "收编无归属图片失败"),
                }
                let emptied = gc::clean_empty_blob_dirs(&state).await;
                if emptied > 0 {
                    tracing::info!(removed = emptied, "清掉数据目录里的空目录");
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }));

    // 5. 缩略图兜底（默认关闭，开启后才有意义）+ 缩略图重算
    handles.push(tokio::spawn({
        let state = state.clone();
        let interval = config.thumb_poll_seconds.max(60);
        async move {
            // 开机把偏小的缩略图一路重算完：客户端早期按 400px 生成的缩略图在宽屏上
            // 就是糊的，让它们自己跟上新尺寸，不必等管理员想起来点「重算缩略图」。
            // 分批做，每批之间让出时间片；跑完就退出这个内层循环，之后交给轮询兜着
            loop {
                match thumbs::refresh_small_thumbs(&state, 40).await {
                    Ok(0) => break,
                    Ok(count) => tracing::info!(count, "把偏小的缩略图重算了一批"),
                    Err(err) => {
                        tracing::warn!(error = %err, "自动重算缩略图失败");
                        break;
                    }
                }
            }
            // 上一轮有没有在重算：决定这一次睡多久
            let mut regen_running = false;
            loop {
                // 重算进行中时用短间隔：默认那一轮是五分钟，管理员点了按钮不该干等
                let wait = if regen_running { 3 } else { interval };
                tokio::time::sleep(Duration::from_secs(wait)).await;
                match thumbs::backfill(&state, 8).await {
                    Ok(0) => {}
                    Ok(count) => tracing::info!(count, "服务端兜底生成了缩略图"),
                    Err(err) => tracing::warn!(error = %err, "缩略图兜底失败"),
                }
                // 管理员点过「重算缩略图」就把游标往前推；没点过时这一步立刻返回 0
                match thumbs::regenerate_batch(&state, 20).await {
                    Ok(0) => {}
                    Ok(count) => {
                        regen_running = true;
                        tracing::info!(count, "重算了一批缩略图");
                    }
                    Err(err) => tracing::warn!(error = %err, "重算缩略图失败"),
                }
                if !regen_running {
                    // 没在重算时也看一眼进度：进程重启后要接着把没跑完的补完
                    regen_running = state
                        .thumb_rebuild_progress()
                        .await
                        .map(|progress| progress.running)
                        .unwrap_or(false);
                }
                // 顺带把「缩略图偏小」的收一批（旧客户端传上来的那种）。
                // 有活可干就按短间隔继续，别让一批老图等上几个小时
                match thumbs::refresh_small_thumbs(&state, 20).await {
                    Ok(0) => {}
                    Ok(count) => {
                        regen_running = true;
                        tracing::info!(count, "重算了偏小的缩略图");
                    }
                    Err(err) => tracing::warn!(error = %err, "自动重算缩略图失败"),
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
