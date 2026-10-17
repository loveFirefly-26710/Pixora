//! 频率限制。
//!
//! 两层结构（ADR-0005）：`rate_limit_policies` 里 `user_id` 为空的那行是全局默认，
//! 其余是用户级覆盖；管理员默认豁免。
//!
//! 计数分两套机制，各有各的道理：
//!
//!   - **分钟级**用进程内令牌桶。它只服务单实例的瞬时保护，够快也够省内存，
//!     不碰数据库，所以每次 API 调用都检查它不会有额外开销。
//!   - **小时级与「天」级**落 `rate_counters` 表。这类窗口天然需要跨请求累计，
//!     而且多实例部署时计数必须共享，否则额度会被放大成实例数的倍数——这是
//!     设计稿点名的第三个容易被绕过的点。
//!
//! 上传限额按「实际落盘的新文件」计数：去重跳过的文件不占额度。这条与内容寻址
//! 存储的设计咬合，否则用户批量传重复图会被莫名其妙限流。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Duration, Timelike, Utc};
use pixora_repo::model::{RateLimitPolicy, User};
use pixora_repo::Repo;

use crate::error::{AppError, Result};

/// 窗口计数的类型名，与 `rate_counters.kind` 列一致。
pub mod kind {
    pub const API_MINUTE: &str = "api_minute";
    pub const API_HOUR: &str = "api_hour";
    pub const API_READ_MINUTE: &str = "api_read_minute";
    pub const UPLOAD_MINUTE: &str = "upload_minute";
    pub const UPLOAD_HOUR: &str = "upload_hour";
    pub const UPLOAD_DAY: &str = "upload_day";
    pub const UPLOAD_BYTES_DAY: &str = "upload_bytes_day";
    /// 下载（带 `?download=1` 的直链请求）。登录用户按账号记，游客按 IP 记
    pub const DOWNLOAD_MINUTE: &str = "download_minute";
    pub const DOWNLOAD_DAY: &str = "download_day";
}

pub mod scope {
    pub const USER: &str = "user";
    pub const IP: &str = "ip";
    pub const TOKEN: &str = "token";
}

/// 一个令牌桶。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/**
 * 一次下载请求的主体：登录了就是账号，没登录就是那个 IP。
 *
 * 两类的额度分开配（游客在系统设置里，登录用户按限流策略、可单用户覆盖），
 * 但判定与计数是同一套机制：分钟级用进程内令牌桶，天级落 `rate_counters`。
 */
#[derive(Debug, Clone)]
pub enum DownloadActor {
    User(i64),
    Ip(String),
}

impl DownloadActor {
    fn scope(&self) -> &'static str {
        match self {
            Self::User(_) => scope::USER,
            Self::Ip(_) => scope::IP,
        }
    }

    fn subject(&self) -> String {
        match self {
            Self::User(id) => id.to_string(),
            Self::Ip(ip) => ip.clone(),
        }
    }

    fn bucket_key(&self) -> String {
        match self {
            Self::User(id) => format!("download:user:{id}"),
            Self::Ip(ip) => format!("download:ip:{ip}"),
        }
    }
}

/// 下载限额。`0` = 不限：游客那两格来自系统设置，登录用户来自限流策略。
#[derive(Debug, Clone, Copy)]
pub struct DownloadLimits {
    pub per_minute: i64,
    pub per_day: i64,
}

