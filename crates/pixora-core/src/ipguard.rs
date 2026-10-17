//! IP 黑白名单与风控自动封禁。
//!
//! 规则条数不多（几十到几百），所以**匹配放在进程内做**，用 `ipnet` 判包含关系。
//! 这样三种数据库方言不必各写一套 CIDR 包含逻辑，热路径上也不查库——
//! 每次请求都要过这一关，查库会成为不必要的开销。规则缓存 30 秒，管理员改动时主动失效。
//!
//! 两条设计稿强调过的规则：
//!   - **白名单优先**：同一条地址同时命中 allow 与 deny 时，放行。
//!     管理员自己的出口 IP 加白之后，即使被风控误伤也能进去处理。
//!   - **只信最靠近服务端的那一层代理**：`X-Forwarded-For` 从右往左数第 N 跳，
//!     N 就是可信代理层数。信任全部转发头等于把 IP 黑名单的钥匙交给客户端。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use pixora_repo::model::IpRule;
use pixora_repo::Repo;

use crate::error::{AppError, Result};
use crate::settings::{keys, Settings};

/// 作用范围。
pub mod scope {
    pub const ALL: &str = "all";
    pub const REGISTER: &str = "register";
    pub const LOGIN: &str = "login";
    pub const UPLOAD: &str = "upload";
}

/// 策略设置。
#[derive(Debug, Clone)]
pub struct IpPolicy {
    pub enabled: bool,
    pub trust_proxy: bool,
    pub proxy_hops: usize,
    pub auto_ban: bool,
    pub fail_threshold: i64,
    pub fail_window_minutes: i64,
    pub ban_minutes: i64,
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            // 读不出设置时宁可不信任转发头：伪造 X-Forwarded-For 的代价
            // 比「代理后面的所有请求都记成代理 IP」更高。
            trust_proxy: false,
            proxy_hops: 0,
            auto_ban: true,
            fail_threshold: 20,
            fail_window_minutes: 10,
            ban_minutes: 30,
        }
    }
}

/// 拦截判定结果。
#[derive(Debug, Clone)]
pub struct IpDecision {
    pub blocked: bool,
    pub allowlisted: bool,
    pub matched: Option<IpRule>,
}

impl IpDecision {
    pub fn allowed() -> Self {
        Self {
            blocked: false,
            allowlisted: false,
            matched: None,
        }
    }
}

struct Cache {
    rules: Vec<IpRule>,
    loaded_at: Instant,
}

pub struct IpGuard {
    repo: Repo,
    settings: Settings,
    cache: RwLock<Option<Cache>>,
    /// 环境变量里给的默认值。后台设置没写过时用它，保证
    /// `PIXORA_TRUST_PROXY` 不是解析了却没人读的摆设。
    default_proxy_hops: usize,
    /// 规则命中次数的写库节流：同一规则最多每分钟记一次。
    /// 被拦截的客户端会不停重试，逐请求 UPDATE 等于把封禁变成写放大。
    hit_marks: Mutex<HashMap<i64, Instant>>,
}

const CACHE_TTL: Duration = Duration::from_secs(30);
const HIT_MARK_TTL: Duration = Duration::from_secs(60);

impl IpGuard {
    pub fn new(repo: Repo, settings: Settings, trust_proxy: usize) -> Self {
        Self {
            repo,
            settings,
            cache: RwLock::new(None),
            default_proxy_hops: trust_proxy.min(10),
            hit_marks: Mutex::new(HashMap::new()),
        }
    }

    /// 管理后台改完规则后调这个，下次请求立刻生效。
    pub fn invalidate(&self) {
        *self.cache.write().expect("IP 规则缓存不该中毒") = None;
    }

    pub async fn policy(&self) -> Result<IpPolicy> {
        let default_trust = self.default_proxy_hops > 0;
        Ok(IpPolicy {
            enabled: self.settings.bool(keys::IP_ENABLED, true).await?,
            trust_proxy: self
                .settings
                .bool(keys::IP_TRUST_PROXY, default_trust)
                .await?,
            proxy_hops: self
                .settings
                .integer(keys::IP_PROXY_HOPS, self.default_proxy_hops as i64)
                .await?
                .clamp(0, 10) as usize,
            auto_ban: self.settings.bool(keys::IP_AUTO_BAN, true).await?,
            fail_threshold: self
                .settings
                .integer(
                    keys::IP_FAIL_THRESHOLD,
                    crate::settings::defaults::IP_FAIL_THRESHOLD,
                )
                .await?,
            fail_window_minutes: 10,
            ban_minutes: self
                .settings
                .integer(
                    keys::IP_BAN_MINUTES,
                    crate::settings::defaults::IP_BAN_MINUTES,
                )
                .await?,
        })
    }

