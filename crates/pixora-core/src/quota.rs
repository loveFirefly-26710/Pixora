//! 配额与容量：系统总额、用户配额分配、注册默认额度，以及上传前的准入。
//!
//! 「配额总额是硬约束」这句话在本项目里的完整形态是：
//! **所有已注册用户的配额合计 ≤ 可分配上限 = 系统总额 − 宿主机整盘已用**。
//! 磁盘上被别的数据先占住的那部分额度是分不出去的，把它算进来才不会出现
//! 「配额都发出去了、上传却写不进盘」。自动模式下这个减数就是探测到的整盘
//! 已用；手填总额或网络挂载降级时拿不到整盘数字，上限退回系统总额。
//! 它必须由服务端把关——管理员随手给几个人加到几百 GB，上限早就超了也不知道，
//! 等磁盘写满时已经来不及。所以本文件里每一处改动配额的路径，改完之前都要
//! 先算一遍合计，超出就拒绝，并把「还差多少」直接说在错误里（界面上要显示这句）。
//! 已经越界时（磁盘占用会把上限压下来）「只减不增」的调整必须放行，
//! 否则管理员连调低配额都做不到，回不到合规状态。
//!
//! 另一条容易踩的规矩：**接口与库里一律用字节数**，格式化只发生在展示层
//! （[`human_bytes`] 只服务于错误文案与界面提示，不进任何存储或比较逻辑）。

use serde::{Deserialize, Serialize};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::settings::keys;
use crate::AppState;

/// `GET /admin/quota` 的响应体。字段名与 `docs/api-contract.md` 8.3 定死一致。
#[derive(Debug, Clone, Serialize)]
pub struct QuotaPayload {
    pub capacity: QuotaCapacity,
    /// 可分配上限（系统总额 − 宿主机整盘已用）。拿不到总额或整盘数字时
    /// 退回总额；两者都没有按 0 给，界面显示「上限未知」
    pub allocatable: i64,
    pub allocated: i64,
    pub allocated_percent: f64,
    pub remaining: i64,
    pub default_quota_bytes: i64,
    pub per_user_max: i64,
    pub auto_close: AutoCloseView,
    pub top_users: Vec<UserRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaCapacity {
    pub mode: String,
    pub total: Option<i64>,
    /// 本项目占用（库内 blob 汇总）
    pub used: i64,
    /// 宿主机整盘已用与可用，只有 auto 且探测可信时才有；占用率与自动关闭
    /// 注册按它算（磁盘满没满跟是谁占的无关），没有时退回本项目占用
    pub host_used: Option<i64>,
    pub host_available: Option<i64>,
    pub source: String,
    pub note: String,
}

/// 容量触顶自动关闭注册的配置与当前状态。
#[derive(Debug, Clone, Serialize)]
pub struct AutoCloseView {
    pub enabled: bool,
    pub on: i64,
    pub off: i64,
    pub register_open: bool,
    pub closed_reason: Option<String>,
}

/// 分配明细里的一行。形状对齐 `UserRow`（契约 8.1），管理端表格复用同一渲染。
#[derive(Debug, Clone, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub uid: String,
    pub email: String,
    pub role: String,
    pub status: String,
    pub status_label: String,
    pub quota_bytes: i64,
    pub used_bytes: i64,
    pub image_count: i64,
}

/// `PUT /admin/quota` 的请求体。字段全部可选，`None` 表示不改这一项——
/// 界面上只改动其中一块时，不能把没碰过的配置顺手写回默认值。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotaInput {
    pub capacity_mode: Option<String>,
    pub manual_total: Option<i64>,
    pub default_quota_bytes: Option<i64>,
    pub per_user_max: Option<i64>,
    pub auto_close_enabled: Option<bool>,
    pub auto_close_on: Option<i64>,
    pub auto_close_off: Option<i64>,
}

/// 单个用户调配额的结果。带上合计与剩余，让界面就地更新，不必整页重拉。
#[derive(Debug, Clone, Serialize)]
pub struct UserQuotaChange {
    pub uid: String,
    pub email: String,
    pub quota_bytes: i64,
    pub allocated: i64,
    pub remaining: i64,
}

