//! 账号历史（后台「系统日志」里那一栏）。
//!
//! 一个账号的一生：什么时候注册的、换过哪几次邮箱、被谁暂停过、什么时候注销的，
//! 以及登录与访问记录。管理员要能在**账号已经注销之后**把这些翻出来看，所以
//! 数据取自 `audit_logs`——它以 UID 为记录目标，账号行删掉了流水还在；
//! 而 `user_actions` 挂在 `users.id` 上，随账号级联删除，只能看活人。
//!
//! 每条记录还要回答「这是谁做的」：本人、管理员还是系统。当年写入的 detail 里
//! 记了 `via` 的就按它来；没记的（老流水）按操作人 id 反查——反查不到账号时
//! 登录这类只可能是本人发起的动作仍然算本人，其余如实标成「已注销的操作人」。

use pixora_repo::model::AuditLog;
use pixora_repo::repo::audit::AuditEntry;
use pixora_repo::{Page, PageRequest};

use crate::audit::actions;
use crate::error::Result;
use crate::AppState;

/// 系统日志默认覆盖的动作（账户信息 + 系统设置）。
///
/// 列表默认只看这些：整张审计流水混着上传、删图、限流命中，翻系统日志时
/// 那些都是噪声。想看全部时把 `action` 传成空以外的其它值即可。
pub const ACCOUNT_ACTIONS: &[&str] = &[
    actions::REGISTER,
    actions::ACCOUNT_UPDATE,
    actions::ACCOUNT_EMAIL_CHANGE,
    actions::ACCOUNT_DELETE,
    actions::ACCOUNT_SUSPEND,
    actions::ACCOUNT_BAN,
    actions::ACCOUNT_RESTORE,
    actions::TEMP_BAN,
    actions::PASSWORD_RESET,
    actions::QUOTA_CHANGE,
    actions::LOGIN_SUCCESS,
    actions::LOGIN_FAIL,
    actions::LOGIN_BLOCKED,
    actions::LOGOUT,
    // 系统配置变更没有账号目标，但同样属于「系统日志」要回答的问题
    actions::SETTINGS_CHANGE,
    actions::IP_RULE_CHANGE,
    actions::STORAGE_CHANGE,
    actions::TERMS_PUBLISH,
];

/// 系统级配置动作：`target` 是配置标识（settings / cidr / backend:id），
/// 不是用户 UID，展示时不能当成账号去反查。
pub const SYSTEM_CONFIG_ACTIONS: &[&str] = &[
    actions::SETTINGS_CHANGE,
    actions::IP_RULE_CHANGE,
    actions::STORAGE_CHANGE,
    actions::TERMS_PUBLISH,
];

/// 只可能是本人发起的动作：反查不到账号时也按「本人」记。
const SELF_ONLY_ACTIONS: &[&str] = &[
    actions::LOGIN_SUCCESS,
    actions::LOGIN_FAIL,
    actions::LOGIN_BLOCKED,
    actions::LOGOUT,
];

/// 动作名 → 界面上的中文标签。不认识的动作原样显示。
pub fn account_action_label(action: &str) -> &str {
    match action {
        actions::REGISTER => "注册账号",
        actions::ACCOUNT_UPDATE => "修改账户信息",
        actions::ACCOUNT_EMAIL_CHANGE => "更换邮箱",
        actions::ACCOUNT_DELETE => "注销账号",
        actions::ACCOUNT_SUSPEND => "暂停使用",
        actions::ACCOUNT_BAN => "封禁账号",
        actions::ACCOUNT_RESTORE => "恢复正常",
        actions::TEMP_BAN => "临时封禁",
        actions::PASSWORD_RESET => "重置密码",
        actions::QUOTA_CHANGE => "调整账号额度",
        actions::LOGIN_SUCCESS => "登录成功",
        actions::LOGIN_FAIL => "登录失败",
        actions::LOGIN_BLOCKED => "登录被拦",
        actions::LOGOUT => "退出登录",
        actions::SETTINGS_CHANGE => "修改系统配置",
        actions::IP_RULE_CHANGE => "修改 IP 黑白名单",
        actions::STORAGE_CHANGE => "修改存储配置",
        actions::TERMS_PUBLISH => "发布服务条款",
        other => other,
    }
}

/// 日志分类：用户管理相关动作归「账户信息」，其余后台配置归「系统设置」。
pub fn action_category(action: &str) -> &'static str {
    if SYSTEM_CONFIG_ACTIONS.contains(&action) {
        "system"
    } else {
        "account"
    }
}

