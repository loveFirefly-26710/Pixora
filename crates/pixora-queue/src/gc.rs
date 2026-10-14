//! Blob 回收与对账。
//!
//! **删除物理文件的唯一入口就在这里**。设计稿把这条写进了代码评审清单：
//! 任何绕过引用计数直接删文件的代码路径都是隐患，因为同一份文件可能被多个
//! 用户引用——用户 A 删掉自己的图，用户 B 的同一张图必须毫发无损。
//!
//! 回收的三步顺序不能变：
//!   1. 先确认 `ref_count = 0` 且已过宽限期（避开「登记实体 → 建引用」的时间窗）；
//!   2. 删物理文件；
//!   3. 删数据库行。
//! 反过来的话，删了行而物理文件还在，就再也没人知道那个文件是谁的了。

use std::sync::Arc;

use chrono::Utc;

use pixora_core::AppState;

/// 回收无引用的物理文件，返回处理数量。
pub async fn collect_blobs(state: &Arc<AppState>, grace_minutes: i64, batch: i64) -> pixora_core::Result<u64> {
    let older_than = Utc::now() - chrono::Duration::minutes(grace_minutes.max(5));
    let candidates = state.repo.list_gc_candidates(older_than, batch).await?;
    let mut removed = 0u64;

    for blob in candidates {
        // 再确认一次：并发上传可能在这一瞬间给它加上了引用
        let Some(fresh) = state.repo.find_blob_by_id(blob.id).await? else {
            continue;
        };
        if fresh.ref_count > 0 {
            continue;
        }

        match state
            .storage
            .delete_object(&fresh.storage_driver, fresh.backend_id, &fresh.storage_key)
            .await
        {
            Ok(()) => {}
            Err(err) => {
                // 物理删除失败就留着这行，下一轮再试。
                // 先删行会让物理文件永远留在存储后端上，那比多留一行糟糕得多。
                tracing::warn!(
                    blob_id = fresh.id,
                    driver = %fresh.storage_driver,
                    error = %err,
                    "物理文件删除失败，保留实体行等待下一轮"
                );
                continue;
            }
        }

        // 只删引用计数仍为 0 的行，防止「删物理文件」与「新建引用」之间被插队
        let affected = state.repo.delete_blob_if_unreferenced(fresh.id).await?;
        if affected > 0 {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 对账：重算用量与相册计数。修正是幂等的，跑多少次结果一样。
pub async fn reconcile(state: &Arc<AppState>) -> pixora_core::Result<u64> {
    let users = state.repo.reconcile_all_used_bytes().await?;
    let albums = state.repo.recount_all_album_images().await?;
    if users > 0 || albums > 0 {
        tracing::info!(users, albums, "对账修正了用量与相册计数");
    }
    Ok(users + albums)
}

/// 清理过期数据。都是「有明确保质期」的行，删了不影响业务。
pub async fn purge_expired(state: &Arc<AppState>) -> pixora_core::Result<()> {
    let now = Utc::now();

    // 过期会话：已吊销，或 90 天没活动过
    let sessions = state
        .repo
        .purge_stale_sessions(now - chrono::Duration::days(90))
        .await?;

    // 过期验证码
    let codes = state
        .repo
        .purge_expired_verifications(now - chrono::Duration::days(1))
        .await?;

    // 过期 IP 规则（自动临时封禁到期的那些）
    let ip_rules = state.ip_guard.purge_expired().await.unwrap_or(0);

    // 滑动窗口计数：留 7 天足够管理界面看趋势
    let counters = state
        .limits
        .purge(now - chrono::Duration::days(7))
        .await
        .unwrap_or(0);

    // 旧的排队任务账本与已发出的邮件
    let jobs = state
        .repo
        .purge_jobs_before(now - chrono::Duration::days(7))
        .await?;
    let mails = state
        .repo
        .purge_sent_mail(now - chrono::Duration::days(30))
        .await?;
    // 审计日志保留 180 天：足够追溯，也不至于无限增长
    let audits = state
        .repo
        .purge_audit_before(now - chrono::Duration::days(180))
        .await?;

    // 没有任何引用的空标签
    let tags = state.repo.purge_unused_tags().await.unwrap_or(0);

    if sessions + codes + ip_rules + counters + jobs + mails + audits + tags > 0 {
        tracing::info!(
            sessions,
            codes,
            ip_rules,
            counters,
            jobs,
            mails,
            audits,
            tags,
            "清理过期数据完成"
        );
    }
    Ok(())
}

/// 回收「卡在 running」的上传任务。
///
/// 进程被 kill 时正在处理的任务会留在 running 状态里，不处理的话它们
/// 会一直占着队列深度，最终把新上传全部挡在门外。
pub async fn requeue_stale_jobs(state: &Arc<AppState>) -> pixora_core::Result<u64> {
    let older_than = Utc::now() - chrono::Duration::minutes(30);
    Ok(state.repo.requeue_stale(older_than).await?)
}