impl AppState {
    /// 契约 8.3：容量、分配汇总、默认额度与占用前 5 名。
    pub async fn quota_payload(&self) -> Result<QuotaPayload> {
        let capacity = self.storage.capacity().await?;
        let allocated = self.repo.sum_allocated_quota(None).await?;
        let (default_quota_bytes, per_user_max) = self.settings.quota_values().await?;
        let policy = self.auto_close_policy().await?;
        let (register_open, closed_reason) = self.register_open().await?;

        let total = capacity.total;
        let ceiling = allocatable_ceiling(total, capacity.host_used);
        let (allocatable, allocated_percent, remaining, note) = match ceiling {
            Some(ceiling) if ceiling > 0 => (
                ceiling,
                round1(allocated as f64 / ceiling as f64 * 100.0),
                (ceiling - allocated).max(0),
                capacity.note,
            ),
            // 上限为 0（磁盘被占满）：没有分子可算，说清是磁盘满而不是「还没分配」
            Some(_) => (
                0,
                0.0,
                0,
                format!("{}（磁盘已满，没有可分配的额度）", capacity.note),
            ),
            // 总额未知时进度条没有分母：百分比与剩余都按 0 给，
            // 并在说明里讲清楚原因，否则界面上的 0% 会被当成「还没分配」
            None => (
                0,
                0.0,
                0,
                format!("{}（系统总额未知，占用率与剩余可分配额度按 0 展示）", capacity.note),
            ),
        };

        let top_users = self
            .repo
            .top_users_by_usage(5)
            .await?
            .iter()
            .map(user_row)
            .collect();

        Ok(QuotaPayload {
            capacity: QuotaCapacity {
                mode: capacity.mode,
                total,
                used: capacity.used,
                host_used: capacity.host_used,
                host_available: capacity.host_available,
                source: capacity.source,
                note,
            },
            allocatable,
            allocated,
            allocated_percent,
            remaining,
            default_quota_bytes,
            per_user_max,
            auto_close: AutoCloseView {
                enabled: policy.enabled,
                on: policy.on,
                off: policy.off,
                register_open,
                closed_reason,
            },
            top_users,
        })
    }