pub struct RateLimits {
    repo: Repo,
    /// key → 桶。键形如 `api:42`。
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimits {
    pub fn new(repo: Repo) -> Self {
        Self {
            repo,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// API 调用的准入检查。读接口有单独的额度，因为成本差得远。
    ///
    /// 返回 `Err(RateLimited)` 时上层直接回 429 并带 `Retry-After`。
    pub fn check_api(&self, user: &User, policy: &RateLimitPolicy, is_read: bool) -> Result<()> {
        if policy.unlimited || user.is_admin() {
            return Ok(());
        }

        let (per_minute, key_suffix) = if is_read {
            (policy.api_read_per_minute, "read")
        } else {
            (policy.api_per_minute, "write")
        };
        if per_minute <= 0 {
            return Ok(());
        }

        let key = format!("api:{key_suffix}:{}", user.id);
        // 桶容量取「每分钟额度」，瞬时并发上限用 burst 单独收紧
        let capacity = per_minute as f64;
        let refill_per_second = capacity / 60.0;

        let mut buckets = self.buckets.lock().expect("令牌桶表不该中毒");
        let now = Instant::now();
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: capacity,
            last_refill: now,
        });

        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_second).min(capacity);
        bucket.last_refill = now;

        if bucket.tokens < 1.0 {
            let wait = ((1.0 - bucket.tokens) / refill_per_second).ceil() as u64;
            return Err(AppError::RateLimited {
                retry_after: wait.max(1),
                detail: format!("API 调用超出额度（{per_minute} 次/分钟）"),
            });
        }
        bucket.tokens -= 1.0;
        Ok(())
    }

    /// 上传张数与流量的准入检查。按窗口查库计数。
    pub async fn check_upload<'a>(
        &self,
        user: &User,
        policy: &RateLimitPolicy,
        count: i64,
        bytes: i64,
    ) -> Result<()> {
        if policy.unlimited || user.is_admin() {
            return Ok(());
        }
        let now = Utc::now();

        if policy.upload_per_minute > 0 {
            let used = self
                .read(kind::UPLOAD_MINUTE, user.id, minute_window(now))
                .await?;
            if used + count > policy.upload_per_minute {
                return Err(AppError::RateLimited {
                    retry_after: seconds_to_next_minute(now),
                    detail: format!("上传张数超出额度（{} 张/分钟）", policy.upload_per_minute),
                });
            }
        }
        if policy.upload_per_hour > 0 {
            let used = self
                .read(kind::UPLOAD_HOUR, user.id, hour_window(now))
                .await?;
            if used + count > policy.upload_per_hour {
                return Err(AppError::RateLimited {
                    retry_after: seconds_to_next_hour(now),
                    detail: format!("上传张数超出额度（{} 张/小时）", policy.upload_per_hour),
                });
            }
        }
        if policy.upload_per_day > 0 {
            let used = self
                .read(kind::UPLOAD_DAY, user.id, day_window(now))
                .await?;
            if used + count > policy.upload_per_day {
                return Err(AppError::RateLimited {
                    retry_after: seconds_to_next_day(now),
                    detail: format!("上传张数超出额度（{} 张/天）", policy.upload_per_day),
                });
            }
        }
        if policy.upload_bytes_per_day > 0 {
            let used = self
                .read_bytes(kind::UPLOAD_BYTES_DAY, user.id, day_window(now))
                .await?;
            if used + bytes > policy.upload_bytes_per_day {
                return Err(AppError::RateLimited {
                    retry_after: seconds_to_next_day(now),
                    detail: format!(
                        "上传流量超出额度（{} 每天）",
                        human_bytes(policy.upload_bytes_per_day)
                    ),
                });
            }
        }
        Ok(())
    }

    /// 记一次上传消耗。**只对真正落盘的文件调用**——去重跳过的不该占额度。
    pub async fn record_upload(&self, user_id: i64, count: i64, bytes: i64) -> Result<()> {
        let now = Utc::now();
        for (kind_name, window) in [
            (kind::UPLOAD_MINUTE, minute_window(now)),
            (kind::UPLOAD_HOUR, hour_window(now)),
            (kind::UPLOAD_DAY, day_window(now)),
        ] {
            self.bump(kind_name, user_id, window, count, 0).await?;
        }
        if bytes > 0 {
            self.bump(kind::UPLOAD_BYTES_DAY, user_id, day_window(now), 0, bytes)
                .await?;
        }
        Ok(())
    }

    /// 记一次 API 调用。管理页的「实时用量」靠它。
    pub async fn record_api(&self, user_id: i64, is_read: bool) -> Result<()> {
        let now = Utc::now();
        let kind_name = if is_read {
            kind::API_READ_MINUTE
        } else {
            kind::API_MINUTE
        };
        self.bump(kind_name, user_id, minute_window(now), 1, 0)
            .await?;
        self.bump(kind::API_HOUR, user_id, hour_window(now), 1, 0)
            .await?;
        Ok(())
    }

    /// 鉴权通过后的 API 限流。
    ///
    /// 准入在进程内令牌桶里做（每个请求都要过，不能查库）；计数落库只为
    /// 管理页展示，浏览器翻页产生的读请求不记，免得把每次看图都变成两次写。
    /// Token 客户端与写操作仍然记账，管理员能看出脚本用了多少。
    pub async fn enforce_api(&self, user: &User, is_read: bool, record: bool) -> Result<()> {
        let policy = self.repo.effective_policy(user).await?;
        self.check_api(user, &policy, is_read)?;
        if record {
            if let Err(err) = self.record_api(user.id, is_read).await {
                tracing::debug!(error = %err, user_id = user.id, "API 用量计数写入失败");
            }
        }
        Ok(())
    }

    /// 读某用户某窗口的计数，给管理页展示。
    pub async fn counters_for(&self, user_id: i64) -> Result<WindowCounters> {
        let now = Utc::now();
        Ok(WindowCounters {
            api_minute: self
                .read(kind::API_MINUTE, user_id, minute_window(now))
                .await?,
            upload_minute: self
                .read(kind::UPLOAD_MINUTE, user_id, minute_window(now))
                .await?,
            upload_day: self
                .read(kind::UPLOAD_DAY, user_id, day_window(now))
                .await?,
        })
    }

    async fn read(&self, kind_name: &str, user_id: i64, window: String) -> Result<i64> {
        self.repo
            .read_counter(
                scope::USER,
                &user_id.to_string(),
                kind_name,
                parse_window(&window)?,
            )
            .await
            .map_err(AppError::from)
    }

    async fn read_bytes(&self, kind_name: &str, user_id: i64, window: String) -> Result<i64> {
        self.repo
            .read_counter_bytes(
                scope::USER,
                &user_id.to_string(),
                kind_name,
                parse_window(&window)?,
            )
            .await
            .map_err(AppError::from)
    }

    async fn bump(
        &self,
        kind_name: &str,
        user_id: i64,
        window: String,
        count: i64,
        bytes: i64,
    ) -> Result<()> {
        self.repo
            .bump_counter(
                scope::USER,
                &user_id.to_string(),
                kind_name,
                parse_window(&window)?,
                count,
                bytes,
            )
            .await?;
        Ok(())
    }

    /// 清掉过期的计数行。后台任务定期调。
    pub async fn purge(&self, older_than: DateTime<Utc>) -> Result<u64> {
        Ok(self.repo.purge_counters_before(older_than).await?)
    }
}

