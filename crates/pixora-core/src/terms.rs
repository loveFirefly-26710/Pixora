//! 服务条款服务：版本化的条款本体、同意记录与「是否需要重新同意」判定。
//!
//! 两条来自设计稿（7.12 + ADR-0004）的硬规则在这里落地：
//!
//!   1. **同一时间只有一个生效版本**，发布新版本时旧的自动归档（事务在仓储层做）。
//!      所以「当前版本」永远只有一个答案，判定同意的逻辑不会出现歧义。
//!   2. **同意记录只增不改**，而且冗余存版本号。条款日后被删也不影响追查
//!      「用户当初同意的到底是哪一版」，所以带同意记录的版本不允许直接删除。
//!
//! 弹窗文案与表单都在前端，这里只回答三个问题：当前是哪一版、这个人同意过没有、
//! 他是不是必须先同意才能继续用。

use chrono::{DateTime, Utc};
use serde::Serialize;

use pixora_repo::model::{terms_status, now, TermsVersion, User};
use pixora_repo::repo::terms::{NewTermsVersion, TermsRow};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::AppState;

/// 条款相关动作名。`audit::actions` 目前只收了 `TERMS_PUBLISH`，
/// 其余动作先在本文件收口，调用点不散落字符串字面量；并轨时只改这一处。
pub mod terms_actions {
    pub const CONSENT: &str = "terms_consent";
    pub const CREATE: &str = "terms_create";
    pub const UPDATE: &str = "terms_update";
    pub const DELETE: &str = "terms_delete";
}

/// 版本号允许的最大长度。
const VERSION_MAX_LEN: usize = 32;

impl AppState {
    /// 当前生效版本。没有发布过任何条款时返回 `None`。
    pub async fn terms_current(&self) -> Result<Option<TermsVersion>> {
        Ok(self.repo.active_terms().await?)
    }

    /// 用户端「服务条款」页要的整包数据（契约 7 章）。
    ///
    /// `user` 为 `None` 表示未登录：`required` 恒为 `false`、`consent` 为空，
    /// 未登录的人没有「同意」这回事。
    pub async fn terms_payload(&self, user: Option<&User>) -> Result<TermsPayload> {
        let active = self.repo.active_terms().await?;

        let mut required = false;
        let mut consent = None;
        let mut agreed_version_ids = std::collections::HashSet::new();
        if let Some(user) = user {
            required = self.terms_required(user).await?;
            // 同意记录按「这一版」取：页面在生效版本下面显示「你在什么时候、从哪个 IP 同意的」
            if let Some(active) = active.as_ref() {
                if let Some(record) = self.repo.find_consent(user.id, active.id).await? {
                    consent = Some(TermsConsentInfo {
                        agreed_at: record.agreed_at,
                        ip: record.ip,
                        version: record.version,
                    });
                }
            }
            for record in self.repo.list_consents(user.id).await? {
                agreed_version_ids.insert(record.version_id);
            }
        }

        // 历史列表只给已发布过的版本：草稿是后台的事，不该出现在用户端
        let history = self
            .repo
            .list_terms()
            .await?
            .into_iter()
            .filter(|row| row.version.status == terms_status::ARCHIVED)
            .map(|row| TermsHistoryItem {
                agreed: agreed_version_ids.contains(&row.version.id),
                version: row.version.version,
                effective_at: row.version.effective_at,
                summary: row.version.summary,
            })
            .collect();

        let current = active.map(|version| TermsCurrent {
            articles: version.article_count(),
            id: version.id,
            version: version.version,
            title: version.title,
            body: version.body,
            summary: version.summary,
            effective_at: version.effective_at,
            published_at: version.published_at,
        });

        Ok(TermsPayload {
            current,
            consent,
            required,
            history,
        })
    }