    /// 取出生效中的规则。带 30 秒缓存，避免每个请求都查库。
    async fn rules(&self) -> Result<Vec<IpRule>> {
        {
            let cache = self.cache.read().expect("IP 规则缓存不该中毒");
            if let Some(cache) = cache.as_ref() {
                if cache.loaded_at.elapsed() < CACHE_TTL {
                    return Ok(cache.rules.clone());
                }
            }
        }
        let rules = self.repo.active_rules(Utc::now()).await?;
        *self.cache.write().expect("IP 规则缓存不该中毒") = Some(Cache {
            rules: rules.clone(),
            loaded_at: Instant::now(),
        });
        Ok(rules)
    }

    /// 判断某个 IP 在某个场景下是否被拦截。
    ///
    /// `scope` 传 `all` / `register` / `login` / `upload` 之一：
    /// 「全站」规则的匹配所有场景，「仅注册」只拦注册。
    pub async fn check(&self, ip: Option<&str>, scene: &str) -> Result<IpDecision> {
        let Some(ip) = ip.filter(|value| !value.trim().is_empty()) else {
            return Ok(IpDecision::allowed());
        };
        let policy = self.policy().await?;
        if !policy.enabled {
            return Ok(IpDecision::allowed());
        }
        let Ok(address) = ip.parse::<IpAddr>() else {
            // IP 解析不出来就当没给出地址，不因为脏数据误伤
            return Ok(IpDecision::allowed());
        };

        let rules = self.rules().await?;
        let now = Utc::now();
        let mut matched_allow: Option<IpRule> = None;
        let mut matched_deny: Option<IpRule> = None;
        let mut hit_ids: Vec<i64> = Vec::new();

        for rule in &rules {
            if rule.is_expired(now) {
                continue;
            }
            // 作用范围：全站规则对所有场景生效，其余按场景匹配
            if rule.scope != scope::ALL && rule.scope != scene {
                continue;
            }
            let Ok(network) = rule.cidr.parse::<ipnet::IpNet>() else {
                tracing::warn!(cidr = %rule.cidr, "IP 规则里的网段无法解析，已跳过");
                continue;
            };
            if !network.contains(&address) {
                continue;
            }

            hit_ids.push(rule.id);
            if rule.is_allow() {
                matched_allow = Some(rule.clone());
            } else if matched_deny.is_none() {
                matched_deny = Some(rule.clone());
            }
        }

        // 命中次数累加（失败不影响判定）。节流到每分钟一次：被拦的客户端
        // 可能每秒重试几十次，逐请求写库只会把封禁变成新的压力源。
        let to_bump: Vec<i64> = {
            let mut marks = self.hit_marks.lock().expect("IP 命中节流表不该中毒");
            hit_ids
                .into_iter()
                .filter(|id| {
                    let fresh = marks
                        .get(id)
                        .map(|at| at.elapsed() >= HIT_MARK_TTL)
                        .unwrap_or(true);
                    if fresh {
                        marks.insert(*id, Instant::now());
                    }
                    fresh
                })
                .collect()
        };
        for id in to_bump {
            if let Err(err) = self.repo.bump_ip_rule_hits(id).await {
                tracing::warn!(rule_id = id, error = %err, "IP 规则命中计数失败");
            }
        }

        // 白名单优先：这条是防止管理员被自动风控锁在外面
        if let Some(allow) = matched_allow {
            return Ok(IpDecision {
                blocked: false,
                allowlisted: true,
                matched: Some(allow),
            });
        }
        if let Some(deny) = matched_deny {
            return Ok(IpDecision {
                blocked: true,
                allowlisted: false,
                matched: Some(deny),
            });
        }
        Ok(IpDecision::allowed())
    }

    /// 拦截检查的便捷封装：命中就返回错误。
    pub async fn ensure_allowed(&self, ip: Option<&str>, scene: &str) -> Result<()> {
        let decision = self.check(ip, scene).await?;
        if decision.blocked {
            return Err(AppError::IpBlocked);
        }
        Ok(())
    }

    /// 记一次失败（登录失败、上传失败），达到阈值就自动临时封禁。
    ///
    /// 临时规则带 `is_auto` 标记与到期时间，界面上可以「提前解除」。
    /// 返回 `Some(提示)` 表示这次触发了自动封禁。
    pub async fn record_failure(&self, ip: Option<&str>, action: &str) -> Result<Option<String>> {
        let Some(ip) = ip.filter(|value| !value.trim().is_empty()) else {
            return Ok(None);
        };
        let policy = self.policy().await?;
        if !policy.enabled || !policy.auto_ban {
            return Ok(None);
        }
        // 已经是白名单就别再自动封了——否则管理员照样会被锁在外面
        if let Ok(decision) = self.check(Some(ip), scope::ALL).await {
            if decision.allowlisted {
                return Ok(None);
            }
        }

        let since = Utc::now() - chrono::Duration::minutes(policy.fail_window_minutes.max(1));
        let hits = self
            .repo
            .count_action_for_ips(action, &[ip.to_string()], since)
            .await
            .unwrap_or(0);
        if hits < policy.fail_threshold {
            return Ok(None);
        }

        // 已有同网段的自动规则就不重复加
        if self
            .repo
            .find_active_rule(ip, "deny", Utc::now())
            .await?
            .is_some()
        {
            return Ok(None);
        }

        let expires = Utc::now() + chrono::Duration::minutes(policy.ban_minutes.max(1));
        let note = match action {
            "login_fail" => format!("登录失败 {hits} 次，自动封 {} 分钟", policy.ban_minutes),
            "upload_fail" => format!("上传失败 {hits} 次，自动封 {} 分钟", policy.ban_minutes),
            other => format!("{other} 触发 {hits} 次，自动封 {} 分钟", policy.ban_minutes),
        };

        self.repo
            .create_ip_rule(&pixora_repo::repo::iprules::NewIpRule {
                cidr: ip.to_string(),
                kind: "deny".into(),
                scope: scope::ALL.into(),
                note: Some(note.clone()),
                is_auto: true,
                expires_at: Some(expires),
                created_by: None,
            })
            .await?;
        self.invalidate();

        tracing::warn!(
            ip,
            action,
            hits,
            minutes = policy.ban_minutes,
            "触发自动临时封禁"
        );
        Ok(Some(note))
    }