impl RateLimits {
    /// 下载准入：分钟级过令牌桶，天级查库。
    ///
    /// 分钟级用令牌桶是刻意的：直链下载常常一串并发过来，查库计数拦不住同一秒
    /// 里的爆发；天级必须落库，否则重启一次额度就回满了。
    pub async fn check_download(
        &self,
        actor: &DownloadActor,
        limits: DownloadLimits,
    ) -> Result<()> {
        let now = Utc::now();
        if limits.per_minute > 0 {
            let wait = self.take_bucket(&actor.bucket_key(), limits.per_minute as f64, 1.0);
            if wait > 0 {
                return Err(AppError::RateLimited {
                    retry_after: wait,
                    detail: format!("下载过于频繁（{} 次/分钟）", limits.per_minute),
                });
            }
        }
        if limits.per_day > 0 {
            let used = self
                .read_count(
                    actor.scope(),
                    &actor.subject(),
                    kind::DOWNLOAD_DAY,
                    day_window(now),
                )
                .await?;
            if used + 1 > limits.per_day {
                return Err(AppError::RateLimited {
                    retry_after: seconds_to_next_day(now),
                    detail: format!("今天的下载次数已用完（{} 次/天）", limits.per_day),
                });
            }
        }
        Ok(())
    }

    /// 记一次下载。放在真的把文件发出去之前——限的是「被取走多少」，
    /// 而不是「被请求多少」。
    pub async fn record_download(&self, actor: &DownloadActor) -> Result<()> {
        self.repo
            .bump_counter(
                actor.scope(),
                &actor.subject(),
                kind::DOWNLOAD_DAY,
                parse_window(&day_window(Utc::now()))?,
                1,
                0,
            )
            .await?;
        Ok(())
    }