    /// 保存整页配额配置。校验在写库之前全部走完：拒绝保存时库里必须原样不动。
    pub async fn quota_update(&self, admin_id: i64, input: QuotaInput) -> Result<()> {
        let mode_before = self.settings.capacity_mode().await?;
        let manual_before = self.manual_total_setting().await?;
        let (default_before, max_before) = self.settings.quota_values().await?;
        let policy_before = self.auto_close_policy().await?;

        let mode = match input.capacity_mode.as_deref() {
            Some(raw) => {
                let mode = raw.trim().to_ascii_lowercase();
                if !matches!(mode.as_str(), "auto" | "manual" | "hidden") {
                    return Err(AppError::bad_request(format!(
                        "容量模式只能是 auto / manual / hidden，收到 {raw}"
                    )));
                }
                mode
            }
            None => mode_before.clone(),
        };

        // 配额为 0 等于「谁也别想上传」，这种配置只该由停用账号来做，不能靠手滑配出来
        if let Some(value) = input.default_quota_bytes {
            if value <= 0 {
                return Err(AppError::bad_request("新用户默认配额必须是正数"));
            }
        }
        if let Some(value) = input.per_user_max {
            if value <= 0 {
                return Err(AppError::bad_request("单用户配额上限必须是正数"));
            }
        }
        if let Some(value) = input.manual_total {
            if value <= 0 {
                return Err(AppError::bad_request("系统总额必须是正数"));
            }
        }

        let default_quota = input.default_quota_bytes.unwrap_or(default_before);
        let per_user_max = input.per_user_max.unwrap_or(max_before);
        if default_quota > per_user_max {
            return Err(AppError::bad_request(format!(
                "新用户默认配额（{}）不能大于单用户配额上限（{}），否则注册时就会配出超上限的账号",
                human_bytes(default_quota),
                human_bytes(per_user_max)
            )));
        }

        for (label, value) in [
            ("触发阈值", input.auto_close_on),
            ("恢复阈值", input.auto_close_off),
        ] {
            if let Some(value) = value {
                if !(0..=100).contains(&value) {
                    return Err(AppError::bad_request(format!(
                        "{label}必须是 0–100 之间的百分比，收到 {value}"
                    )));
                }
            }
        }
        let on = input.auto_close_on.unwrap_or(policy_before.on);
        let off = input.auto_close_off.unwrap_or(policy_before.off);
        if off >= on {
            return Err(AppError::bad_request(format!(
                "恢复阈值（{off}%）必须小于触发阈值（{on}%），否则注册入口会在同一占用率上反复开关"
            )));
        }

        // 可分配上限的来源随模式变：manual 用手填值（没填就沿用库里已有的），
        // auto 用容量探测的「总额 − 宿主机整盘已用」——磁盘上被别的数据先占住的
        // 额度是分不出去的；hidden 不设上限。注意 capacity() 读的是库里当下的模式，
        // 同一请求里从 manual 切到 auto 时它回的还是旧模式的总数——宁可保守拒绝，
        // 也不能放过一次超额分配。
        let manual_total = input.manual_total.or(manual_before);
        let (total, host_used) = match mode.as_str() {
            "manual" => (manual_total, None),
            "hidden" => (None, None),
            _ => {
                let capacity = self.storage.capacity().await?;
                (capacity.total, capacity.host_used)
            }
        };
        let ceiling = allocatable_ceiling(total, host_used);

        let mut check_skipped = false;
        if let Some(ceiling) = ceiling {
            let allocated = self.repo.sum_allocated_quota(None).await?;
            if allocated > ceiling {
                let gap = allocated - ceiling;
                // 讲清上限是怎么算出来的：手填/降级时它等于总额，自动模式下
                // 总额要先减掉磁盘占用，不说明白管理员会以为数字算错了
                let how = match host_used {
                    Some(host_used) => format!(
                        "系统总额 {} 先减去磁盘占用 {}，剩下的才是可分配额度",
                        human_bytes(total.unwrap_or(ceiling)),
                        human_bytes(host_used)
                    ),
                    None => format!("系统总额 {}", human_bytes(total.unwrap_or(ceiling))),
                };
                return Err(AppError::bad_request(format!(
                    "所有用户配额合计（{}）超出可分配上限（{}）：{}；还差 {}（{} 字节）。请先调低部分用户的配额，或改用「手动配置」给出更大的总额。",
                    human_bytes(allocated),
                    human_bytes(ceiling),
                    how,
                    human_bytes(gap),
                    gap
                )));
            }
        } else {
            // 没有总额就没有可比的上限，这项校验跳过；写进审计并在日志里说明，
            // 免得日后有人以为「保存成功 = 校验通过」
            check_skipped = true;
            tracing::info!(
                admin_id,
                "系统总额取不到，配额合计校验已跳过"
            );
        }

        if input.capacity_mode.is_some() {
            self.settings.set(keys::CAPACITY_MODE, &mode).await?;
        }
        if let Some(value) = input.manual_total {
            self.settings
                .set_integer(keys::CAPACITY_MANUAL_TOTAL, value)
                .await?;
        }
        if let Some(value) = input.default_quota_bytes {
            self.settings
                .set_integer(keys::QUOTA_DEFAULT, value)
                .await?;
        }
        if let Some(value) = input.per_user_max {
            self.settings
                .set_integer(keys::QUOTA_PER_USER_MAX, value)
                .await?;
        }
        if let Some(value) = input.auto_close_enabled {
            self.settings
                .set_bool(crate::stats::KEY_AUTO_CLOSE_ENABLED, value)
                .await?;
        }
        if let Some(value) = input.auto_close_on {
            self.settings
                .set_integer(keys::REGISTER_AUTO_CLOSE_ON, value)
                .await?;
        }
        if let Some(value) = input.auto_close_off {
            self.settings
                .set_integer(keys::REGISTER_AUTO_CLOSE_OFF, value)
                .await?;
        }

        // 审计里留全量前后值：只记「改了什么」的话，事后没法还原当时那套配置
        self.audit_detail(
            actions::SETTINGS_CHANGE,
            Some(admin_id),
            "quota",
            serde_json::json!({
                "before": {
                    "capacity_mode": mode_before,
                    "manual_total": manual_before,
                    "default_quota_bytes": default_before,
                    "per_user_max": max_before,
                    "auto_close_enabled": policy_before.enabled,
                    "auto_close_on": policy_before.on,
                    "auto_close_off": policy_before.off,
                },
                "after": {
                    "capacity_mode": mode,
                    "manual_total": manual_total,
                    "default_quota_bytes": default_quota,
                    "per_user_max": per_user_max,
                    "auto_close_enabled": input.auto_close_enabled.unwrap_or(policy_before.enabled),
                    "auto_close_on": on,
                    "auto_close_off": off,
                },
                "capacity_check_skipped": check_skipped,
            }),
            None,
        )
        .await;

        Ok(())
    }