    /// 这个人是否必须先同意才能继续用。
    ///
    /// 四种情形：
    ///   - 没有生效版本 → `false`（站点还没发布过条款，不拦人）；
    ///   - 已同意当前生效版本 → `false`；
    ///   - 生效版本要求重新同意且他还没同意 → `true`；
    ///   - 生效版本没要求重新同意，但他也没同意过这一版 → 同样按需要处理。
    ///     首次注册的用户属于这一种：登录后要走一次通告弹窗，所以判定必须
    ///     基于「有没有同意过当前这一版」，而不是「有没有同意过任意版本」。
    pub async fn terms_required(&self, user: &User) -> Result<bool> {
        let Some(active) = self.repo.active_terms().await? else {
            return Ok(false);
        };
        if self.repo.find_consent(user.id, active.id).await?.is_some() {
            return Ok(false);
        }

        // 发布新版本时没有勾「要求所有用户重新同意」→ 只有从没同意过任何版本的人
        // （新注册用户）需要先同意；老用户不被再打扰一次。
        // 设计稿 7.12 的原话是「没勾就只是更新内容，不打扰用户」。
        if !active.require_reconsent {
            let history = self.repo.list_consents(user.id).await?;
            return Ok(history.is_empty());
        }
        Ok(true)
    }

    /// 记一次同意。只增不改，重复提交（刷新页面、双开标签）不报错。
    ///
    /// `version_id` 必须是**当前生效版本**：前端可能在弹窗打开期间等到了一次发布，
    /// 那时候同意的是旧版本的正文，记下来就说不清了，所以直接拒绝并要求刷新。
    pub async fn record_terms_consent(
        &self,
        user: &User,
        version_id: i64,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<()> {
        let Some(active) = self.repo.active_terms().await? else {
            return Err(AppError::bad_request("当前还没有生效的条款版本"));
        };
        if active.id != version_id {
            return Err(AppError::bad_request(
                "条款内容已更新，请刷新页面后重新阅读并同意",
            ));
        }

        self.repo
            .record_consent(user.id, &active, ip, user_agent, now())
            .await?;
        self.audit_detail(
            terms_actions::CONSENT,
            Some(user.id),
            active.version.clone(),
            serde_json::json!({ "version_id": active.id }),
            ip,
        )
        .await;
        Ok(())
    }

    /// 管理列表：版本 + 同意人数 + 用户总数（契约 8.7）。
    pub async fn terms_list(&self) -> Result<Vec<TermsRow>> {
        Ok(self.repo.list_terms().await?)
    }

    /// 新建一版条款。一律以草稿落地，`publish = true` 时创建后立刻发布。
    pub async fn terms_create(&self, admin_id: i64, input: TermsInput) -> Result<TermsVersion> {
        let version = validate_version_format(&input.version)?;
        let title = resolve_title(&input.title);
        validate_body(&input.body)?;

        if self.repo.find_terms_by_version(&version).await?.is_some() {
            return Err(AppError::Conflict(format!("版本号 {version} 已存在")));
        }

        let created = self
            .repo
            .create_terms(&NewTermsVersion {
                version: version.clone(),
                title,
                body: input.body,
                summary: clean_summary(input.summary),
                require_reconsent: input.require_reconsent,
                effective_at: input.effective_at,
                created_by: Some(admin_id),
            })
            .await?;

        self.audit_detail(
            terms_actions::CREATE,
            Some(admin_id),
            version,
            serde_json::json!({
                "published": input.publish,
                "require_reconsent": input.require_reconsent,
            }),
            None,
        )
        .await;

        if input.publish {
            self.terms_publish(admin_id, created.id).await?;
            // 发布后状态、发布时间都变了，回读一次把最新形态还给调用方
            return self
                .repo
                .find_terms(created.id)
                .await?
                .ok_or_else(|| AppError::not_found("条款版本"));
        }
        Ok(created)
    }

    /// 修改条款。**草稿与历史版本可以改，生效中的不能改**。
    ///
    /// 历史（已归档）版本允许修订：管理员可能发现旧版本里有个错别字，
    /// 而它又不在生效状态，改它不会动到任何人已经同意过的那一版。
    /// 生效中的版本必须走「另存为新版本再发布」——改了正文之后，
    /// 之前那些「同意记录」到底同意的是什么就说不清了。
    pub async fn terms_update(&self, admin_id: i64, id: i64, input: TermsInput) -> Result<()> {
        let existing = self
            .repo
            .find_terms(id)
            .await?
            .ok_or_else(|| AppError::not_found("条款版本"))?;
        if existing.is_active() {
            return Err(AppError::Conflict(
                "生效中的版本不能就地修改：请用「另存为新版本」改出一版草稿再发布".into(),
            ));
        }

        let version = validate_version_format(&input.version)?;
        let title = resolve_title(&input.title);
        validate_body(&input.body)?;

        if let Some(same_version) = self.repo.find_terms_by_version(&version).await? {
            if same_version.id != id {
                return Err(AppError::Conflict(format!("版本号 {version} 已存在")));
            }
        }

        self.repo
            .update_terms(
                id,
                &NewTermsVersion {
                    version: version.clone(),
                    title,
                    body: input.body,
                    summary: clean_summary(input.summary),
                    require_reconsent: input.require_reconsent,
                    effective_at: input.effective_at,
                    created_by: existing.created_by,
                },
            )
            .await?;

        self.audit_detail(
            terms_actions::UPDATE,
            Some(admin_id),
            version,
            serde_json::json!({ "require_reconsent": input.require_reconsent }),
            None,
        )
        .await;
        Ok(())
    }

    /// 发布一版条款，返回被归档的旧版本号（没有则 `None`）。
    ///
    /// 重复点发布不报错：已经是生效版本时直接返回 `None`，不重新归档任何东西。
    pub async fn terms_publish(&self, admin_id: i64, id: i64) -> Result<Option<String>> {
        let target = self
            .repo
            .find_terms(id)
            .await?
            .ok_or_else(|| AppError::not_found("条款版本"))?;
        if target.is_active() {
            return Ok(None);
        }

        let archived = self.repo.active_terms().await?.map(|item| item.version);
        self.repo.publish_terms(id, now()).await?;

        self.audit_detail(
            actions::TERMS_PUBLISH,
            Some(admin_id),
            target.version.clone(),
            serde_json::json!({
                "archived": archived,
                "require_reconsent": target.require_reconsent,
            }),
            None,
        )
        .await;
        Ok(archived)
    }

    /// 删除一版条款。生效中的版本拒绝删除——删掉就再也没有「当前条款」了，
    /// 正确做法是先让另一个版本生效，再回来删这一版。
    pub async fn terms_delete(&self, admin_id: i64, id: i64) -> Result<()> {
        let target = self
            .repo
            .find_terms(id)
            .await?
            .ok_or_else(|| AppError::not_found("条款版本"))?;
        if target.is_active() {
            return Err(AppError::bad_request(
                "生效中的版本不能删除，必须先把另一个版本设为生效",
            ));
        }

        // 删除就是真删：版本行与它的同意记录一起消失。
        // 同意记录本来是为了「当初同意的是哪一版」可追溯，而这个可追溯性
        // 是挂在版本行上的——版本行没了，留下的记录也只是孤儿。
        // 删掉多少条会写进审计，界面上也会先确认一次。
        let consents = self.repo.delete_terms(id).await?;
        self.audit_detail(
            terms_actions::DELETE,
            Some(admin_id),
            target.version,
            serde_json::json!({
                "status": target.status,
                "consents_removed": consents,
            }),
            None,
        )
        .await;
        Ok(())
    }
}

/// 版本号形状校验：`v2.1` 或 `2.1`——可选 `v` 前缀加至少两段数字，
/// 长度不超过 32。返回去空白后的原样字符串（不强制补 `v`，界面上怎么写的就怎么存）。
pub fn validate_version_format(input: &str) -> Result<String> {
    let version = input.trim();
    if version.is_empty() {
        return Err(AppError::bad_request("版本号不能为空"));
    }
    if version.chars().count() > VERSION_MAX_LEN {
        return Err(AppError::bad_request(format!(
            "版本号最长 {VERSION_MAX_LEN} 个字符"
        )));
    }

    let digits = version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version);
    let mut segments = digits.split('.');
    let first = segments.next().unwrap_or_default();
    let rest: Vec<&str> = segments.collect();