pub fn category_label(category: &str) -> &'static str {
    if category == "system" {
        "系统设置"
    } else {
        "账户信息"
    }
}

/// 一条账号历史。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountLogRow {
    pub id: i64,
    pub action: String,
    pub action_label: String,
    /// 日志分类：`account`（账户信息）/ `system`（系统设置）
    pub category: String,
    pub category_label: String,
    /// 这条记录讲的是哪个账号（系统配置动作没有账号目标，为 None）
    pub target_uid: Option<String>,
    /// 审计里的原始目标：账号 UID，或配置标识（settings / cidr / backend:id）
    pub target: Option<String>,
    /// 记录里能读到的邮箱：注册、换邮箱、注销都带着它，
    /// 登录这类没有（那时只在 IP 上留了痕）
    pub email: Option<String>,
    /// 谁做的：`self`（本人）/ `admin` / `system` / `unknown`（操作人账号已注销）
    pub actor_kind: String,
    /// 界面直接显示的一句话
    pub actor_label: String,
    pub actor_id: Option<i64>,
    pub ip: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: String,
}

/// 账号历史的筛选条件。
#[derive(Debug, Clone, Default)]
pub struct AccountLogFilter {
    /// 只看某个动作；空表示用默认的那一组 [`ACCOUNT_ACTIONS`]
    pub action: Option<String>,
    /// 只看某个账号（UID 精确匹配）
    pub uid: Option<String>,
    /// 关键词：UID、详情、IP 三处任意命中
    pub query: Option<String>,
}

impl AppState {
    /// 账号历史：一页流水，外加每条「是谁做的」。
    pub async fn account_log(
        &self,
        filter: &AccountLogFilter,
        page: PageRequest,
    ) -> Result<Page<AccountLogRow>> {
        let actions: Vec<String> = match filter
            .action
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            // 逗号分隔的一串：界面上「登录记录」这一个筛选项背后是四条动作
            Some(list) => list
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };
        let actions = if actions.is_empty() {
            ACCOUNT_ACTIONS.iter().map(|name| name.to_string()).collect()
        } else {
            actions
        };
        let page = self
            .repo
            .list_account_audit(
                &actions,
                filter.uid.as_deref(),
                filter.query.as_deref(),
                page,
            )
            .await?;

        // 一页里的操作人一次查完：逐条回查会变成 N+1
        let actor_ids: Vec<i64> = page
            .items
            .iter()
            .filter_map(|row| row.actor_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let identities = self.repo.identities_for_ids(&actor_ids).await?;

        let rows = page
            .items
            .iter()
            .map(|row| shape(row, &identities))
            .collect::<Vec<_>>();
        Ok(Page::new(
            rows,
            page.total,
            PageRequest {
                page: page.page,
                per_page: page.per_page,
            },
        ))
    }

    /// 写一条账号历史。管理员侧与用户自助侧都从这里走，保证动作名与 detail
    /// 里那几个键（`via` / `actor_email`）的形状一致——它们是「谁做的」的依据。
    pub async fn audit_account_event(
        &self,
        action: &str,
        actor_id: Option<i64>,
        actor_email: Option<&str>,
        target_uid: &str,
        mut detail: serde_json::Value,
        ip: Option<&str>,
    ) {
        if let Some(actor_email) = actor_email {
            if let Some(map) = detail.as_object_mut() {
                map.insert(
                    "actor_email".to_string(),
                    serde_json::Value::String(actor_email.to_string()),
                );
            }
        }
        self.audit(
            AuditEntry::new(action)
                .actor(actor_id)
                .target(target_uid)
                .detail(detail)
                .ip(ip.map(str::to_string)),
        )
        .await;
    }
}