    /// 单个用户调配额。`POST /api/v1/admin/users/{id}/quota` 的落点。
    pub async fn set_user_quota(
        &self,
        admin_id: i64,
        user_id: i64,
        quota_bytes: i64,
    ) -> Result<UserQuotaChange> {
        if quota_bytes <= 0 {
            return Err(AppError::bad_request("配额必须是正数"));
        }
        let user = self
            .repo
            .find_user_summary(user_id)
            .await?
            .ok_or_else(|| AppError::not_found("用户"))?;

        let (_, per_user_max) = self.settings.quota_values().await?;
        if quota_bytes > per_user_max {
            return Err(AppError::bad_request(format!(
                "单用户配额上限是 {}（{} 字节），{}（u_{}）不能配到 {}",
                human_bytes(per_user_max),
                per_user_max,
                user.email,
                user.uid,
                human_bytes(quota_bytes)
            )));
        }

        let capacity = self.storage.capacity().await?;
        let ceiling = allocatable_ceiling(capacity.total, capacity.host_used);
        if let Some(ceiling) = ceiling {
            // 把这个人换成新额度后合计不能破上限：先算「其他人 + 新的他」
            let others = self.repo.sum_allocated_quota(Some(user.id)).await?;
            let projected = others + quota_bytes;
            let allocated_now = self.repo.sum_allocated_quota(None).await?;
            // 已经越界时只拦「往上加」：磁盘占用会把上限压低，若连调低都拒绝，
            // 管理员就再也回不到合规状态了
            if projected > ceiling && projected > allocated_now {
                let gap = projected - ceiling;
                let cap_note = if capacity.host_used.is_some() {
                    "（系统总额减去磁盘占用）"
                } else {
                    ""
                };
                return Err(AppError::bad_request(format!(
                    "给 {} 分配 {} 后，所有用户配额合计将达 {}，超出可分配上限 {}{cap_note}，还差 {}（{} 字节）",
                    user.email,
                    human_bytes(quota_bytes),
                    human_bytes(projected),
                    human_bytes(ceiling),
                    human_bytes(gap),
                    gap
                )));
            }
        }
        if ceiling.is_none() {
            tracing::info!(
                user = %user.email,
                "可分配上限取不到（总额未知），单用户配额校验已跳过"
            );
        }

        self.repo.update_user_quota(user.id, quota_bytes).await?;

        let detail = serde_json::json!({
            "from": user.quota_bytes,
            "to": quota_bytes,
        });
        self.repo
            .insert_user_action(
                user.id,
                Some(admin_id),
                "quota_change",
                None,
                &detail,
                false,
                pixora_repo::model::now(),
            )
            .await?;
        self.audit_detail(
            actions::QUOTA_CHANGE,
            Some(admin_id),
            user.uid.clone(),
            detail,
            None,
        )
        .await;

        let allocated = self.repo.sum_allocated_quota(None).await?;
        let remaining = ceiling.map(|ceiling| (ceiling - allocated).max(0)).unwrap_or(0);
        Ok(UserQuotaChange {
            uid: user.uid,
            email: user.email,
            quota_bytes,
            allocated,
            remaining,
        })
    }

    /// 上传前的准入：`已用 + 本次 > 配额` 就拒绝，并把差额说清楚。
    ///
    /// 校验放在这里而不是上传流程里，是为了让「按张校验」与「批量预检」
    /// 走同一个判断——两处各写一遍，迟早有一套口径漂了。
    pub async fn ensure_quota_available(
        &self,
        user: &pixora_repo::model::User,
        incoming: i64,
    ) -> Result<()> {
        if incoming <= 0 {
            return Ok(());
        }
        let projected = user.used_bytes.saturating_add(incoming);
        if projected <= user.quota_bytes {
            return Ok(());
        }
        let missing = projected - user.quota_bytes;
        Err(AppError::QuotaExceeded {
            detail: format!(
                "还差 {}（{} 字节）：本次上传 {}，账号配额 {}，已用 {}",
                human_bytes(missing),
                missing,
                human_bytes(incoming),
                human_bytes(user.quota_bytes),
                human_bytes(user.used_bytes)
            ),
        })
    }
}

/// 上传前的准入，自由函数形态。
///
/// 上传流程（`uploads.rs`）是在「拿到真实大小」的那一刻调用的，那边手上已有
/// `AppState`，所以这里同时提供自由函数入口；两种形态共用同一份判断，
/// 不会出现两套口径。
pub async fn ensure_quota_available(
    state: &AppState,
    user: &pixora_repo::model::User,
    incoming: i64,
) -> Result<()> {
    state.ensure_quota_available(user, incoming).await
}

/// 用户摘要 → 分配明细行。
fn user_row(user: &pixora_repo::model::UserSummary) -> UserRow {
    UserRow {
        id: user.id,
        uid: user.uid.clone(),
        email: user.email.clone(),
        role: user.role.clone(),
        status: user.status.clone(),
        status_label: pixora_repo::model::status::label(&user.status).to_string(),
        quota_bytes: user.quota_bytes,
        used_bytes: user.used_bytes,
        image_count: user.image_count,
    }
}

