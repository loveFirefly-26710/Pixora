//! 出网代理解析。
//!
//! reqwest 默认只读 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 这类环境变量，
//! 不会读 Windows「Internet 选项」里的系统代理。可用户在本机开代理时，改的
//! 通常就是那一份系统设置：浏览器能打开 Google，服务进程却直连超时。这里把
//! 同一份用户级设置读出来，显式交给共享 HTTP 客户端。
//!
//! 优先级：`PIXORA_HTTP_PROXY` > `HTTPS_PROXY` > `ALL_PROXY` > Windows 系统代理。
//! `HTTP_PROXY` 只覆盖 http 目标，而 OAuth 端点都是 https，所以不拿它顶替。

use serde::Serialize;

/// 一次启动内解析出的代理配置。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProxyInfo {
    /// 实际交给 reqwest 的地址，可能带凭据；不直接回给前端
    #[serde(skip)]
    pub url: Option<String>,
    /// 后台展示用：去掉凭据与路径，只留 host:port
    pub display: Option<String>,
    /// 来源，例如「Windows 系统代理」
    pub source: Option<String>,
    /// 合并后的 no_proxy 规则
    #[serde(skip)]
    pub no_proxy: Option<String>,
}

impl ProxyInfo {
    pub fn configured(&self) -> bool {
        self.url.is_some()
    }
}

/// 启动时解析一次。代理设置或环境变量变了要重启进程才会重新读取。
pub fn detect() -> ProxyInfo {
    let (url, source, system_bypass) = detect_url();
    let Some(url) = url else {
        return ProxyInfo::default();
    };
    let no_proxy = build_no_proxy(
        std::env::var("NO_PROXY").ok().as_deref(),
        std::env::var("no_proxy").ok().as_deref(),
        system_bypass.as_deref(),
    );
    ProxyInfo {
        display: Some(display_of(&url)),
        url: Some(url),
        source: Some(source.to_string()),
        no_proxy: Some(no_proxy),
    }
}

fn detect_url() -> (Option<String>, &'static str, Option<String>) {
    if let Some(value) = non_empty_env("PIXORA_HTTP_PROXY") {
        return (Some(value), "PIXORA_HTTP_PROXY", None);
    }
    if let Some(value) = non_empty_env("HTTPS_PROXY").or_else(|| non_empty_env("https_proxy")) {
        return (Some(value), "HTTPS_PROXY 环境变量", None);
    }
    if let Some(value) = non_empty_env("ALL_PROXY").or_else(|| non_empty_env("all_proxy")) {
        return (Some(value), "ALL_PROXY 环境变量", None);
    }
    #[cfg(windows)]
    if let Some((value, bypass)) = windows_system_proxy() {
        return (Some(value), "Windows 系统代理", Some(bypass));
    }
    (None, "", None)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 合并三处排除规则，并始终放过本机回环：远端存储与本机调试都靠它。
fn build_no_proxy(
    no_proxy: Option<&str>,
    no_proxy_lower: Option<&str>,
    system_bypass: Option<&str>,
) -> String {
    let mut rules: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    for raw in [no_proxy, no_proxy_lower, system_bypass]
        .into_iter()
        .flatten()
    {
        for entry in raw.split(|ch| ch == ',' || ch == ';') {
            let entry = normalize_no_proxy_entry(entry);
            if !entry.is_empty() && !rules.iter().any(|rule| rule.eq_ignore_ascii_case(&entry)) {
                rules.push(entry);
            }
        }
    }
    rules.join(",")
}

/// Windows 的 `ProxyOverride` 用分号分隔，还允许 `127.*` 这种 IPv4 通配。
/// `NoProxy` 认 CIDR，不认星号，这里换算一下；`*.example.com` 去掉星号即可。
fn normalize_no_proxy_entry(raw: &str) -> String {
    let entry = raw.trim();
    if entry.is_empty() || entry == "<local>" {
        return String::new();
    }
    let entry = entry.strip_prefix("*.").unwrap_or(entry);
    ipv4_wildcard_to_cidr(entry).unwrap_or_else(|| entry.to_string())
}

fn ipv4_wildcard_to_cidr(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split('.').collect();
    let wildcard = parts.iter().position(|part| *part == "*")?;
    if wildcard == 0 || wildcard > 3 || parts[wildcard..].iter().any(|part| *part != "*") {
        return None;
    }
    let mut octets = [0u8; 4];
    for (index, part) in parts[..wildcard].iter().enumerate() {
        octets[index] = part.parse().ok()?;
    }
    Some(format!(
        "{}.{}.{}.{}/{}",
        octets[0],
        octets[1],
        octets[2],
        octets[3],
        wildcard * 8
    ))
}

/// 代理地址可能带用户名密码，日志和后台只展示主机端口。
fn display_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest
        .split(|ch| ch == '/' || ch == '?' || ch == '#')
        .next()
        .unwrap_or(rest);
    authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_string()
}

