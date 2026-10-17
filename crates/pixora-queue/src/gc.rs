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

/// 一轮回收的结果。
pub struct GcOutcome {
    /// 真正清掉的实体数
    pub removed: u64,
    /// 引用已归零、但还在宽限期里的实体数：调用方据此安排下一轮早点来
    pub pending_grace: i64,
}

/// 回收无引用的物理文件。
///
/// `retention_days` 是回收站的保留天数：只要还有没到期的墓碑挂着这个实体，
/// 文件就留着（恢复还要用它）。到期的墓碑先摘掉——外键挡着实体行不让删。
/// 回收站关着（0 天）时所有墓碑都算到期，等于没有回收站。
pub async fn collect_blobs(
    state: &Arc<AppState>,
    grace_minutes: i64,
    batch: i64,
    retention_days: i64,
) -> pixora_core::Result<GcOutcome> {
    // 下限两分钟：这段宽限期保护的是「登记实体 → 建引用」那一瞬间（毫秒级），
    // 两分钟已经是几十万倍的余量。再长只会让「删完还显示占着盘」看起来像没删掉
    let grace = chrono::Duration::minutes(grace_minutes.max(2));
    let older_than = Utc::now() - grace;
    let retention_cutoff = Utc::now() - chrono::Duration::days(retention_days.max(0));
    let candidates = state.repo.list_gc_candidates(older_than, batch).await?;
    let mut removed = 0u64;

    for blob in candidates {
        // 与上传的「查重 → 落盘 → 建引用」串行化。没有这把锁时，复查完
        // ref_count 到真正删文件之间，新上传仍可能挂到这一行上。
        let _blob_guard = state.blob_lock(&blob.sha256).lock().await;
        // 再确认一次：并发上传可能在这一瞬间给它加上了引用
        let Some(fresh) = state.repo.find_blob_by_id(blob.id).await? else {
            continue;
        };
        if fresh.ref_count > 0 {
            continue;
        }

        // 回收站里有它、且还没到期：这一步什么都不做，等保留期过了再来
        if retention_days > 0
            && state
                .repo
                .count_tombstones_after(fresh.id, retention_cutoff)
                .await
                .unwrap_or(0)
                > 0
        {
            continue;
        }

        // 到期的墓碑先摘掉：外键挡着实体行不让删，不清的话物理文件回收不掉。
        // 单条出问题只跳过这一条——原来的写法会让一个「删不掉」的实体
        // 拖垮整轮回收，其它几十个孤儿文件跟着一起走不掉。
        if let Err(err) = state
            .repo
            .purge_tombstones_for_blob_before(fresh.id, retention_cutoff)
            .await
        {
            tracing::warn!(blob_id = fresh.id, error = %err, "清理过期墓碑失败，跳过这个实体");
            continue;
        }

        match state
            .storage
            .delete_object(&fresh.storage_driver, fresh.backend_id, &fresh.storage_key)
            .await
        {
            Ok(()) => {}
            Err(err) => {
                // 驱动实例装配不出来（管理员把后端配置行删了）时走孤儿回收：
                // 本地与数据库内这两种的落点与配置无关，能照常收掉；远端对象
                // 只能在对象存储侧解决，这里已经记了警告，行留着不再反复重试。
                match state
                    .storage
                    .delete_orphan_object(&fresh.storage_driver, &fresh.storage_key)
                    .await
                {
                    Ok(true) => tracing::info!(
                        blob_id = fresh.id,
                        driver = %fresh.storage_driver,
                        "后端配置已删除，按孤儿对象回收"
                    ),
                    Ok(false) => {
                        tracing::warn!(
                            blob_id = fresh.id,
                            driver = %fresh.storage_driver,
                            error = %err,
                            "物理文件删除失败，保留实体行等待下一轮"
                        );
                        continue;
                    }
                    Err(orphan_err) => {
                        tracing::warn!(
                            blob_id = fresh.id,
                            driver = %fresh.storage_driver,
                            error = %orphan_err,
                            "孤儿对象回收失败，保留实体行等待下一轮"
                        );
                        continue;
                    }
                }
            }
        }

        // 只删引用计数仍为 0 的行，防止「删物理文件」与「新建引用」之间被插队
        match state.repo.delete_blob_if_unreferenced(fresh.id).await {
            Ok(affected) if affected > 0 => removed += 1,
            // 0 行说明这一瞬间有人新建了引用：文件已经删了，只能下一轮再看
            Ok(_) => tracing::warn!(blob_id = fresh.id, "实体行删除时已被重新引用"),
            Err(err) => {
                tracing::warn!(blob_id = fresh.id, error = %err, "实体行删除失败，留到下一轮");
            }
        }
    }

    // 宽限期按「登记时间」算，这里换个口径数一下：引用已归零但还没到点的，
    // 下一轮要早点来（宽限期一到就该收走，不再等一个完整周期）
    let pending_grace = state
        .repo
        .count_unreferenced_newer_than(older_than)
        .await
        .unwrap_or(0);
    Ok(GcOutcome {
        removed,
        pending_grace,
    })
}