/// 百分比保留一位小数。界面上的一位数就够了，多出来的位数只会让行宽跳动。
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// 可分配上限：系统总额刨掉宿主机整盘已用。
///
/// 磁盘上被别的数据先占住的那部分额度分不出去，自动模式下必须减掉；
/// 手填总额（可能只是给本项目定的预算、或 OSS 场景的估算）与网络挂载
/// 降级时拿不到可信的整盘数字，上限就退回总额本身。总额也为 `None`
/// （hidden 模式）时返回 `None`，调用方跳过校验并在日志里说明。
fn allocatable_ceiling(total: Option<i64>, host_used: Option<i64>) -> Option<i64> {
    total.map(|total| match host_used {
        Some(host_used) => (total - host_used).max(0),
        None => total,
    })
}

/// 字节数的人话表示，只给用户看的文案用。
///
/// 小于 1 KB 时按整数展示（`512 B` 比 `512.0 B` 干净），更大的量保留一位小数；
/// 负数与 0 一律归零——传进来的都是非负量，出现负值说明上游算错了，
/// 这里也不该展示出更难理解的 `-1.0 B`。
pub fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes <= 0 {
        return "0 B".to_string();
    }
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::defaults;
    use pixora_repo::db::DbDriver;
    use pixora_repo::model::status as user_status;
    use pixora_repo::NewUser;
    use std::sync::Arc;

    const GIB: i64 = 1024 * 1024 * 1024;

    /// 真实 SQLite + 本地目录的实例：设置读写、配额校验、审计都走真路径。
    async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let dsn = dir.path().join("pixora-quota.db");
        let config = crate::config::Config {
            bind: "127.0.0.1:0".parse().expect("监听地址"),
            db_driver: DbDriver::Sqlite,
            db_dsn: dsn.to_string_lossy().to_string(),
            db_max_connections: 4,
            data_dir: dir.path().join("data"),
            master_key: None,
            key_source: crate::config::KeySource::Missing,
            workers: 1,
            base_url: Some("https://img.example.com".into()),
            trust_proxy: 1,
            web_dir: None,
            accel_prefix: None,
            secure_cookies: false,
            dev_return_codes: false,
            log_level: "error".into(),
        };
        let state = AppState::bootstrap(config).await.expect("装配 AppState");
        (state, dir)
    }

    async fn add_user(state: &AppState, email: &str, uid: &str, quota_bytes: i64) -> i64 {
        state
            .repo
            .create_user(&NewUser {
                uid: uid.to_string(),
                email: email.to_string(),
                password_hash: "$argon2id$fake".to_string(),
                role: "user".to_string(),
                status: user_status::ACTIVE.to_string(),
                quota_bytes,
                email_verified: true,
            })
            .await
            .expect("建用户")
            .id
    }

    /// 操作人必须是真账号：`user_actions.actor_id` 与 `audit_logs.actor_id`
    /// 都有指向 `users(id)` 的外键，随便编一个 id 会让记录悄悄写不进去。
    /// 额度给 0，免得它挤占测试里的总额。
    async fn add_admin(state: &AppState, email: &str, uid: &str) -> i64 {
        state
            .repo
            .create_user(&NewUser {
                uid: uid.to_string(),
                email: email.to_string(),
                password_hash: "$argon2id$fake".to_string(),
                role: "admin".to_string(),
                status: user_status::ACTIVE.to_string(),
                quota_bytes: 0,
                email_verified: true,
            })
            .await
            .expect("建管理员")
            .id
    }

    #[test]
    fn human_bytes_formats_the_small_and_the_large() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(10 * 1024 * 1024 * 1024), "10.0 GB");
        // 负数不该被展示成「-1.0 B」这种读不懂的形状
        assert_eq!(human_bytes(-5), "0 B");
    }

    #[tokio::test]
    async fn quota_update_rejects_allocation_beyond_total_and_says_how_much_is_missing() {
        let (state, _dir) = test_state().await;
        let admin = add_admin(&state, "admin@example.com", "1000").await;
        add_user(&state, "chenyu@example.com", "1001", 10 * GIB).await;
        add_user(&state, "design@studio.io", "2044", 10 * GIB).await;

        // 已分配合计 20 GiB，手填总额只给 15 GiB：必须拒绝并报出差 5 GiB
        let err = state
            .quota_update(
                admin,
                QuotaInput {
                    capacity_mode: Some("manual".into()),
                    manual_total: Some(15 * GIB),
                    ..Default::default()
                },
            )
            .await
            .expect_err("超额分配必须被拒绝");
        assert_eq!(err.code(), "BAD_REQUEST");
        assert_eq!(err.status(), 400);
        let message = err.message();
        assert!(message.contains("还差"), "{message}");
        assert!(message.contains("5.0 GB"), "{message}");
        assert!(
            message.contains(&(5 * GIB).to_string()),
            "字节数也要给全：{message}"
        );

        // 拒绝时库里不能留下任何改动
        assert_eq!(state.settings.capacity_mode().await.expect("容量模式"), "auto");
        assert_eq!(state.manual_total_setting().await.expect("手填总额"), None);

        // 总额给够就存得进去，并且面板上能读到
        state
            .quota_update(
                admin,
                QuotaInput {
                    capacity_mode: Some("manual".into()),
                    manual_total: Some(25 * GIB),
                    ..Default::default()
                },
            )
            .await
            .expect("总额足够时应当保存");
        let payload = state.quota_payload().await.expect("配额面板");
        assert_eq!(payload.capacity.mode, "manual");
        assert_eq!(payload.capacity.total, Some(25 * GIB));
        assert_eq!(payload.allocated, 20 * GIB);
        assert_eq!(payload.remaining, 5 * GIB);
        assert!((payload.allocated_percent - 80.0).abs() < 0.01);

        // 改设置要写审计，且带变更前后
        let logs = state
            .list_audit(
                &pixora_repo::repo::audit::AuditFilter {
                    actor_id: Some(admin),
                    action: Some("settings_change".into()),
                    ..Default::default()
                },
                pixora_repo::PageRequest::default(),
            )
            .await
            .expect("审计日志");
        assert_eq!(logs.total, 1);
        assert_eq!(logs.items[0].detail["before"]["capacity_mode"], "auto");
        assert_eq!(logs.items[0].detail["after"]["capacity_mode"], "manual");
        assert_eq!(logs.items[0].detail["after"]["manual_total"], 25 * GIB);
    }

    #[tokio::test]
    async fn quota_update_validates_thresholds_defaults_and_mode() {
        let (state, _dir) = test_state().await;
        let admin = add_admin(&state, "admin@example.com", "1000").await;

        let err = state
            .quota_update(
                admin,
                QuotaInput {
                    auto_close_off: Some(95),
                    ..Default::default()
                },
            )
            .await
            .expect_err("恢复阈值不小于触发阈值必须被拒绝");
        assert!(err.message().contains("小于"), "{}", err.message());

        for bad in [101_i64, -1] {
            let err = state
                .quota_update(
                    admin,
                    QuotaInput {
                        auto_close_on: Some(bad),
                        ..Default::default()
                    },
                )
                .await
                .expect_err("百分比越界必须被拒绝");
            assert_eq!(err.code(), "BAD_REQUEST");
        }

        // 默认配额 10 GiB（内置默认）不能大于单用户上限
        let err = state
            .quota_update(
                admin,
                QuotaInput {
                    per_user_max: Some(GIB),
                    ..Default::default()
                },
            )
            .await
            .expect_err("默认配额超上限必须被拒绝");
        assert!(err.message().contains("上限"), "{}", err.message());

        let err = state
            .quota_update(
                admin,
                QuotaInput {
                    capacity_mode: Some("weird".into()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("未知容量模式必须被拒绝");
        assert!(err.message().contains("auto"), "{}", err.message());
    }

    #[tokio::test]
    async fn set_user_quota_checks_per_user_max_and_the_hard_total() {
        let (state, _dir) = test_state().await;
        let admin = add_admin(&state, "admin@example.com", "1000").await;
        let first = add_user(&state, "chenyu@example.com", "1001", 10 * GIB).await;
        add_user(&state, "design@studio.io", "2044", 10 * GIB).await;

        // 手填 25 GiB 总额、单用户上限 100 GiB
        state.settings.set(keys::CAPACITY_MODE, "manual").await.expect("容量模式");
        state
            .settings
            .set_integer(keys::CAPACITY_MANUAL_TOTAL, 25 * GIB)
            .await
            .expect("手填总额");
        state
            .settings
            .set_integer(keys::QUOTA_PER_USER_MAX, 100 * GIB)
            .await
            .expect("单用户上限");

        // 别的用户 10 GiB + 这个人 20 GiB = 30 GiB > 25 GiB，差 5 GiB
        let err = state
            .set_user_quota(admin, first, 20 * GIB)
            .await
            .expect_err("破总额必须被拒绝");
        assert_eq!(err.code(), "BAD_REQUEST");
        let message = err.message();
        assert!(message.contains("还差") && message.contains("5.0 GB"), "{message}");
        assert_eq!(
            state.repo.find_user_by_id(first).await.expect("查用户").expect("存在").quota_bytes,
            10 * GIB,
            "被拒绝时不能落库"
        );

        // 超过单用户上限同样拒绝
        let err = state
            .set_user_quota(admin, first, 101 * GIB)
            .await
            .expect_err("超单用户上限必须被拒绝");
        assert!(err.message().contains("上限"), "{}", err.message());

        // 15 GiB 正好把总额用满：允许，并且返回值要能直接刷新界面
        let change = state.set_user_quota(admin, first, 15 * GIB).await.expect("合法配额");
        assert_eq!(change.uid, "1001");
        assert_eq!(change.email, "chenyu@example.com");
        assert_eq!(change.quota_bytes, 15 * GIB);
        assert_eq!(change.allocated, 25 * GIB);
        assert_eq!(change.remaining, 0);
        assert_eq!(
            state.repo.find_user_by_id(first).await.expect("查用户").expect("存在").quota_bytes,
            15 * GIB
        );

        // 处置记录与审计各留一条
        let actions = state.repo.list_user_actions(first, 10).await.expect("处置记录");
        let quota_action = actions
            .iter()
            .find(|action| action.action == "quota_change")
            .expect("要留下配额变更记录");
        assert_eq!(quota_action.actor_id, Some(admin));
        assert_eq!(quota_action.detail["from"], 10 * GIB);
        assert_eq!(quota_action.detail["to"], 15 * GIB);
        let logs = state
            .list_audit(
                &pixora_repo::repo::audit::AuditFilter {
                    actor_id: Some(admin),
                    action: Some("quota_change".into()),
                    ..Default::default()
                },
                pixora_repo::PageRequest::default(),
            )
            .await
            .expect("审计日志");
        assert_eq!(logs.total, 1);
        assert_eq!(logs.items[0].detail["to"], 15 * GIB);
    }

    #[tokio::test]
    async fn quota_payload_explains_a_missing_total_instead_of_lying() {
        let (state, _dir) = test_state().await;
        add_user(&state, "chenyu@example.com", "1001", 10 * GIB).await;
        add_user(&state, "design@studio.io", "2044", 10 * GIB).await;
        state.settings.set(keys::CAPACITY_MODE, "manual").await.expect("容量模式");
        state
            .settings
            .set_integer(keys::CAPACITY_MANUAL_TOTAL, 25 * GIB)
            .await
            .expect("手填总额");

        let payload = state.quota_payload().await.expect("配额面板");
        assert_eq!(payload.allocated, 20 * GIB);
        assert_eq!(payload.allocatable, 25 * GIB, "手填总额时上限就是总额本身");
        assert_eq!(payload.remaining, 5 * GIB);
        assert_eq!(payload.default_quota_bytes, defaults::QUOTA_DEFAULT);
        assert_eq!(payload.per_user_max, defaults::QUOTA_PER_USER_MAX);
        assert!(payload.auto_close.enabled);
        assert_eq!(payload.auto_close.on, 90);
        assert_eq!(payload.auto_close.off, 80);
        assert!(payload.auto_close.register_open);
        assert!(payload.auto_close.closed_reason.is_none());
        assert_eq!(payload.top_users.len(), 2);
        assert_eq!(payload.top_users[0].status_label, "正常");
        assert_eq!(payload.top_users[0].image_count, 0);

        // hidden 模式没有总额：百分比与剩余给 0，并在说明里讲清楚
        state.settings.set(keys::CAPACITY_MODE, "hidden").await.expect("容量模式");
        let payload = state.quota_payload().await.expect("配额面板");
        assert_eq!(payload.capacity.total, None);
        assert_eq!(payload.allocated_percent, 0.0);
        assert_eq!(payload.remaining, 0);
        assert!(
            payload.capacity.note.contains("系统总额未知"),
            "{}",
            payload.capacity.note
        );
    }

    /// 可分配上限 = 系统总额 − 宿主机整盘已用（拿不到整盘数字时就是总额）。
    #[test]
    fn allocatable_ceiling_subtracts_host_usage_when_known() {
        assert_eq!(allocatable_ceiling(Some(1000), Some(600)), Some(400));
        assert_eq!(allocatable_ceiling(Some(1000), None), Some(1000));
        assert_eq!(allocatable_ceiling(None, Some(600)), None);
        // 磁盘占满：上限是 0，不是负数
        assert_eq!(allocatable_ceiling(Some(1000), Some(1200)), Some(0));
        assert_eq!(allocatable_ceiling(Some(1000), Some(1000)), Some(0));
    }

    /// 自动模式下校验按「总额 − 磁盘占用」判：上限以内能存，越过就拒绝，
    /// 并且把上限是怎么算出来的讲清楚。
    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn quota_ceiling_subtracts_host_usage_in_auto_mode() {
        let (state, _dir) = test_state().await;
        let admin = add_admin(&state, "admin@example.com", "1000").await;

        let payload = state.quota_payload().await.expect("配额面板");
        let total = payload.capacity.total.expect("自动模式有总额");
        let host_used = payload.capacity.host_used.expect("自动模式要给出宿主机整盘已用");
        assert_eq!(
            payload.allocatable,
            total - host_used,
            "同一个响应里上限必须等于总额减整盘已用"
        );
        assert!(payload.allocatable > 0, "测试机的磁盘不该是满的");

        // 探测与校验之间磁盘占用会微动（其它进程还在写盘），所以不追求「正好
        // 卡在上限」，留 1 GiB 余量，用例只验证两侧的判定方向
        const MARGIN: i64 = GIB;
        let ceiling = payload.allocatable;

        // 上限以内：放行（顺手改一个配额之外的阈值，把校验完整跑一遍）
        add_user(&state, "chenyu@example.com", "1001", ceiling - MARGIN).await;
        let saved = state
            .quota_update(
                admin,
                QuotaInput {
                    auto_close_on: Some(85),
                    ..Default::default()
                },
            )
            .await;
        assert!(saved.is_ok(), "上限以内不该被拒：{saved:?}");

        // 合计越过上限 1 GiB：拒绝，且说明上限是「总额减磁盘占用」
        add_user(&state, "design@studio.io", "2044", 2 * MARGIN).await;
        let err = state
            .quota_update(
                admin,
                QuotaInput {
                    auto_close_on: Some(84),
                    ..Default::default()
                },
            )
            .await
            .expect_err("超出可分配上限必须被拒绝");
        let message = err.message();
        assert!(message.contains("可分配上限"), "{message}");
        assert!(message.contains("磁盘占用"), "{message}");
    }

    /// 已经越界时（磁盘占用会把上限压低）「只减不增」的调整必须放行：
    /// 全拦的话管理员连调低配额都做不到，就再也回不到合规状态了。
    #[tokio::test]
    async fn over_allocated_state_allows_lowering_but_blocks_raising() {
        let (state, _dir) = test_state().await;
        let admin = add_admin(&state, "admin@example.com", "1000").await;
        let first = add_user(&state, "chenyu@example.com", "1001", 800).await;
        add_user(&state, "design@studio.io", "2044", 800).await;

        // 手填 1000 字节，合计 1600 已经越界——等价于磁盘被占满把上限压下来之后
        state.settings.set(keys::CAPACITY_MODE, "manual").await.expect("容量模式");
        state
            .settings
            .set_integer(keys::CAPACITY_MANUAL_TOTAL, 1000)
            .await
            .expect("手填总额");

        // 往上加：合计会到 1700，拒绝
        let err = state
            .set_user_quota(admin, first, 900)
            .await
            .expect_err("越界时加额度必须被拒绝");
        assert!(err.message().contains("可分配上限"), "{}", err.message());

        // 往下降：合计从 1600 到 1200，虽然仍越界但方向是对的，放行
        let lowered = state.set_user_quota(admin, first, 400).await.expect("只减不增要放行");
        assert_eq!(lowered.allocated, 1200);
        assert_eq!(lowered.remaining, 0, "还没降到上限以内，剩余仍是 0");

        // 继续降到正好用满：放行
        let fit = state.set_user_quota(admin, first, 200).await.expect("降到上限以内");
        assert_eq!(fit.allocated, 1000);
        assert_eq!(fit.remaining, 0);
    }

    #[tokio::test]
    async fn ensure_quota_available_reports_the_shortfall() {
        let (state, _dir) = test_state().await;
        let id = add_user(&state, "chenyu@example.com", "1001", 10 * GIB).await;
        let mut user = state.repo.find_user_by_id(id).await.expect("查用户").expect("存在");
        user.used_bytes = 9 * GIB;

        // 还差 1 GiB 时放行
        state
            .ensure_quota_available(&user, GIB)
            .await
            .expect("正好用满应当放行");

        let err = state
            .ensure_quota_available(&user, 2 * GIB)
            .await
            .expect_err("超出配额必须被拒绝");
        assert_eq!(err.code(), "QUOTA_EXCEEDED");
        assert_eq!(err.status(), 413);
        let detail = err.message();
        assert!(detail.contains("还差"), "{detail}");
        assert!(detail.contains("1.0 GB"), "{detail}");

        // 空文件（以及伪造的负数）不该触发校验
        state.ensure_quota_available(&user, 0).await.expect("零字节放行");
        state.ensure_quota_available(&user, -1).await.expect("负值按零处理");
    }
}