/// `ProxyServer` 有两种写法：`127.0.0.1:7899`，或
/// `http=127.0.0.1:7899;https=127.0.0.1:7899`。https 优先，其次 http。
fn pick_proxy_server(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.contains('=') {
        return Some(raw.to_string());
    }
    let mut http = None;
    let mut https = None;
    for part in raw.split(';') {
        let Some((scheme, host)) = part.split_once('=') else {
            continue;
        };
        let host = host.trim();
        if host.is_empty() {
            continue;
        }
        if scheme.trim().eq_ignore_ascii_case("https") {
            https = Some(host.to_string());
        } else if scheme.trim().eq_ignore_ascii_case("http") {
            http = Some(host.to_string());
        }
    }
    https.or(http)
}

#[cfg(windows)]
fn windows_system_proxy() -> Option<(String, String)> {
    use windows_registry::CURRENT_USER;

    let key = CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if key.get_u32("ProxyEnable").unwrap_or(0) == 0 {
        return None;
    }
    let server = key.get_string("ProxyServer").ok()?;
    let url = pick_proxy_server(&server)?;
    let bypass = key.get_string("ProxyOverride").unwrap_or_default();
    Some((url, bypass))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_https_entry_from_windows_proxy_server() {
        let raw = "http=10.0.0.2:7890;https=10.0.0.3:7891;socks=10.0.0.4:1080";
        assert_eq!(pick_proxy_server(raw).as_deref(), Some("10.0.0.3:7891"));
    }

    #[test]
    fn falls_back_to_http_entry_when_https_missing() {
        let raw = "http=10.0.0.2:7890;socks=10.0.0.4:1080";
        assert_eq!(pick_proxy_server(raw).as_deref(), Some("10.0.0.2:7890"));
    }

    #[test]
    fn accepts_bare_host_and_port() {
        assert_eq!(
            pick_proxy_server(" 127.0.0.1:7899 ").as_deref(),
            Some("127.0.0.1:7899")
        );
        assert_eq!(pick_proxy_server(""), None);
    }

    #[test]
    fn hides_credentials_in_display() {
        assert_eq!(
            display_of("http://alice:secret@127.0.0.1:7899/"),
            "127.0.0.1:7899"
        );
        assert_eq!(display_of("127.0.0.1:7899"), "127.0.0.1:7899");
    }

    #[test]
    fn normalizes_windows_bypass_list() {
        let rules = build_no_proxy(
            None,
            None,
            Some("localhost;127.*;10.*;*.example.com;<local>"),
        );
        assert!(rules.contains("127.0.0.0/8"));
        assert!(rules.contains("10.0.0.0/8"));
        assert!(rules.contains("example.com"));
        assert!(!rules.contains("<local>"));
    }

    #[test]
    fn keeps_loopback_when_no_proxy_is_set() {
        let rules = build_no_proxy(Some("example.com,127.0.0.1"), None, None);
        assert!(rules.contains("localhost"));
        assert!(rules.contains("127.0.0.1"));
        assert!(rules.contains("::1"));
        assert!(rules.contains("example.com"));
        assert_eq!(rules.matches("127.0.0.1").count(), 1);
    }
}