/// 把一条审计行翻译成账号历史：补出邮箱、判定「谁做的」。
fn shape(row: &AuditLog, identities: &std::collections::HashMap<i64, (String, String)>) -> AccountLogRow {
    let detail = &row.detail;
    let detail_str = |key: &str| -> Option<String> {
        detail
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .filter(|value| !value.is_empty())
    };

    // 邮箱：换邮箱那条留 `to`（现在用的那个），注册与注销留 `email`
    let mut email = detail_str("email")
        .or_else(|| detail_str("to"))
        .or_else(|| detail_str("from"));

    let actor = row
        .actor_id
        .map(|id| identities.get(&id).cloned())
        .unwrap_or(None);
    let self_initiated =
        detail_str("via").as_deref() == Some("self") || SELF_ONLY_ACTIONS.contains(&row.action.as_str());

    // 系统配置动作的 target 是 settings / cidr / backend:id，不是账号
    let system_action = SYSTEM_CONFIG_ACTIONS.contains(&row.action.as_str());
    if system_action {
        email = None;
    }
    // 登录与退出这类审计行只记了操作人、没记目标，可它讲的就是操作人自己。
    // 账号列空着的话，「哪个账号什么时候登录过」这一页反而答不上来
    let target_uid = if system_action {
        None
    } else {
        row.target.clone().or_else(|| {
            if self_initiated {
                actor.as_ref().map(|(uid, _)| uid.clone())
            } else {
                None
            }
        })
    };
    if email.is_none() && target_uid.is_some() && self_initiated {
        email = actor.as_ref().map(|(_, mail)| mail.clone());
    }

    let (actor_kind, actor_label) = match (row.actor_id, actor.clone()) {
        // 没有操作人：系统自己做的事（例如内容审核累计违规的自动处置）
        (None, _) => ("system".to_string(), "系统".to_string()),
        (Some(_), Some((uid, mail))) => {
            let is_self = self_initiated || Some(uid.as_str()) == row.target.as_deref();
            if is_self {
                ("self".to_string(), format!("本人（{mail}）"))
            } else {
                (
                    "admin".to_string(),
                    format!("管理员 {mail}"),
                )
            }
        }
        // 操作人账号已经不在了：detail 里留了快照就用快照，否则直说查不到
        (Some(id), None) => {
            if self_initiated {
                ("self".to_string(), "本人".to_string())
            } else if let Some(snapshot) = detail_str("actor_email") {
                ("admin".to_string(), format!("管理员 {snapshot}"))
            } else {
                (
                    "unknown".to_string(),
                    format!("已注销的操作人 #{id}"),
                )
            }
        }
    };

    let category = action_category(&row.action);
    AccountLogRow {
        id: row.id,
        action: row.action.clone(),
        action_label: account_action_label(&row.action).to_string(),
        category: category.to_string(),
        category_label: category_label(category).to_string(),
        target_uid,
        target: row.target.clone(),
        email,
        actor_kind,
        actor_label,
        actor_id: row.actor_id,
        ip: row.ip.clone(),
        detail: row.detail.clone(),
        created_at: row.created_at.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_config_rows_do_not_look_like_user_targets() {
        let mut identities = std::collections::HashMap::new();
        identities.insert(1, ("1000".to_string(), "root@example.com".to_string()));
        let row = AuditLog {
            id: 7,
            actor_id: Some(1),
            action: actions::SETTINGS_CHANGE.to_string(),
            target: Some("settings".to_string()),
            detail: serde_json::json!({ "keys": ["site.name"] }),
            ip: Some("203.0.113.8".to_string()),
            created_at: chrono::Utc::now(),
        };

        let shaped = shape(&row, &identities);
        assert_eq!(shaped.action_label, "修改系统配置");
        assert_eq!(shaped.category, "system");
        assert_eq!(shaped.category_label, "系统设置");
        assert!(shaped.target_uid.is_none(), "系统配置的 target 不是用户 UID");
        assert_eq!(shaped.target.as_deref(), Some("settings"));
        assert_eq!(shaped.actor_kind, "admin");
    }

    #[test]
    fn account_management_rows_are_account_info() {
        let mut identities = std::collections::HashMap::new();
        identities.insert(1, ("1000".to_string(), "root@example.com".to_string()));
        let row = AuditLog {
            id: 8,
            actor_id: Some(1),
            action: actions::ACCOUNT_UPDATE.to_string(),
            target: Some("2001".to_string()),
            detail: serde_json::json!({ "role": "user" }),
            ip: None,
            created_at: chrono::Utc::now(),
        };

        let shaped = shape(&row, &identities);
        assert_eq!(shaped.action_label, "修改账户信息");
        assert_eq!(shaped.category, "account");
        assert_eq!(shaped.category_label, "账户信息");
        assert_eq!(shaped.target_uid.as_deref(), Some("2001"));
    }

    #[test]
    fn system_actions_are_in_the_default_log_view() {
        for action in SYSTEM_CONFIG_ACTIONS {
            assert!(
                ACCOUNT_ACTIONS.contains(action),
                "系统配置动作 {action} 应出现在默认日志里"
            );
        }
    }
}