    /// 从请求头里解析真实 IP。
    ///
    /// `X-Forwarded-For` 从右往左数第 `proxy_hops` 个才是客户端地址——
    /// 右边那些是可信代理自己加上的。设成不限制就等于让客户端随便伪造。
    pub fn client_ip(
        policy: &IpPolicy,
        forwarded_for: Option<&str>,
        real_ip: Option<&str>,
        peer: Option<&str>,
    ) -> Option<String> {
        if policy.trust_proxy {
            if let Some(chain) = forwarded_for.filter(|value| !value.trim().is_empty()) {
                let parts: Vec<&str> = chain
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect();
                let hops = policy.proxy_hops.min(parts.len().saturating_sub(1));
                let index = parts.len().saturating_sub(hops + 1);
                if let Some(candidate) = parts.get(index) {
                    if candidate.parse::<IpAddr>().is_ok() {
                        return Some(candidate.to_string());
                    }
                }
            }
            if let Some(real) = real_ip.filter(|value| !value.trim().is_empty()) {
                if real.parse::<IpAddr>().is_ok() {
                    return Some(real.to_string());
                }
            }
        }
        peer.map(str::to_string)
    }

    /// 规则列表，管理界面用。
    pub async fn list(
        &self,
        filter: &pixora_repo::repo::iprules::IpRuleFilter,
        limit: i64,
    ) -> Result<Vec<IpRule>> {
        Ok(self.repo.list_ip_rules(filter, limit).await?)
    }

    /// 清理过期规则（自动临时封禁到期的那些）。后台任务定期调。
    pub async fn purge_expired(&self) -> Result<u64> {
        let removed = self.repo.purge_expired_ip_rules(Utc::now()).await?;
        if removed > 0 {
            self.invalidate();
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(hops: usize, trust: bool) -> IpPolicy {
        IpPolicy {
            trust_proxy: trust,
            proxy_hops: hops,
            ..Default::default()
        }
    }

    #[test]
    fn forwarded_for_takes_the_closest_to_server_hop() {
        // 客户端伪造了 1.2.3.4，真实客户端是 203.0.113.18，
        // 代理（可信 1 层）在右边加了自己的地址
        let chain = "1.2.3.4, 203.0.113.18, 10.0.0.1";
        let ip = IpGuard::client_ip(&policy(1, true), Some(chain), None, Some("10.0.0.1"));
        assert_eq!(ip.as_deref(), Some("203.0.113.18"));
    }

    #[test]
    fn two_trusted_proxies_walk_two_hops() {
        let chain = "1.2.3.4, 203.0.113.18, 10.0.0.1, 10.0.0.2";
        let ip = IpGuard::client_ip(&policy(2, true), Some(chain), None, Some("10.0.0.2"));
        assert_eq!(ip.as_deref(), Some("203.0.113.18"));
    }

    #[test]
    fn untrusted_forwarded_header_is_ignored() {
        let ip = IpGuard::client_ip(
            &policy(0, false),
            Some("1.2.3.4"),
            Some("5.6.7.8"),
            Some("198.51.100.42"),
        );
        assert_eq!(
            ip.as_deref(),
            Some("198.51.100.42"),
            "不信任代理头时必须用连接对端地址"
        );
    }

    #[test]
    fn garbage_forwarded_values_fall_back() {
        let ip = IpGuard::client_ip(
            &policy(1, true),
            Some("not-an-ip"),
            None,
            Some("198.51.100.42"),
        );
        assert_eq!(ip.as_deref(), Some("198.51.100.42"));
    }

    #[test]
    fn real_ip_header_is_used_when_no_chain() {
        let ip = IpGuard::client_ip(
            &policy(1, true),
            None,
            Some("203.0.113.9"),
            Some("10.0.0.1"),
        );
        assert_eq!(ip.as_deref(), Some("203.0.113.9"));
    }
}