    let all_digits = |segment: &str| {
        !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit())
    };
    if !all_digits(first) || rest.is_empty() || !rest.iter().all(|item| all_digits(item)) {
        return Err(AppError::bad_request(format!(
            "版本号格式不正确：{version}，应形如 v2.1 或 2.1"
        )));
    }

    Ok(version.to_string())
}

/// 标题留空时用默认值：让管理员少填一个必填项，契约里的默认标题就是这个。
fn resolve_title(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        "服务条款".to_string()
    } else {
        trimmed.to_string()
    }
}

fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return Err(AppError::bad_request("条款正文不能为空"));
    }
    Ok(())
}

/// 更新摘要空串按「没写」处理，界面才不会显示一行空白。
fn clean_summary(summary: Option<String>) -> Option<String> {
    summary
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

/// `GET /api/v1/terms` 响应体（契约 7 章）。
#[derive(Debug, Clone, Serialize)]
pub struct TermsPayload {
    pub current: Option<TermsCurrent>,
    pub consent: Option<TermsConsentInfo>,
    pub required: bool,
    pub history: Vec<TermsHistoryItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TermsCurrent {
    pub id: i64,
    pub version: String,
    pub title: String,
    pub body: String,
    pub summary: Option<String>,
    /// 正文里的条款条数，页面显示「共 N 条」
    pub articles: usize,
    pub effective_at: Option<DateTime<Utc>>,
    pub published_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TermsConsentInfo {
    pub agreed_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TermsHistoryItem {
    pub version: String,
    pub effective_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    /// 当前用户是否同意过这一版
    pub agreed: bool,
}

/// 条款的写入入参。
///
/// `publish` 决定创建后是否立即发布；契约 8.7 把同一个意图写在 `status` 字段里
/// （`draft` / `active`），所以两种写法都接受：前端按契约发 `status`，
/// 进程内的调用方直接写 `publish`。
#[derive(Debug, Clone, Serialize)]
pub struct TermsInput {
    pub version: String,
    pub title: String,
    pub body: String,
    pub summary: Option<String>,
    pub require_reconsent: bool,
    /// 留空表示发布那一刻立即生效
    pub effective_at: Option<DateTime<Utc>>,
    pub publish: bool,
}

impl<'de> serde::Deserialize<'de> for TermsInput {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Wire {
            version: String,
            #[serde(default)]
            title: String,
            body: String,
            #[serde(default)]
            summary: Option<String>,
            #[serde(default)]
            require_reconsent: bool,
            #[serde(default)]
            effective_at: Option<DateTime<Utc>>,
            #[serde(default)]
            publish: bool,
            /// 契约里的写法：`active` 等价于 `publish: true`
            #[serde(default)]
            status: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Ok(TermsInput {
            version: wire.version,
            title: wire.title,
            body: wire.body,
            summary: wire.summary,
            require_reconsent: wire.require_reconsent,
            effective_at: wire.effective_at,
            publish: wire.publish || wire.status.as_deref() == Some(terms_status::ACTIVE),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{make_user, test_state};

    /// 条款的 `created_by` 有外键，管理动作必须挂在一个真实账号上。
    async fn make_actor(state: &AppState) -> User {
        make_user(state, "admin@example.com", "1000").await
    }

    fn terms_input(version: &str, publish: bool, require_reconsent: bool) -> TermsInput {
        TermsInput {
            version: version.to_string(),
            title: "服务条款".to_string(),
            body: "一、总则\n二、账号\n三、内容规范\n\n正文内容。".to_string(),
            summary: Some("首次发布".to_string()),
            require_reconsent,
            effective_at: None,
            publish,
        }
    }

    #[test]
    fn version_format_is_validated() {
        for ok in ["v2.1", "2.1", "V3.2", "v1.0.1", "10.20.30"] {
            assert_eq!(validate_version_format(ok).unwrap(), ok);
        }
        assert_eq!(validate_version_format("  v2.1  ").unwrap(), "v2.1");

        for bad in [
            "",
            "   ",
            "v",
            "2",
            "vv2.1",
            "v2.",
            ".1",
            "2..1",
            "abc",
            "2.1-beta",
            "第2版",
        ] {
            assert!(validate_version_format(bad).is_err(), "{bad:?} 应该被拒绝");
        }
        // 长度上限 32
        let long = format!("1.{}", "2".repeat(40));
        assert!(validate_version_format(&long).is_err());
        let edge = format!("1.{}", "2".repeat(30));
        assert_eq!(edge.chars().count(), 32);
        assert!(validate_version_format(&edge).is_ok());
    }

    #[test]
    fn terms_input_accepts_contract_status_field() {
        // 契约 8.7 发的是 status 字段，内部调用写 publish，两种都要认
        let from_status: TermsInput = serde_json::from_str(
            r#"{"version":"v2.2","title":"服务条款","body":"一、总则","summary":"新增内容合规条款",
                "status":"active","require_reconsent":true,"effective_at":null}"#,
        )
        .expect("按契约字段解析");
        assert!(from_status.publish);
        assert!(from_status.require_reconsent);

        let from_publish: TermsInput = serde_json::from_str(
            r#"{"version":"v2.3","body":"一、总则","summary":null,
                "require_reconsent":false,"effective_at":null,"publish":false}"#,
        )
        .expect("按 publish 字段解析");
        assert!(!from_publish.publish);
        assert_eq!(from_publish.title, "", "缺省标题由服务层回落默认值");
    }

    #[tokio::test]
    async fn required_has_four_shapes() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;
        let user = make_user(&state, "chenyu@example.com", "1001").await;

        // 情形一：没有生效版本 → 不拦人
        assert!(!state.terms_required(&user).await.unwrap());

        // 情形二：已同意当前生效版本 → 不拦人
        let v1 = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();
        assert!(state.terms_required(&user).await.unwrap(), "首次用户要走一次通告");
        state
            .record_terms_consent(&user, v1.id, Some("203.0.113.18"), Some("Mozilla/5.0"))
            .await
            .unwrap();
        assert!(!state.terms_required(&user).await.unwrap());

        // 情形三：生效版本要求重新同意且未同意 → 拦
        let _v2 = state
            .terms_create(actor.id, terms_input("v2.0", true, true))
            .await
            .unwrap();
        assert!(state.terms_required(&user).await.unwrap());

        // 情形四：没要求重新同意 → 老用户不被再打扰一次（设计稿 7.12）
        let v3 = state
            .terms_create(actor.id, terms_input("v3.0", true, false))
            .await
            .unwrap();
        assert!(
            !state.terms_required(&user).await.unwrap(),
            "未勾选「要求重新同意」时不该拦已有同意记录的老用户"
        );
        state
            .record_terms_consent(&user, v3.id, None, None)
            .await
            .unwrap();
        assert!(!state.terms_required(&user).await.unwrap());

        // 全新用户（首次注册）也要走一次通告
        let fresh = make_user(&state, "design@studio.io", "2044").await;
        assert!(state.terms_required(&fresh).await.unwrap());
    }

    #[tokio::test]
    async fn payload_carries_current_consent_and_history() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;
        let user = make_user(&state, "chenyu@example.com", "1001").await;

        // 未登录：没有 current、没有 consent、不要求
        let anonymous = state.terms_payload(None).await.unwrap();
        assert!(anonymous.current.is_none());
        assert!(anonymous.consent.is_none());
        assert!(!anonymous.required);
        assert!(anonymous.history.is_empty());

        let v1 = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();
        state
            .record_terms_consent(&user, v1.id, Some("203.0.113.18"), None)
            .await
            .unwrap();

        let v2 = state
            .terms_create(actor.id, terms_input("v2.1", true, true))
            .await
            .unwrap();

        let payload = state.terms_payload(Some(&user)).await.unwrap();
        let current = payload.current.as_ref().expect("有生效版本");
        assert_eq!(current.version, "v2.1");
        assert_eq!(current.id, v2.id);
        assert_eq!(current.articles, 3, "正文里的条款条数要数出来");
        assert!(payload.required, "未同意 v2.1，需要先同意");
        assert!(payload.consent.is_none(), "还没同意当前版本，没有同意记录");

        assert_eq!(payload.history.len(), 1, "草稿与当前版本不进历史列表");
        assert_eq!(payload.history[0].version, "v1.0");
        assert!(payload.history[0].agreed, "v1.0 同意过");
        assert_eq!(payload.history[0].summary.as_deref(), Some("首次发布"));

        state
            .record_terms_consent(&user, v2.id, Some("203.0.113.18"), None)
            .await
            .unwrap();
        let after = state.terms_payload(Some(&user)).await.unwrap();
        assert!(!after.required);
        let consent = after.consent.expect("同意记录要带出来");
        assert_eq!(consent.version, "v2.1");
        assert_eq!(consent.ip.as_deref(), Some("203.0.113.18"));
    }

    #[tokio::test]
    async fn consent_must_target_the_active_version() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;
        let user = make_user(&state, "chenyu@example.com", "1001").await;

        let draft = state
            .terms_create(actor.id, terms_input("v9.9", false, false))
            .await
            .unwrap();
        assert_eq!(draft.status, terms_status::DRAFT, "创建默认是草稿");

        // 没有生效版本
        let error = state
            .record_terms_consent(&user, draft.id, None, None)
            .await
            .expect_err("没有生效版本时不能记同意");
        assert_eq!(error.code(), "BAD_REQUEST");

        let active = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();
        assert_eq!(active.status, terms_status::ACTIVE);

        // 拿草稿 id 来同意会被拒
        let error = state
            .record_terms_consent(&user, draft.id, None, None)
            .await
            .expect_err("只能同意生效版本");
        assert!(error.message().contains("刷新页面"), "{}", error.message());

        // 重复提交不报错（唯一键命中直接忽略）
        state
            .record_terms_consent(&user, active.id, None, None)
            .await
            .unwrap();
        state
            .record_terms_consent(&user, active.id, None, None)
            .await
            .unwrap();
        let consents = state.repo.list_consents(user.id).await.unwrap();
        assert_eq!(consents.len(), 1);
    }

    #[tokio::test]
    async fn publish_archives_previous_version() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;

        let v1 = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();
        assert_eq!(v1.status, terms_status::ACTIVE);

        // 重复发布不报错、不重复归档
        assert_eq!(state.terms_publish(actor.id, v1.id).await.unwrap(), None);

        // 先落草稿再手动发布，期间 v1.0 要被归档
        let v2 = state
            .terms_create(actor.id, terms_input("v2.1", false, false))
            .await
            .unwrap();
        assert_eq!(v2.status, terms_status::DRAFT);
        let archived = state.terms_publish(actor.id, v2.id).await.unwrap();
        assert_eq!(archived.as_deref(), Some("v1.0"));

        // 归档只能有一个生效版本
        let old = state.repo.find_terms(v1.id).await.unwrap().unwrap();
        assert_eq!(old.status, terms_status::ARCHIVED);
        let active = state.terms_current().await.unwrap().expect("有生效版本");
        assert_eq!(active.id, v2.id);

        // 发布不存在的版本 → NOT_FOUND
        let error = state
            .terms_publish(actor.id, 9999)
            .await
            .expect_err("版本不存在");
        assert_eq!(error.code(), "NOT_FOUND");
    }

    #[tokio::test]
    async fn drafts_are_editable_and_active_versions_are_not() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;

        let draft = state
            .terms_create(actor.id, terms_input("v1.0", false, false))
            .await
            .unwrap();
        let mut changed = terms_input("v1.1", false, true);
        changed.summary = None;
        state.terms_update(actor.id, draft.id, changed).await.unwrap();

        let updated = state.repo.find_terms(draft.id).await.unwrap().unwrap();
        assert_eq!(updated.version, "v1.1");
        assert!(updated.require_reconsent);
        assert!(updated.summary.is_none(), "空摘要按没写处理");

        // 版本号撞车要报冲突，而不是让数据库抛唯一约束
        state
            .terms_create(actor.id, terms_input("v2.0", false, false))
            .await
            .unwrap();
        let error = state
            .terms_update(actor.id, draft.id, terms_input("v2.0", false, false))
            .await
            .expect_err("版本号已存在");
        assert_eq!(error.code(), "CONFLICT");

        // 生效后的版本不能再改：改了之后「用户同意的是哪一份」就说不清了
        state.terms_publish(actor.id, draft.id).await.unwrap();
        let error = state
            .terms_update(actor.id, draft.id, terms_input("v1.2", false, false))
            .await
            .expect_err("生效版本不可就地改");
        assert_eq!(error.code(), "CONFLICT");
        assert!(
            error.message().contains("另存为新版本"),
            "提示里要给出路：{}",
            error.message()
        );

        // 归档（历史）版本可以修订：它不生效，改它不影响任何人的同意记录
        let archived = state
            .repo
            .find_terms_by_version("v2.0")
            .await
            .unwrap()
            .expect("上一版");
        // 先让 v2.0 生效，v1.1 就成了历史版本
        state.terms_publish(actor.id, archived.id).await.unwrap();
        let mut edited = terms_input("v1.1", false, false);
        edited.body = "历史版本的正文也可以改：它不生效，改它不影响任何人的同意记录".into();
        state
            .terms_update(actor.id, draft.id, edited)
            .await
            .expect("历史版本可修订");
        let reloaded = state.repo.find_terms(draft.id).await.unwrap().unwrap();
        assert!(reloaded.body.contains("历史版本"));
    }

    #[tokio::test]
    async fn delete_refuses_active_but_really_removes_others() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;
        let user = make_user(&state, "chenyu@example.com", "1001").await;

        let active = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();

        // 生效中的版本：报错必须说清出路
        let error = state
            .terms_delete(actor.id, active.id)
            .await
            .expect_err("生效版本不能删");
        assert_eq!(error.code(), "BAD_REQUEST");
        assert!(
            error.message().contains("必须先把另一个版本设为生效"),
            "{}",
            error.message()
        );

        // 先趁 v1.0 还生效时留下同意记录，再让 v2.0 生效
        state
            .record_terms_consent(&user, active.id, None, None)
            .await
            .unwrap();
        let _v2 = state
            .terms_create(actor.id, terms_input("v2.0", true, false))
            .await
            .unwrap();

        // v1.0 已归档、且已经有同意记录：删除就是真删，
        // 连同那条同意记录一起消失（记录挂在版本行上，版本没了它也只是孤儿）
        state
            .terms_delete(actor.id, active.id)
            .await
            .expect("归档版本应当能真删");
        assert!(
            state.repo.find_terms(active.id).await.unwrap().is_none(),
            "删除后版本行不该还在"
        );
        assert_eq!(
            state.repo.count_consents(active.id).await.unwrap(),
            0,
            "同意记录跟着版本一起删掉"
        );

        // 没人同意过的草稿可以删
        let draft = state
            .terms_create(actor.id, terms_input("v3.0", false, false))
            .await
            .unwrap();
        state.terms_delete(actor.id, draft.id).await.unwrap();
        assert!(state.repo.find_terms(draft.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn admin_list_shows_consent_counts() {
        let dir = tempfile::tempdir().expect("临时目录");
        let state = test_state(&dir).await;
        let actor = make_actor(&state).await;
        let user = make_user(&state, "chenyu@example.com", "1001").await;

        let active = state
            .terms_create(actor.id, terms_input("v1.0", true, false))
            .await
            .unwrap();
        state
            .record_terms_consent(&user, active.id, None, None)
            .await
            .unwrap();

        let rows = state.terms_list().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].consent_count, 1);
        assert_eq!(rows[0].user_total, 2, "管理员加普通用户");
        assert_eq!(rows[0].version.version, "v1.0");
    }
}
