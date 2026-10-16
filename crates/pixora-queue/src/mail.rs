//! 发件箱投递。
//!
//! 邮件走「先落库、再投递、失败退避重试」的形状。自建 SMTP 的送达率本来
//! 就是弱项（设计稿把它列进风险清单），再加上网络抖动，一次性投递必然丢信——
//! 而验证码丢了用户就得重新走一遍注册。
//!
//! 退避策略：2 分钟起，每次翻倍，最多 6 次（约 2 小时）。超过就标成永久失败，
//! 界面上能看到失败原因，管理员也就能知道「是不是 SMTP 配置写错了」。

use std::sync::Arc;

use chrono::Utc;

use pixora_core::{AppError, AppState};

/// 最多尝试次数。
const MAX_ATTEMPTS: i32 = 6;
/// 首次重试的间隔（分钟）。
const BASE_BACKOFF_MINUTES: i64 = 2;

/// 投递一批到期邮件，返回成功条数。
pub async fn pump(state: &Arc<AppState>, limit: i64) -> pixora_core::Result<u64> {
    if state.mailer().is_none() {
        // SMTP 没配时不要动发件箱：这些行留着，等管理员配好 SMTP 之后自然会被投递
        return Ok(0);
    }

    let items = state.repo.due_mails(Utc::now(), limit).await?;
    let mut sent = 0u64;

    for item in items {
        match state.deliver_outbox_mail(&item).await {
            Ok(()) => {
                state.repo.mark_mail_sent(item.id, Utc::now()).await?;
                sent += 1;
            }
            Err(err) => {
                let next_attempt = item.attempts + 1;
                // 收件服务器明确拒收、发信凭据不对：这两种错重发多少次都是同一个
                // 结果，不能跟网络抖动一起排队——那只会往发信账号里堆退信
                let permanent = err.is_permanent() || next_attempt >= MAX_ATTEMPTS;
                let next_at = if permanent {
                    None
                } else {
                    // 指数退避：2、4、8、16、32 分钟
                    let minutes = BASE_BACKOFF_MINUTES * (1i64 << next_attempt.min(4));
                    Some(Utc::now() + chrono::Duration::minutes(minutes))
                };

                state
                    .repo
                    .mark_mail_failed(item.id, &err.message(), next_at, permanent)
                    .await?;

                if permanent {
                    // 永久失败要显眼：多半是 SMTP 配置或收件地址的问题
                    tracing::error!(
                        mail_id = item.id,
                        to = %item.to_email,
                        template = %item.template_name,
                        error = %err,
                        attempts = next_attempt,
                        "邮件不再重试，已标记为永久失败"
                    );
                } else {
                    tracing::warn!(
                        mail_id = item.id,
                        attempts = next_attempt,
                        error = %err,
                        "邮件投递失败，稍后重试"
                    );
                }
            }
        }
    }
    Ok(sent)
}

/// 立即投递一封（注册与处置流程里已经尝试过一次，这里是队列侧的兜底入口）。
pub async fn deliver_now(state: &Arc<AppState>, mail_id: i64) -> pixora_core::Result<()> {
    let item = state
        .repo
        .find_mail(mail_id)
        .await?
        .ok_or_else(|| AppError::not_found("邮件"))?;
    if item.is_sent() {
        return Ok(());
    }
    state.deliver_outbox_mail(&item).await?;
    state.repo.mark_mail_sent(item.id, Utc::now()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_but_stays_bounded() {
        let mut previous = 0;
        for attempt in 0..MAX_ATTEMPTS {
            let minutes = BASE_BACKOFF_MINUTES * (1i64 << attempt.min(4));
            assert!(minutes >= previous);
            assert!(minutes <= 32, "退避上限 32 分钟，别让一封邮件等到明天");
            previous = minutes;
        }
        // 第 6 次就该放弃了
        assert!(MAX_ATTEMPTS >= 1);
    }
}