    async fn read_count(
        &self,
        scope_name: &str,
        subject: &str,
        kind_name: &str,
        window: String,
    ) -> Result<i64> {
        self.repo
            .read_counter(scope_name, subject, kind_name, parse_window(&window)?)
            .await
            .map_err(AppError::from)
    }

    /// 取一个令牌。返回 0 表示放行，其余是要等的秒数。
    ///
    /// `per_minute` 既是桶容量（允许一次性用掉一整分钟的额度）也是补充速率，
    /// 这样「60 次/分钟」在用户感知里就是「一分钟六十次，别一直连着刷」。
    fn take_bucket(&self, key: &str, per_minute: f64, cost: f64) -> u64 {
        let capacity = per_minute.max(1.0);
        let rate = per_minute / 60.0;
        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last_refill = now;
        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            return 0;
        }
        let missing = cost - bucket.tokens;
        (missing / rate).ceil().max(1.0) as u64
    }
}

/// 管理页「实时用量」的一行。
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowCounters {
    pub api_minute: i64,
    pub upload_minute: i64,
    pub upload_day: i64,
}

// 窗口起点：分钟对齐、小时对齐、当天 0 点。三种方言都按文本存，
// 所以时间对齐在 Rust 里算好再入库，不依赖数据库的日期函数。
fn minute_window(at: DateTime<Utc>) -> String {
    at.with_second(0)
        .and_then(|value| value.with_nanosecond(0))
        .unwrap_or(at)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn hour_window(at: DateTime<Utc>) -> String {
    at.with_minute(0)
        .and_then(|value| value.with_second(0))
        .and_then(|value| value.with_nanosecond(0))
        .unwrap_or(at)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn day_window(at: DateTime<Utc>) -> String {
    at.date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or(at)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn parse_window(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|err| AppError::internal(format!("窗口起点时间无法解析：{err}")))
}

fn seconds_to_next_minute(at: DateTime<Utc>) -> u64 {
    60 - at.second() as u64
}

fn seconds_to_next_hour(at: DateTime<Utc>) -> u64 {
    let minutes = at.minute() as u64;
    let seconds = at.second() as u64;
    (59 - minutes) * 60 + (60 - seconds)
}

fn seconds_to_next_day(at: DateTime<Utc>) -> u64 {
    let next = at.date_naive().succ_opt().unwrap_or(at.date_naive());
    let midnight = next
        .and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or(at);
    (midnight - at).num_seconds().max(1) as u64
}

fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index < UNITS.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    format!("{value:.1} {}", UNITS[index])
}

/// 判断「连续超限要不要临时封禁」。阈值与时长来自设置。
pub fn should_temp_ban(hits: i64, threshold: i64) -> bool {
    threshold > 0 && hits >= threshold
}

/// 剩余时间的人话表示（错误详情里会用）。
pub fn describe_wait(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分钟", seconds.div_ceil(60))
    } else {
        format!("{} 小时", (seconds as f64 / 3600.0).ceil() as u64)
    }
}

/// 一天的开始，统计与窗口计算共用。
pub fn start_of_day(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
        .unwrap_or(at)
}

