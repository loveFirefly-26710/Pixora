//! 审计日志的写入封装。
//!
//! 设计稿要求「谁在什么时候删了什么、管理员跨用户访问」都要留档，而且这些
//! 写入点散落在各服务里（登录失败、命限流、上传失败、处置账号、跨用户访问）。
//! 所以这里给一个足够省的接口：一行调用就够，不打断主流程。
//!
//! **审计写入失败不能让业务失败**：日志是给事后追责用的，不是业务前置条件。
//! 失败只记警告，并且把原因写进 tracing 让运维能发现。

use pixora_repo::repo::audit::{AuditEntry, AuditFilter};
use pixora_repo::model::AuditLog;
use pixora_repo::Page;

use crate::error::Result;
use crate::AppState;

/// 动作名常量。界面按这些名字做筛选与统计，拼错就等于漏记。
pub mod actions {
    /// 从回收站恢复（图片或相册），detail 里说明是哪一种
    pub const RECYCLE_RESTORE: &str = "recycle_restore";
    /// 彻底删除（清空回收站、或单个彻底删除）
    pub const RECYCLE_PURGE: &str = "recycle_purge";
    pub const LOGIN_SUCCESS: &str = "login_success";
    pub const LOGIN_FAIL: &str = "login_fail";
    pub const LOGIN_BLOCKED: &str = "login_blocked";
    pub const LOGOUT: &str = "logout";
    pub const REGISTER: &str = "register";
    pub const PASSWORD_RESET: &str = "password_reset";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const UPLOAD_FAIL: &str = "upload_fail";
    pub const UPLOAD: &str = "upload";
    pub const DELETE_IMAGE: &str = "delete_image";
    pub const ADMIN_VIEW_USER: &str = "admin_view_user";
    pub const ACCOUNT_SUSPEND: &str = "account_suspend";
    pub const ACCOUNT_BAN: &str = "account_ban";
    pub const ACCOUNT_RESTORE: &str = "account_restore";
    pub const ACCOUNT_DELETE: &str = "account_delete";
    pub const QUOTA_CHANGE: &str = "quota_change";
    pub const SETTINGS_CHANGE: &str = "settings_change";
    pub const STORAGE_CHANGE: &str = "storage_change";
    pub const TERMS_PUBLISH: &str = "terms_publish";
    pub const IP_RULE_CHANGE: &str = "ip_rule_change";
    pub const TEMP_BAN: &str = "temp_ban";
}

impl AppState {
    /// 记一条审计事件。失败只警告，不影响调用方。
    pub async fn audit(&self, entry: AuditEntry) {
        if let Err(err) = self.repo.insert_audit(&entry).await {
            tracing::warn!(action = %entry.action, error = %err, "审计日志写入失败");
        }
    }

    /// 便捷封装：只有动作 + 操作人。
    pub async fn audit_action(&self, action: &str, actor_id: Option<i64>, ip: Option<&str>) {
        self.audit(
            AuditEntry::new(action)
                .actor(actor_id)
                .ip(ip.map(str::to_string)),
        )
        .await;
    }

    /// 便捷封装：带目标与细节。
    pub async fn audit_detail(
        &self,
        action: &str,
        actor_id: Option<i64>,
        target: impl Into<String>,
        detail: serde_json::Value,
        ip: Option<&str>,
    ) {
        self.audit(
            AuditEntry::new(action)
                .actor(actor_id)
                .target(target)
                .detail(detail)
                .ip(ip.map(str::to_string)),
        )
        .await;
    }

    /// 管理员跨用户访问要单独记一条——这是设计稿点名的合规要求。
    pub async fn audit_cross_user(&self, admin_id: i64, user_uid: &str, page: &str, ip: Option<&str>) {
        self.audit(
            AuditEntry::new(actions::ADMIN_VIEW_USER)
                .actor(Some(admin_id))
                .target(user_uid)
                .detail(serde_json::json!({ "page": page }))
                .ip(ip.map(str::to_string)),
        )
        .await;
    }

    pub async fn list_audit(
        &self,
        filter: &AuditFilter,
        page: pixora_repo::PageRequest,
    ) -> Result<Page<AuditLog>> {
        Ok(self.repo.list_audit(filter, page).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_are_stable() {
        // 界面与统计都按这些字符串走，改名等于丢历史数据
        assert_eq!(actions::RATE_LIMITED, "rate_limited");
        assert_eq!(actions::LOGIN_FAIL, "login_fail");
        assert_eq!(actions::ADMIN_VIEW_USER, "admin_view_user");
        assert_eq!(actions::DELETE_IMAGE, "delete_image");
    }
}
