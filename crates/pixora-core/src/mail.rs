//! 邮件投递的对外说明。
//!
//! 发件箱本身在 `pixora-repo`（落库）+ `pixora-queue`（重试），这里只放
//! 「这一次请求里邮件到底出去没有」这一个结论——管理员点「发重置邮件」
//! 之后要看到的正是它，而不是一句无条件的「已发送」。

/// 一次投递的结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MailDelivery {
    /// 这次请求里投成功了。false 时邮件仍在发件箱里排队，不是丢了
    pub delivered: bool,
    /// 没投成功的原因，给管理员看的人话
    pub detail: Option<String>,
}

impl MailDelivery {
    /// 界面上一句话：投出去了，或者说明为什么还在排队。
    pub fn describe(&self) -> String {
        match (self.delivered, self.detail.as_deref()) {
            (true, _) => "邮件已投递".to_string(),
            (false, Some(detail)) => detail.to_string(),
            (false, None) => "邮件已留在发件箱，稍后自动重试".to_string(),
        }
    }
}