/// 近 N 天的起点。
pub fn days_ago(days: i64) -> DateTime<Utc> {
    Utc::now() - Duration::days(days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, hour, minute, second)
            .unwrap()
    }

    /// 令牌桶检查不碰数据库，所以这里给一个指向临时文件的空库就够。
    /// 真正落库的窗口计数由 tests/ 下的集成测试覆盖。
    async fn limits() -> RateLimits {
        let dir = std::env::temp_dir().join(format!("pixora-rl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let dsn = dir.join("dummy.db").to_string_lossy().to_string();
        let db = pixora_repo::Db::connect(pixora_repo::DbDriver::Sqlite, &dsn, 1)
            .await
            .expect("临时库");
        RateLimits::new(Repo::new(std::sync::Arc::new(db)))
    }

    #[test]
    fn windows_align_to_boundaries() {
        let now = at(15, 30, 45);
        assert_eq!(minute_window(now), "2026-10-14T15:30:00.000000Z");
        assert_eq!(hour_window(now), "2026-10-14T15:00:00.000000Z");
        assert_eq!(day_window(now), "2026-10-14T00:00:00.000000Z");
    }

    #[test]
    fn retry_after_points_at_next_boundary() {
        assert_eq!(seconds_to_next_minute(at(15, 30, 45)), 15);
        // 距离下一个整点还有 29 分 15 秒
        assert_eq!(seconds_to_next_hour(at(15, 30, 45)), 29 * 60 + 15);
        assert_eq!(seconds_to_next_day(at(15, 30, 45)), 8 * 3600 + 29 * 60 + 15);
    }

    #[tokio::test]
    async fn admin_is_exempt() {
        let limits = limits().await;
        let mut user = sample_user();
        user.role = "admin".into();
        let policy = RateLimitPolicy::defaults();
        assert!(limits.check_api(&user, &policy, false).is_ok());
    }

    #[tokio::test]
    async fn token_bucket_throttles_after_quota() {
        let limits = limits().await;
        let user = sample_user();
        let mut policy = RateLimitPolicy::defaults();
        policy.api_per_minute = 3;

        for _ in 0..3 {
            limits
                .check_api(&user, &policy, false)
                .expect("前三次应通过");
        }
        let error = limits.check_api(&user, &policy, false).unwrap_err();
        assert_eq!(error.code(), "RATE_LIMITED");
        assert!(error.retry_after().unwrap() >= 1);
        assert!(error.message().contains("3 次/分钟"));
    }

    #[tokio::test]
    async fn read_endpoints_have_their_own_bucket() {
        let limits = limits().await;
        let user = sample_user();
        let mut policy = RateLimitPolicy::defaults();
        policy.api_per_minute = 1;
        policy.api_read_per_minute = 5;

        limits.check_api(&user, &policy, false).unwrap();
        assert!(
            limits.check_api(&user, &policy, false).is_err(),
            "写接口已用尽"
        );
        for _ in 0..5 {
            limits
                .check_api(&user, &policy, true)
                .expect("读接口有自己的额度");
        }
        assert!(limits.check_api(&user, &policy, true).is_err());
    }

    #[test]
    fn temp_ban_threshold() {
        assert!(should_temp_ban(10, 10));
        assert!(!should_temp_ban(9, 10));
        assert!(!should_temp_ban(100, 0), "阈值为 0 表示不启用");
    }

    #[test]
    fn wait_description_is_readable() {
        assert_eq!(describe_wait(30), "30 秒");
        assert_eq!(describe_wait(90), "2 分钟");
        assert_eq!(describe_wait(7200), "2 小时");
    }

    fn sample_user() -> User {
        User {
            id: 42,
            uid: "1001".into(),
            email: "chenyu@example.com".into(),
            email_verified: true,
            password_hash: String::new(),
            role: "user".into(),
            status: "active".into(),
            status_reason: None,
            status_changed_at: None,
            status_changed_by: None,
            totp_secret: None,
            totp_enabled: false,
            quota_bytes: 1024,
            used_bytes: 0,
            login_fail_count: 0,
            locked_until: None,
            auto_unsuspend_at: None,
            must_change_password: false,
            avatar: None,
            created_at: Utc::now(),
            last_login_at: None,
            last_login_ip: None,
        }
    }
}
