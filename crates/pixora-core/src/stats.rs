//! 管理端统计聚合：存储面板、后台总览与注册开关。
//!
//! 管理端页面要的是「一次请求拿全」：存储页同时展示默认驱动、后端清单、容量、
//! 目录布局、分布与最近上传。逐块拼装会让同一个数字被算好几遍，还会出现
//! 「存储页和概览页对不上」这种说不清的口径分裂——所以这里一次取齐仓储的
//! 聚合结果，本文件只做形状转换与文案。
//!
//! 注册是否开放有两个来源：
//!   1. `register.enabled` 总开关（管理员显式关闭）；
//!   2. 容量触顶自动关闭——占用率触到 `register.auto_close_on` 时关，
//!      降到 `register.auto_close_off` 以下才恢复。
//!
//! 第二条必须带迟滞（记住「上一步是关着的」），否则占用在阈值附近抖动时
//! 注册入口会来回开关，注册页跟着闪。`AppState` 结构体不允许加字段，
//! 闩锁就落在设置表里（`register.auto_closed`），且只在状态真的翻转时写库。

use chrono::SecondsFormat;
use pixora_storage::names as driver_names;
use serde::Serialize;

use crate::error::Result;
use crate::settings::{defaults, keys};
use crate::AppState;

/// 「容量触顶自动关闭注册」的功能总开关。
///
/// 它和站点设置里的 `register.enabled`（注册总开关）是两件事：前者是策略，
/// 后者是入口。`settings::keys` 里没有这一项，而本次改动不允许改 `settings.rs`，
/// 所以键名就近定义——拼写与 `register.auto_close_on/off` 保持一致。
pub const KEY_AUTO_CLOSE_ENABLED: &str = "register.auto_close_enabled";

/// 自动关闭的闩锁：「上一步已经因为触顶关掉了」。只在翻转时写。
pub const KEY_AUTO_CLOSED: &str = "register.auto_closed";
/// 管理员手动重开过注册：容量恢复到恢复阈值以下之前，自动关闭不再插手。
/// 与 `auto_closed`（闩锁）成对：闩锁记「现在是不是我们关的」，这一项记
/// 「我们关过、但管理员又开回来了，别跟他抢」。
pub const KEY_AUTO_CLOSE_OVERRIDE: &str = "register.auto_close_override";

/// 「此刻的关闭是我们替他关的」。
///
/// 与闩锁成对存在：闩锁说明「现在是关闭态」，这一项说明「所以开关是我们翻的」。
/// 只有它立着时，容量恢复后才会把系统设置里的注册开关还回去——管理员自己关的
/// 不算，那是他的意思，容量恢复了也不该自动替他打开。
pub const KEY_AUTO_CLOSE_OWNED: &str = "register.auto_close_owned";

/// 注册关闭时面向访客的那句话。具体口径（占用多少、阈值多少、什么时候恢复）
/// 只在管理端出现，不上面向公众的页面。
pub const PUBLIC_CLOSED_REASON: &str = "注册入口已暂停，重新开放时间待定";

/// 自动关闭注册的配置：总开关 + 触发/恢复阈值。
#[derive(Debug, Clone, Copy)]
pub struct AutoClosePolicy {
    pub enabled: bool,
    pub on: i64,
    pub off: i64,
}

// ============================================================
// 契约 8.2：存储面板
// ============================================================

/// `GET /admin/storage` 的响应体。字段名与 `docs/api-contract.md` 8.2 定死一致。
#[derive(Debug, Clone, Serialize)]
pub struct StoragePayload {
    pub default_driver: String,
    pub backends: Vec<BackendView>,
    pub capacity: StorageCapacityView,
    pub paths: PathsView,
    pub distribution: Vec<DriverDistribution>,
    /// 回收站里还没过保留期的那部分（单独一行展示）
    pub binned: StorageTotals,
    /// 已经没人引用、等后台回收的那部分（单独一行展示）
    pub pending: StorageTotals,
    pub totals: StorageTotals,
    pub mimes: Vec<MimeView>,
    pub trend: Vec<TrendView>,
    pub hourly: Vec<TrendView>,
    pub recent: Vec<RecentImage>,
    /// 缩略图重算的进度（管理员点过「重算缩略图」之后才有意义）
    pub thumb_rebuild: ThumbRebuildView,
    pub average_size: i64,
    pub image_total: i64,
}