/// 对账：重算用量与相册计数。修正是幂等的，跑多少次结果一样。
/// 清掉 CAS 目录树里的空目录。
///
/// 删除实体时会顺手收掉它所在的那层目录（`blobs/xx/`），但早期版本留下的空壳
/// 没人管，数据目录里会攒下一片空目录。这里按第二轮扫描一次，只删空的、
/// 不递归、删不掉就算了——纯卫生问题，不值得为它报错。
pub async fn clean_empty_blob_dirs(state: &Arc<AppState>) -> u64 {
    let root = state.config.data_dir.join("blobs");
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return 0;
    };
    let mut removed = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !entry
            .metadata()
            .await
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        // remove_dir 只对空目录成功，非空的会报错——正是我们要的语义
        if tokio::fs::remove_dir(&path).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 清掉旧版镜像树的残留：`{data_dir}/users/users/…`。
///
/// 早期版本的镜像路径解析多拼了一层 `users`，于是硬链接落进了 `users/users/`。
/// 修好路径之后那批目录就成了没人认领的残骸，而且删图/删相册都不会再碰它们。
/// 里面的文件都是硬链接，删链接不动 CAS 里的正本，所以整棵清掉是安全的。
/// 清过一次之后这就是个 no-op（`users/users` 不可能是正常路径：uid 是数字，
/// 相册 slug 夹在 uid 后面）。
async fn clean_legacy_view_tree(state: &Arc<AppState>) {
    let legacy = state.config.data_dir.join("users").join("users");
    if !legacy.exists() {
        return;
    }
    match tokio::fs::remove_dir_all(&legacy).await {
        Ok(()) => tracing::info!(path = %legacy.display(), "清掉了旧版遗留的镜像目录"),
        Err(err) => tracing::warn!(error = %err, path = %legacy.display(), "清理旧版镜像目录失败"),
    }
}

pub async fn reconcile(state: &Arc<AppState>) -> pixora_core::Result<u64> {
    clean_legacy_view_tree(state).await;
    let users = state.repo.reconcile_all_used_bytes().await?;
    let albums = state.repo.recount_all_album_images().await?;
    // 引用计数也要对账。删除账号走的是外键级联，少了「递减计数」那一步，
    // 计数虚高的实体永远等不到归零，物理文件也就永远不被回收。
    let blobs = state.repo.recompute_blob_ref_counts().await?;
    if users > 0 || albums > 0 || blobs > 0 {
        tracing::info!(users, albums, blobs, "对账修正了用量、相册计数与引用计数");
    }
    Ok(users + albums + blobs)
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

    // 回收站：过了保留期的墓碑在这里清掉（物理文件交给同一轮的 collect_blobs）。
    // 单靠 collect_blobs 不够——一张图的实体可能还被别的活图引用着，
    // 那种情况下它不会进候选，墓碑却早该清了。
    let retention_days = state.settings.retention_days().await.unwrap_or(0);
    let mut tombstones = 0u64;
    if retention_days > 0 {
        let expired = state
            .repo
            .purge_expired_tombstones(now - chrono::Duration::days(retention_days), 500)
            .await?;
        tombstones = expired.len() as u64;
        // 保留期到了就是彻底销毁：文件当场收掉，不等下一轮 collect_blobs
        for target in expired {
            let _ = state.reclaim_blob_now(target.blob_id).await;
            if let Some(thumb_id) = target.thumb_blob_id {
                let _ = state.reclaim_blob_now(thumb_id).await;
            }
        }
    }

    // 没有任何引用的空标签
    let tags = state.repo.purge_unused_tags().await.unwrap_or(0);

    if sessions + codes + ip_rules + counters + jobs + mails + audits + tags + tombstones > 0 {
        tracing::info!(
            sessions,
            codes,
            ip_rules,
            counters,
            jobs,
            mails,
            audits,
            tags,
            tombstones,
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