/// 后端实例行。`config` 只回显掩码后的结果：敏感字段是布尔（配没配），不是明文。
#[derive(Debug, Clone, Serialize)]
pub struct BackendView {
    pub id: i64,
    pub driver: String,
    pub label: String,
    pub is_default: bool,
    pub enabled: bool,
    pub capacity_mode: String,
    pub capacity_total: Option<i64>,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageCapacityView {
    pub mode: String,
    pub total: Option<i64>,
    /// 本项目占用（库内 blob 汇总）
    pub used: i64,
    /// 宿主机整盘已用与可用，只有 auto 且探测可信时才有
    pub host_used: Option<i64>,
    pub host_available: Option<i64>,
    pub source: String,
    /// 管理员上次手填的总额原值，切回手动模式时用来回显输入框
    pub manual_total: Option<i64>,
    pub note: String,
    pub probe: ProbeView,
}

/// 探测结果。`fs_type` 用十六进制文本，界面直接展示给管理员核对。
#[derive(Debug, Clone, Serialize)]
pub struct ProbeView {
    pub fs_kind: String,
    pub fs_name: String,
    pub fs_type: String,
    pub trustworthy: bool,
    pub note: String,
}

impl From<crate::storage_svc::CapacityProbeLike> for ProbeView {
    fn from(probe: crate::storage_svc::CapacityProbeLike) -> Self {
        Self {
            fs_kind: probe.fs_kind,
            fs_name: probe.fs_name,
            fs_type: probe.fs_type,
            trustworthy: probe.trustworthy,
            note: probe.note,
        }
    }
}

/// 运行时路径。数据目录只走环境变量（ADR-0003），这里如实转述来源。
#[derive(Debug, Clone, Serialize)]
pub struct PathsView {
    pub data_dir: String,
    pub upload_prefix: String,
    /// 人话模板，界面只读展示——字符串里的花括号是占位符，不是格式化串
    pub layout: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriverDistribution {
    pub driver: String,
    pub label: String,
    pub count: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageTotals {
    pub count: i64,
    pub bytes: i64,
    pub dedup_saved_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MimeView {
    pub mime: String,
    pub count: i64,
    pub bytes: i64,
}

/// 缩略图重算的进度。
#[derive(Debug, Clone, Serialize)]
pub struct ThumbRebuildView {
    pub running: bool,
    pub done: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrendView {
    pub bucket: String,
    pub count: i64,
}

/// 最近上传。形状对齐 `ImageItem`（契约 4.1），列表页复用同一套渲染。
#[derive(Debug, Clone, Serialize)]
pub struct RecentImage {
    pub id: i64,
    pub title: Option<String>,
    pub original_name: String,
    pub sha256: String,
    pub short_hash: String,
    pub ext: String,
    pub mime: String,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub album_id: Option<i64>,
    pub album_name: Option<String>,
    pub owner_uid: String,
    pub owner_id: i64,
    pub visibility: String,
    pub views: i64,
    pub upload_time: String,
    pub tags: Vec<String>,
    pub has_thumb: bool,
    pub url: String,
    pub thumb_url: Option<String>,
}

impl RecentImage {
    async fn from_view(state: &AppState, view: &pixora_repo::model::ImageView) -> Self {
        let url = state.direct_url(&view.sha256, &view.ext, false).await;
        // 只有真有缩略图才给地址：给了地址却没有图，前端会渲染成碎图
        let thumb_url = if view.has_thumb() {
            Some(state.direct_url(&view.sha256, &view.ext, true).await)
        } else {
            None
        };
        Self {
            id: view.id,
            title: view.title.clone(),
            original_name: view.original_name.clone(),
            sha256: view.sha256.clone(),
            short_hash: view.sha256.chars().take(8).collect(),
            ext: view.ext.clone(),
            mime: view.mime_type.clone(),
            size: view.size,
            width: view.width,
            height: view.height,
            album_id: view.album_id,
            album_name: view.album_name.clone(),
            owner_uid: view.owner_uid.clone(),
            owner_id: view.owner_id,
            visibility: view.visibility.clone(),
            views: view.views,
            upload_time: view
                .upload_time
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            tags: view.tags.clone(),
            has_thumb: view.has_thumb(),
            url,
            thumb_url,
        }
    }
}

// ============================================================
// 契约 8.1 末尾：后台总览
// ============================================================

/// `GET /admin/stats` 的响应体。
#[derive(Debug, Clone, Serialize)]
pub struct AdminOverview {
    pub user_total: i64,
    pub user_new_this_month: i64,
    pub allocated_quota: i64,
    /// 按用户账本汇总的口径（与用户列表页一致）
    pub used_bytes: i64,
    pub image_total: i64,
    /// 物理占用：同一份字节被多张图引用时只算一次
    pub storage_bytes: i64,
    pub register_open: bool,
}

// ============================================================
// 实现
// ============================================================

impl AppState {
    /// 缩略图重算的进度。三个数都在设置表里，读起来不碰像素。
    pub async fn thumb_rebuild_progress(&self) -> Result<ThumbRebuildView> {
        let total = self
            .setting_i64(crate::settings::keys::THUMB_REGEN_TOTAL)
            .await;
        let done = self
            .setting_i64(crate::settings::keys::THUMB_REGEN_DONE)
            .await;
        Ok(ThumbRebuildView {
            running: done < total,
            done,
            total,
        })
    }

    /// 读一个整数设置；缺失或不是数字都当 0。
    async fn setting_i64(&self, key: &str) -> i64 {
        self.settings
            .raw(key)
            .await
            .ok()
            .flatten()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 开始（或重头开始）一轮缩略图重算：记下总张数并把游标归零。
    ///
    /// 真正的重算在后台队列里分批做（`pixora-queue` 的缩略图任务），
    /// 所以这个接口立刻返回，界面轮询进度即可。
    pub async fn start_thumb_rebuild(&self) -> Result<ThumbRebuildView> {
        let total = self.repo.count_all_images().await?;
        self.settings
            .set(crate::settings::keys::THUMB_REGEN_CURSOR, "0")
            .await?;
        self.settings
            .set(crate::settings::keys::THUMB_REGEN_DONE, "0")
            .await?;
        self.settings
            .set(crate::settings::keys::THUMB_REGEN_TOTAL, &total.to_string())
            .await?;
        Ok(ThumbRebuildView {
            running: total > 0,
            done: 0,
            total: total.max(0),
        })
    }

    pub async fn storage_admin_payload(&self) -> Result<StoragePayload> {
        let backends = self.repo.list_backends().await?;
        let stats = self.repo.storage_stats(12).await?;
        let capacity = self.storage.capacity().await?;
        let site = self.settings.site(self.config.base_url.as_deref()).await?;

        // 默认键形如 `github#2`，界面上要的是驱动名本身
        let default_driver = match self.storage.registry().default_key() {
            Some(key) => key.split('#').next().unwrap_or(key.as_str()).to_string(),
            // 注册表没有默认键说明装配阶段就没配成；注册表此时兜底本地，这里保持一致
            None => driver_names::LOCAL.to_string(),
        };

        let data_dir = self.config.data_dir.display().to_string();
        let layout = format!(
            "{}/{{uid}}/{{相册}}/{{hash}}.{{ext}}",
            data_dir.trim_end_matches(['/', '\\'])
        );

        let mut recent = Vec::with_capacity(stats.recent.len());
        for view in &stats.recent {
            recent.push(RecentImage::from_view(self, view).await);
        }

        let thumb_rebuild = self.thumb_rebuild_progress().await?;

        Ok(StoragePayload {
            default_driver,
            backends: backends.iter().map(backend_view).collect(),
            capacity: StorageCapacityView {
                mode: capacity.mode,
                total: capacity.total,
                used: capacity.used,
                host_used: capacity.host_used,
                host_available: capacity.host_available,
                source: capacity.source,
                manual_total: self.manual_total_setting().await?,
                note: capacity.note,
                probe: capacity.probe.into(),
            },
            paths: PathsView {
                data_dir,
                upload_prefix: site.upload_prefix,
                layout,
                source: "PIXORA_DATA_DIR".to_string(),
            },
            distribution: stats
                .drivers
                .iter()
                .map(|row| DriverDistribution {
                    driver: row.driver.clone(),
                    label: driver_names::label(&row.driver).to_string(),
                    count: row.count,
                    bytes: row.bytes,
                })
                .collect(),
            binned: StorageTotals {
                count: stats.binned_count,
                bytes: stats.binned_bytes,
                dedup_saved_bytes: 0,
            },
            pending: StorageTotals {
                count: stats.pending_count,
                bytes: stats.pending_bytes,
                dedup_saved_bytes: 0,
            },
            totals: StorageTotals {
                count: stats.image_count,
                bytes: stats.total_bytes,
                dedup_saved_bytes: stats.dedup_saved_bytes,
            },
            mimes: stats
                .mimes
                .iter()
                .map(|row| MimeView {
                    mime: row.mime.clone(),
                    count: row.count,
                    bytes: row.bytes,
                })
                .collect(),
            trend: stats
                .trend
                .iter()
                .map(|point| TrendView {
                    bucket: point.bucket.clone(),
                    count: point.count,
                })
                .collect(),
            hourly: stats
                .hourly
                .iter()
                .map(|point| TrendView {
                    bucket: point.bucket.clone(),
                    count: point.count,
                })
                .collect(),
            recent,
            thumb_rebuild,
            average_size: stats.average_size,
            image_total: stats.image_count,
        })
    }

    /// 管理端首页的数字。`register_open` 由这里覆盖仓储的占位值——
    /// 仓储不读设置，注册开关的判定只在本层。
    pub async fn admin_overview(&self) -> Result<AdminOverview> {
        let stats = self.repo.admin_stats().await?;
        let (register_open, _) = self.register_open().await?;
        Ok(AdminOverview {
            user_total: stats.user_total,
            user_new_this_month: stats.user_new_this_month,
            allocated_quota: stats.allocated_quota,
            used_bytes: stats.used_bytes,
            image_total: stats.image_total,
            storage_bytes: stats.storage_bytes,
            register_open,
        })
    }

    /// 注册入口是否开放，以及关闭原因（开放时为 `None`）。
    ///
    /// 返回原因而不是布尔，是因为注册页要整页替换成说明文案：
    /// 「已注册用户不受影响、什么时候可能恢复」这两句必须由服务端给准话。
    pub async fn register_open(&self) -> Result<(bool, Option<String>)> {
        let enabled = self.settings.bool(keys::REGISTER_ENABLED, true).await?;
        let latched = self.settings.bool(KEY_AUTO_CLOSED, false).await?;
        // 开关关着、闩锁也没立：管理员自己关的，不用跑容量判定。
        // 闩锁立着时即便开关是关的也要跑一次——那种关闭是我们替他关的，
        // 占用恢复后要自动还回去
        if !enabled && !latched {
            return Ok((false, Some(PUBLIC_CLOSED_REASON.to_string())));
        }
        match self.auto_close_reason().await? {
            // 对外只说「暂停了」，不说为什么、也不承诺什么时候恢复：占用率、阈值
            // 这些是运维口径，摆在注册页上对访客没有任何用处，还会天天变。
            // 管理员要看的是详细那句，走 `auto_close_reason`（配额页）
            Some(_) => Ok((false, Some(PUBLIC_CLOSED_REASON.to_string()))),
            None => Ok((true, None)),
        }
    }

    /// 容量触顶自动关闭的当前判定。`None` 表示这项策略此刻不生效。
    ///
    /// 判定不生效的三种情形都不该拦人：功能被管理员关掉、拿不到系统总额
    /// （auto 探测不可信又没手填，没有分母就算不出占用率）、总额为 0。
    pub async fn auto_close_reason(&self) -> Result<Option<String>> {
        let policy = self.auto_close_policy().await?;
        if !policy.enabled {
            return Ok(None);
        }

        let capacity = self.storage.capacity().await?;
        let Some(total) = capacity.total.filter(|total| *total > 0) else {
            return Ok(None);
        };

        // 分子跟着总额的口径走：auto 模式下总额是磁盘容量，已用就取宿主机
        // 整盘口径（磁盘被别的东西写满，同样得停注册）；手填/降级时退回本项目占用
        let (basis, scope) = occupancy_basis(&capacity);
        let percent = occupancy_percent(basis, total);
        let latched = self.settings.bool(KEY_AUTO_CLOSED, false).await?;
        let overridden = self.settings.bool(KEY_AUTO_CLOSE_OVERRIDE, false).await?;
        let owned = self.settings.bool(KEY_AUTO_CLOSE_OWNED, false).await?;

        // 管理员手动动过注册开关：容量回到恢复阈值以下之前，这一层不插手。
        // 优先级最高的始终是管理员——自动关闭是他自己开的策略，不能反过来
        // 跟他抢开关
        if overridden {
            if percent > policy.off as f64 {
                return Ok(None);
            }
            // 占用恢复，覆盖功成身退。开关保持管理员当下的样子，我们不再碰它
            self.settings.set_bool(KEY_AUTO_CLOSE_OVERRIDE, false).await?;
            self.settings.set_bool(KEY_AUTO_CLOSED, false).await?;
            self.settings.set_bool(KEY_AUTO_CLOSE_OWNED, false).await?;
            return Ok(None);
        }

        let closed = auto_close_decision(percent, policy.on, policy.off, latched);
        if closed != latched {
            // 每次注册与每次打开管理页都会走到这里，只在翻转时落库
            self.settings.set_bool(KEY_AUTO_CLOSED, closed).await?;
            if closed {
                // 关：系统设置里那个注册开关跟着翻成关闭——管理员在设置页看到的
                // 必须与真实状态一致，否则「开关是开的、注册却关了」会让人以为
                // 前端坏了。记一笔「是我们关的」，恢复时才还回去
                self.settings.set_bool(keys::REGISTER_ENABLED, false).await?;
                self.settings.set_bool(KEY_AUTO_CLOSE_OWNED, true).await?;
            } else if owned {
                // 开：只还我们自己关过的那一次；管理员自己关的不算
                self.settings.set_bool(keys::REGISTER_ENABLED, true).await?;
                self.settings.set_bool(KEY_AUTO_CLOSE_OWNED, false).await?;
            }
            tracing::info!(
                percent,
                scope,
                on = policy.on,
                off = policy.off,
                closed,
                "容量占用变化，注册入口自动开关已切换"
            );
        }
        if !closed {
            return Ok(None);
        }
        Ok(Some(format!(
            "{scope}占用已达 {percent:.1}%（触发阈值 {}%），注册入口已自动关闭；\
             占用降到 {}% 以下会自动重新开放，已注册用户完全不受影响",
            policy.on, policy.off
        )))
    }

    /// 当前占用率（与自动关闭判定同一口径）。算不出时返回 `None`。
    ///
    /// 管理端要用它把「已超阈值」这句话说准；对外页面上不出现。
    pub async fn occupancy_percent_now(&self) -> Option<f64> {
        let capacity = self.storage.capacity().await.ok()?;
        let total = capacity.total.filter(|value| *value > 0)?;
        let (basis, _) = occupancy_basis(&capacity);
        Some(occupancy_percent(basis, total))
    }

    /// 自动关闭注册的配置。
    pub async fn auto_close_policy(&self) -> Result<AutoClosePolicy> {
        Ok(AutoClosePolicy {
            enabled: self.settings.bool(KEY_AUTO_CLOSE_ENABLED, true).await?,
            on: self
                .settings
                .integer(keys::REGISTER_AUTO_CLOSE_ON, defaults::AUTO_CLOSE_ON)
                .await?,
            off: self
                .settings
                .integer(keys::REGISTER_AUTO_CLOSE_OFF, defaults::AUTO_CLOSE_OFF)
                .await?,
        })
    }

    /// 手填的系统总额原值（不分模式）。
    ///
    /// `Settings::manual_total()` 只在 `manual` 模式下才认这一项，而配额页
    /// 要在任何模式下都能把管理员上次填的数字回显到输入框里。
    pub async fn manual_total_setting(&self) -> Result<Option<i64>> {
        let value = self.settings.integer(keys::CAPACITY_MANUAL_TOTAL, 0).await?;
        Ok(if value > 0 { Some(value) } else { None })
    }
}

/// 后端行 → 面板行。配置里的敏感字段只回显「配没配」。
fn backend_view(backend: &pixora_repo::model::StorageBackend) -> BackendView {
    BackendView {
        id: backend.id,
        driver: backend.driver.clone(),
        label: backend.label.clone(),
        is_default: backend.is_default,
        enabled: backend.enabled,
        capacity_mode: backend.capacity_mode.clone(),
        capacity_total: backend.capacity_total,
        config: crate::storage_svc::StorageService::mask_config(&backend.config),
    }
}

/// 占用率的分子，以及它属于哪个口径（错误信息里要讲清楚是谁占的）。
///
/// 自动模式且探测可信时总额就是磁盘容量，已用取宿主机整盘口径——磁盘满没满
/// 跟是谁占的无关，宿主机上别的数据同样能把盘写满。手填总额（可能只是给本项目
/// 定的预算）与网络挂载降级时拿不到整盘数字，只能按本项目占用算。
pub(crate) fn occupancy_basis(capacity: &crate::storage_svc::CapacityInfo) -> (i64, &'static str) {
    match capacity.host_used {
        Some(host_used) => (host_used, "宿主机磁盘"),
        None => (capacity.used, "本项目"),
    }
}

/// 占用率（百分比）。总额为 0 时分母不存在，按 0 算而不是报错——
/// 空盘上除以零不该让注册页打不开。
pub(crate) fn occupancy_percent(used: i64, total: i64) -> f64 {
    if total <= 0 {
        return 0.0;
    }
    used.max(0) as f64 / total as f64 * 100.0
}

/// 迟滞判定的核心：已经关着就等跌破恢复阈值，否则看是否触到关闭阈值。
///
/// 没有 `closed` 这个「上一步状态」，占用在 on/off 之间摆动时开关会来回翻。
fn auto_close_decision(percent: f64, on: i64, off: i64, closed: bool) -> bool {
    if closed {
        percent >= off as f64
    } else {
        percent >= on as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixora_repo::db::DbDriver;
    use pixora_repo::model::status as user_status;
    use pixora_repo::NewUser;
    use pixora_repo::repo::backends::NewBackend;
    use pixora_repo::repo::blobs::NewBlob;
    use std::sync::Arc;

    const GIB: i64 = 1024 * 1024 * 1024;

    /// 只跑 SQLite + 本地目录的真实实例：迁移、设置、容量探测都走真代码。
    async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let dsn = dir.path().join("pixora-core.db");
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
            samesite: crate::config::SameSite::Lax,
            dev_return_codes: false,
            log_level: "error".into(),
        };
        let state = AppState::bootstrap(config).await.expect("装配 AppState");
        (state, dir)
    }

    async fn add_user(state: &AppState, email: &str, uid: &str) {
        state
            .repo
            .create_user(&NewUser {
                uid: uid.to_string(),
                email: email.to_string(),
                password_hash: "$argon2id$fake".to_string(),
                role: "user".to_string(),
                status: user_status::ACTIVE.to_string(),
                quota_bytes: 10 * GIB,
                email_verified: true,
            })
            .await
            .expect("建用户");
    }

    async fn add_blob(state: &AppState, sha: &str, size: i64) -> i64 {
        let blob = state
            .repo
            .insert_blob_if_absent(&NewBlob {
                sha256: sha.to_string(),
                kind: "original".to_string(),
                size,
                mime_type: "image/png".to_string(),
                ext: "png".to_string(),
                width: None,
                height: None,
                storage_driver: "local".to_string(),
                storage_key: format!("blobs/{sha}.png"),
                backend_id: None,
            })
            .await
            .expect("登记 blob");
        blob.id
    }

    /// 手填 1000 字节总额，占用完全由入库的 blob 大小控制，比值可以精确到字节。
    async fn use_manual_capacity(state: &AppState, total: i64) {
        state
            .settings
            .set(keys::CAPACITY_MODE, "manual")
            .await
            .expect("容量模式");
        state
            .settings
            .set_integer(keys::CAPACITY_MANUAL_TOTAL, total)
            .await
            .expect("手填总额");
    }

    #[test]
    fn auto_close_decision_has_hysteresis_at_90_and_80() {
        // 开着的时候：正好触到 90% 就关
        assert!(auto_close_decision(90.0, 90, 80, false));
        assert!(!auto_close_decision(89.9, 90, 80, false));
        assert!(!auto_close_decision(0.0, 90, 80, false));

        // 关着的时候：停在 80–90 之间不动，跌破 80% 才恢复
        assert!(auto_close_decision(95.0, 90, 80, true));
        assert!(auto_close_decision(85.0, 90, 80, true));
        assert!(auto_close_decision(80.0, 90, 80, true), "「降到 80% 以下」不含 80% 本身");
        assert!(!auto_close_decision(79.9, 90, 80, true));
    }

    #[test]
    fn occupancy_percent_survives_zero_total() {
        assert_eq!(occupancy_percent(0, 0), 0.0);
        assert_eq!(occupancy_percent(5, 0), 0.0);
        assert!((occupancy_percent(900, 1000) - 90.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn register_open_follows_admin_switch_with_reason() {
        let (state, _dir) = test_state().await;
        // 这里只验管理员总开关。自动关闭的判定在自动模式下跟宿主机磁盘占用挂钩
        // （整盘有多满随机器变化），用它验「注册开关」会随环境飘，先把容量那条线
        // 换成手填模式：本项目占用为 0 就永远算 0%
        use_manual_capacity(&state, 1000 * GIB).await;

        let (open, reason) = state.register_open().await.expect("注册状态");
        assert!(open, "默认是开放的");
        assert!(reason.is_none());

        state
            .settings
            .set_bool(keys::REGISTER_ENABLED, false)
            .await
            .expect("关闭注册");
        let (open, reason) = state.register_open().await.expect("注册状态");
        assert!(!open);
        let reason = reason.expect("关闭必须带原因");
        // 对外只说「暂停 + 时间待定」：是管理员关的还是容量关的，不告诉访客
        assert!(reason.contains("暂停"), "{reason}");
        assert!(reason.contains("时间待定"), "{reason}");
    }

    #[tokio::test]
    async fn auto_close_latches_between_on_and_off_and_reopens_below_off() {
        let (state, _dir) = test_state().await;
        use_manual_capacity(&state, 1000).await;

        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(open, "还没有任何占用");

        // 900 / 1000 = 90.0%：正好触顶，关闭并给出原因
        let first = add_blob(&state, "a1", 900).await;
        let (open, reason) = state.register_open().await.expect("注册状态");
        assert!(!open, "占用 90% 应该自动关闭注册");
        let reason = reason.expect("自动关闭要带原因");
        // 对外那句话不带口径；详细口径只在管理端（配额页）出现
        assert!(reason.contains("时间待定"), "{reason}");
        let detail = state
            .auto_close_reason()
            .await
            .expect("详细原因")
            .expect("自动关闭时应当有详细原因");
        assert!(detail.contains("90"), "{detail}");
        assert!(detail.contains("本项目"), "手填总额时占用口径是本项目：{detail}");
        assert!(detail.contains("不受影响"), "说明要讲清已注册用户不受影响：{detail}");
        // 系统设置里的注册开关跟着翻成关闭：管理员在设置页看到的必须与真实状态一致
        assert!(
            !state
                .settings
                .bool(keys::REGISTER_ENABLED, true)
                .await
                .expect("读注册开关"),
            "自动关闭要顺手把系统设置里的注册开关关掉"
        );

        // 950 / 1000 = 95%：占用继续涨，仍然是关闭态
        let second = add_blob(&state, "a2", 50).await;
        assert!(!state.register_open().await.expect("注册状态").0);

        // 回到 900 / 1000 = 90%：正好在触发阈值上，迟滞让它保持关闭
        state.repo.delete_blob(second).await.expect("删 blob");
        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(!open, "还在阈值带内，不能一掉头就重开");

        // 正好 800 / 1000 = 80.0%：仍然是「没跌破」，保持关闭
        state.repo.delete_blob(first).await.expect("删 blob");
        let boundary = add_blob(&state, "a3", 800).await;
        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(!open, "正好 80% 不该恢复");

        // 799 / 1000 = 79.9%：跌破恢复阈值，自动重开
        state.repo.delete_blob(boundary).await.expect("删 blob");
        add_blob(&state, "a4", 799).await;
        let (open, reason) = state.register_open().await.expect("注册状态");
        assert!(open, "跌破恢复阈值要自动重开");
        assert!(reason.is_none());
        // 自动恢复时把开关还回去（当初是我们替他关的）
        assert!(
            state
                .settings
                .bool(keys::REGISTER_ENABLED, true)
                .await
                .expect("读注册开关"),
            "自动重开要把注册开关还原成打开"
        );
    }

    /// 管理员手开优先：自动关闭关掉注册之后，管理员重新打开注册，占用率
    /// 还挂在阈值以上也不该再被自动关回去；等他手开之后，那句「开关为什么
    /// 自己关了」的解释也就换成「策略暂时不生效」。
    #[tokio::test]
    async fn manual_reopen_beats_auto_close_until_capacity_recovers() {
        let (state, _dir) = test_state().await;
        use_manual_capacity(&state, 1000).await;
        let c1 = add_blob(&state, "c1", 950).await;

        // 占用 95%：自动关闭接管，开关被翻成关闭
        assert!(!state.register_open().await.expect("注册状态").0);
        assert!(!state
            .settings
            .bool(keys::REGISTER_ENABLED, true)
            .await
            .expect("读注册开关"));

        // 管理员手动打开：开关开了、注册也开了
        state
            .settings
            .set_bool(keys::REGISTER_ENABLED, true)
            .await
            .expect("手动打开注册");
        state
            .settings
            .set_bool(KEY_AUTO_CLOSE_OVERRIDE, true)
            .await
            .expect("立起覆盖标记");
        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(open, "管理员手开之后注册要开着");
        assert!(
            state.auto_close_reason().await.expect("自动关闭判定").is_none(),
            "管理员手开期间，自动关闭不再插手"
        );

        // 占用继续涨到 98%：仍然不抢开关
        let c2 = add_blob(&state, "c2", 30).await;
        assert!(
            state.register_open().await.expect("注册状态").0,
            "管理员手开优先：占用再高也不自动关回去"
        );

        // 占用回到 50%：覆盖标记功成身退，之后的判定重新生效
        state.repo.delete_blob(c2).await.expect("删 blob");
        state.repo.delete_blob(c1).await.expect("删 blob");
        add_blob(&state, "c4", 500).await;
        assert!(state.register_open().await.expect("注册状态").0);
        assert!(
            !state
                .settings
                .bool(KEY_AUTO_CLOSE_OVERRIDE, false)
                .await
                .expect("读覆盖标记"),
            "容量恢复之后覆盖标记要清掉"
        );
    }

    #[tokio::test]
    async fn auto_close_skips_when_total_is_unknown_or_feature_off() {
        let (state, _dir) = test_state().await;
        // hidden 模式没有总额 → 没有分母就算不出占用率，不参与判定
        state
            .settings
            .set(keys::CAPACITY_MODE, "hidden")
            .await
            .expect("容量模式");
        add_blob(&state, "b1", 900_000_000).await;
        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(open, "没有总额就没有占用率，不该拦注册");

        // 功能开关关掉后，即便占用触顶也不关注册
        use_manual_capacity(&state, 1000).await;
        state
            .settings
            .set_bool(KEY_AUTO_CLOSE_ENABLED, false)
            .await
            .expect("关闭自动关闭");
        let (open, _) = state.register_open().await.expect("注册状态");
        assert!(open, "功能关掉后只看管理员总开关");
    }

    /// 自动模式下占用率必须按宿主机整盘已用算：磁盘满没满跟是谁占的无关，
    /// 宿主机上别的数据同样能把盘写满，只盯着本项目那点字节会漏判。
    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn auto_mode_occupancy_uses_host_disk_usage() {
        let (state, _dir) = test_state().await;
        // 默认就是 auto；加 1 个字节，本项目占用可以忽略不计
        add_blob(&state, "c1", 1).await;

        let capacity = state.storage.capacity().await.expect("容量");
        assert_eq!(capacity.source, "auto", "本机（Linux/Windows）应能自动探测");
        let host_used = capacity.host_used.expect("自动模式要给出宿主机整盘已用");
        let host_available = capacity.host_available.expect("自动模式要给出宿主机可用");
        assert_eq!(
            host_used + host_available,
            capacity.total.expect("自动模式有总额"),
            "整盘口径：已用 + 可用 = 总额"
        );

        let (basis, scope) = occupancy_basis(&capacity);
        assert_eq!(scope, "宿主机磁盘");
        assert_eq!(basis, host_used, "占用率的分子要取整盘已用，而不是本项目那 1 字节");
        assert!(
            host_used > 1,
            "宿主机上不可能只有本项目这 1 字节（读到 {host_used}）"
        );
    }

    /// 手填总额时拿不到可信的整盘数字，占用率退回本项目占用——
    /// 手填的值可能只是给本项目定的预算，拿磁盘占用去比它会算出荒唐的百分比。
    #[tokio::test]
    async fn manual_mode_occupancy_falls_back_to_our_own_bytes() {
        let (state, _dir) = test_state().await;
        use_manual_capacity(&state, 1000).await;
        add_blob(&state, "d1", 900).await;

        let capacity = state.storage.capacity().await.expect("容量");
        assert!(capacity.host_used.is_none(), "手填总额时不该报告整盘数字");
        assert!(capacity.host_available.is_none());

        let (basis, scope) = occupancy_basis(&capacity);
        assert_eq!(scope, "本项目");
        assert_eq!(basis, 900);
        assert!((occupancy_percent(basis, 1000) - 90.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn storage_payload_masks_secrets_and_names_what_it_shows() {
        let (state, _dir) = test_state().await;
        state
            .repo
            .create_backend(&NewBackend {
                driver: "github".to_string(),
                label: "GitHub 仓库".to_string(),
                config: serde_json::json!({
                    "token": "v1.abc",
                    "owner": "example-org",
                    "repo": "img-bed",
                })
                .to_string(),
                is_default: false,
                enabled: true,
                capacity_mode: "auto".to_string(),
                capacity_total: None,
            })
            .await
            .expect("建后端");

        let payload = state.storage_admin_payload().await.expect("存储面板");
        assert_eq!(payload.default_driver, "local", "装配阶段兜底的是本地驱动");
        assert_eq!(payload.backends.len(), 1);
        assert_eq!(payload.backends[0].label, "GitHub 仓库");
        assert_eq!(
            payload.backends[0].config["token"],
            serde_json::Value::Bool(true),
            "敏感字段只能回显布尔"
        );
        assert_eq!(payload.backends[0].config["owner"], "example-org");
        assert!(payload.paths.upload_prefix.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'));
        assert!(payload.paths.layout.contains("{uid}"), "{}", payload.paths.layout);
        assert!(payload.paths.layout.contains("{hash}.{ext}"), "{}", payload.paths.layout);
        assert!(payload.paths.source.contains("PIXORA_DATA_DIR"), "{}", payload.paths.source);
        assert!(!payload.capacity.mode.is_empty());
        assert!(payload.distribution.is_empty(), "还没有图片");
        assert_eq!(payload.totals.count, 0);
        assert_eq!(payload.image_total, 0);
        assert!(payload.recent.is_empty());
    }

    #[tokio::test]
    async fn admin_overview_counts_users_and_reports_register_state() {
        let (state, _dir) = test_state().await;
        // register_open 受自动关闭影响，而自动模式下的判定看宿主机磁盘有多满
        // （随机器变化）；换成手填模式让这条线只反映管理员开关
        use_manual_capacity(&state, 1000 * GIB).await;
        add_user(&state, "chenyu@example.com", "1001").await;
        add_user(&state, "design@studio.io", "2044").await;

        let overview = state.admin_overview().await.expect("后台总览");
        assert_eq!(overview.user_total, 2);
        assert_eq!(overview.user_new_this_month, 2, "刚建的两个账号都算本月新增");
        assert_eq!(overview.allocated_quota, 20 * GIB);
        assert_eq!(overview.used_bytes, 0);
        assert_eq!(overview.image_total, 0);
        assert_eq!(overview.storage_bytes, 0);
        assert!(overview.register_open);

        state
            .settings
            .set_bool(keys::REGISTER_ENABLED, false)
            .await
            .expect("关闭注册");
        let closed = state.admin_overview().await.expect("后台总览");
        assert!(!closed.register_open, "总览要跟着注册开关走");
    }
}
