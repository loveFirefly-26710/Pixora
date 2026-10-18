//! 广场背景音乐。
//!
//! 两种来源：
//!   - **API**：Meting 风格的 `?server=…&type=playlist&id=…` 接口。管理员选平台、
//!     填歌单 ID 就能用，不需要把音频文件放到服务器上。默认走这条。
//!   - **本地**：扫描管理员配置的目录，音频文件直接由服务端带 Range 提供；
//!     同名 `.lrc` 与封面图会一起带上。
//!
//! 歌单只在服务端拉一次并归一化成统一结构，浏览器不直接碰第三方 API：
//! 既避开 CORS，也不把 API 地址与平台参数散到前端。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use reqwest::Url;
use serde::Serialize;

use crate::error::{AppError, Result};
use crate::AppState;

/// 管理后台可选的 Meting 平台。Meting 还支持更多，保留常用的这些。
pub const PLATFORMS: &[&str] = &[
    "netease", "tencent", "kugou", "kuwo", "migu", "baidu", "xiami", "joox",
];

const MAX_TRACKS: usize = 500;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_LYRICS_BYTES: usize = 512 * 1024;
const MAX_SCAN_DEPTH: usize = 3;
/// 第三方音乐接口与 CDN 经常对默认客户端 UA 更敏感；用常见的浏览器标识。
pub const MUSIC_REQUEST_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "aac", "flac", "ogg", "opus", "wav", "webm"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "avif", "gif"];

#[derive(Debug, Clone, Serialize)]
pub struct MusicTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// 秒；API 给了就填，没给让前端从音频元数据里补
    pub duration_seconds: i64,
    pub cover_url: Option<String>,
    pub audio_url: String,
    /// 需要服务端代理的歌词地址（Meting 的 lrc 或本地 .lrc 的相对路径）
    pub lyric_url: Option<String>,
    /// API 直接返回的歌词正文（有些 Meting 实例会内联）
    pub lyrics: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MusicPlaylist {
    pub enabled: bool,
    pub source: String,
    pub tracks: Vec<MusicTrack>,
    /// 配置不完整或第三方接口失败时的说明。前端把它显示在歌单侧栏里，
    /// 不要因为管理员少填一个字段就让整个页面 500。
    pub message: Option<String>,
}

impl AppState {
    /// 本地音乐根目录：环境变量优先，其次后台填的目录。
    /// 扫描歌单、读音频文件、读歌词三条路必须用同一个根，否则 Docker 下
    /// 歌单能列出来、点播放却找不到文件。
    fn local_music_root(&self, settings: &crate::settings::MusicSettings) -> Option<PathBuf> {
        self.config.music_dir.clone().or_else(|| {
            let dir = settings.local_dir.trim();
            if dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(dir))
            }
        })
    }

    /// 把第三方媒体地址签成站内代理地址。浏览器只请求同源的 `/music/*`，
    /// 既避开混合内容（Meting 常返回 http 音频）与 CORS，也防止这个端点被拿去做
    /// 任意 URL 代理：只有服务端从管理员配置的 API 响应里签过的地址才能取。
    fn signed_media_url(&self, endpoint: &str, raw: &str, kind: &str) -> Result<String> {
        let payload = serde_json::json!({
            "url": raw,
            "kind": kind,
            "exp": (Utc::now() + ChronoDuration::minutes(720)).timestamp(),
        })
        .to_string();
        let token = self.settings.sign_payload(&payload)?;
        let mut url = Url::parse(&format!("http://music.local{endpoint}")).expect("静态地址");
        url.query_pairs_mut().append_pair("token", &token);
        Ok(format!("{}?{}", url.path(), url.query().unwrap_or_default()))
    }

    pub async fn music_playlist(&self) -> Result<MusicPlaylist> {
        let settings = self.settings.music().await?;
        if !settings.enabled {
            return Ok(MusicPlaylist {
                enabled: false,
                source: settings.source,
                tracks: Vec::new(),
                message: None,
            });
        }

        if settings.source == "local" {
            // Docker 里不能填宿主机绝对路径：把宿主机目录挂到容器内，
            // 再用 PIXORA_MUSIC_DIR 指定容器内路径；它优先于后台输入。
            let from_env = self.config.music_dir.is_some();
            let Some(root) = self.local_music_root(&settings) else {
                return Ok(MusicPlaylist {
                    enabled: true,
                    source: "local".into(),
                    tracks: Vec::new(),
                    message: Some("管理员还没有填写本地音乐目录".into()),
                });
            };
            let tracks = tokio::task::spawn_blocking(move || scan_local(&root))
                .await
                .map_err(|err| AppError::internal(format!("音乐目录扫描失败：{err}")))?;
            let message = if tracks.is_empty() {
                Some(if from_env {
                    "PIXORA_MUSIC_DIR 指向的目录里没有可播放的音频文件".into()
                } else {
                    "音乐目录里没有可播放的音频文件".into()
                })
            } else {
                None
            };
            return Ok(MusicPlaylist {
                enabled: true,
                source: "local".into(),
                tracks,
                message,
            });
        }

        if settings.api_urls.is_empty() || settings.playlist_id.trim().is_empty() {
            return Ok(MusicPlaylist {
                enabled: true,
                source: "api".into(),
                tracks: Vec::new(),
                message: Some("管理员还没有填音乐 API 地址或歌单 ID".into()),
            });
        }
        match self
            .fetch_api_tracks(&settings.api_urls, &settings.platform, &settings.playlist_id)
            .await
        {
            Ok(tracks) => Ok(MusicPlaylist {
                enabled: true,
                source: "api".into(),
                tracks,
                message: None,
            }),
            Err(err) => Ok(MusicPlaylist {
                enabled: true,
                source: "api".into(),
                tracks: Vec::new(),
                message: Some(err.message().to_string()),
            }),
        }
    }

    async fn fetch_api_tracks(
        &self,
        api_urls: &[String],
        platform: &str,
        playlist_id: &str,
    ) -> Result<Vec<MusicTrack>> {
        let platform = if PLATFORMS.contains(&platform) {
            platform
        } else {
            "netease"
        };
        // 顺序由后台列表决定，第一项是主 API；重复地址只请求一次。
        let mut sources: Vec<&str> = Vec::new();
        for url in api_urls {
            let url = url.trim();
            if !url.is_empty() && !sources.iter().any(|existing| *existing == url) {
                sources.push(url);
            }
        }
        if sources.is_empty() {
            return Err(AppError::Config("管理员还没有填音乐 API 地址".into()));
        }

        let multiple = sources.len() > 1;
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let attempt_cap = if multiple {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(12)
        };
        let mut last_error: Option<String> = None;
        for source in sources {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining < Duration::from_millis(200) {
                break;
            }
            let timeout = remaining.min(attempt_cap);
            match self
                .fetch_api_source(source, platform, playlist_id, timeout)
                .await
            {
                Ok(tracks) if !tracks.is_empty() => return Ok(tracks),
                Ok(_) => last_error = Some("歌单没有返回可播放的曲目".into()),
                Err(err) => last_error = Some(err.message().to_string()),
            }
        }
        let prefix = if multiple {
            "主 API 与备用 API 都没有拿到曲目"
        } else {
            "音乐 API 没有拿到曲目"
        };
        Err(AppError::Config(match last_error {
            Some(message) => format!("{prefix}：{message}"),
            None => prefix.to_string(),
        }))
    }

    async fn fetch_api_source(
        &self,
        api_url: &str,
        platform: &str,
        playlist_id: &str,
        timeout: Duration,
    ) -> Result<Vec<MusicTrack>> {
        // Meting 地址常写成 `?server=:server&type=:type&id=:id` 的模板。
        // 再 append 一遍参数会让 Node 版 Meting-API 取到字面量 `:server`
        // （它读第一个值），歌单于是整份空掉。这里统一替换占位符。
        let url = api_request_url(api_url, platform, playlist_id)?;

        let response = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, MUSIC_REQUEST_UA)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| AppError::Config(format!("音乐 API 请求失败：{err}")))?;
        if !response.status().is_success() {
            return Err(AppError::Config(format!(
                "音乐 API 返回 {}，请检查地址与歌单 ID",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| AppError::Config(format!("音乐 API 响应读取失败：{err}")))?;
        if bytes.len() > MAX_JSON_BYTES {
            return Err(AppError::Config("音乐 API 响应过大".into()));
        }
        // 少数 Meting-API 部署会把 QQ 音乐包成 JSONP（`callback({...})`），
        // 多见于 `OVERSEAS=1`。能剥就剥，不为这一种部署多配一个开关。
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.trim().trim_start_matches('\u{feff}').trim();
                match (text.find('('), text.rfind(')')) {
                    (Some(start), Some(end)) if end > start => {
                        serde_json::from_str(text[start + 1..end].trim())
                            .map_err(|_| AppError::Config("音乐 API 返回的不是 JSON".into()))?
                    }
                    _ => return Err(AppError::Config("音乐 API 返回的不是 JSON".into())),
                }
            }
        };
        let items = value
            .as_array()
            .or_else(|| value.get("data").and_then(serde_json::Value::as_array))
            .or_else(|| value.get("result").and_then(serde_json::Value::as_array))
            .ok_or_else(|| AppError::Config("音乐 API 的响应格式无法识别".into()))?;

        let mut tracks = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if tracks.len() >= MAX_TRACKS {
                break;
            }
            let Some(raw_audio) = string_field(item, &["url", "audio", "src"])
                .and_then(|raw| resolve_url(&url, &raw))
            else {
                continue;
            };
            let audio_url =
                self.signed_media_url("/api/v1/music/stream", &raw_audio, "audio")?;
            let title = string_field(item, &["name", "title"])
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("曲目 {}", index + 1));
            let artist = artist_field(item);
            let album =
                string_field(item, &["album", "albumname", "album_name"]).unwrap_or_default();
            let cover_url = match string_field(item, &["pic", "cover", "image"])
                .and_then(|raw| resolve_url(&url, &raw))
            {
                Some(raw) => Some(self.signed_media_url(
                    "/api/v1/music/stream",
                    &raw,
                    "cover",
                )?),
                None => None,
            };
            let (lyric_url, lyrics) = match string_field(item, &["lrc", "lyric"]) {
                Some(raw) if raw.trim().starts_with('[') => (None, Some(raw)),
                Some(raw) => {
                    let target = match resolve_url(&url, &raw) {
                        Some(value) => Some(self.signed_media_url(
                            "/api/v1/music/lyrics",
                            &value,
                            "lyric",
                        )?),
                        None => None,
                    };
                    (target, None)
                }
                None => (None, None),
            };
            let id = item
                .get("id")
                .map(|value| match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("api-{index}"));
            tracks.push(MusicTrack {
                id,
                title,
                artist,
                album,
                duration_seconds: duration_field(item),
                cover_url,
                audio_url,
                lyric_url,
                lyrics,
            });
        }
        Ok(tracks)
    }

    /// 歌词：API 地址只允许与管理员配置的 Meting 地址同源，避免变成任意 URL 代理。
    pub async fn music_lyrics(
        &self,
        token: Option<&str>,
        url: Option<&str>,
        path: Option<&str>,
    ) -> Result<String> {
        let settings = self.settings.music().await?;
        if !settings.enabled {
            return Err(AppError::not_found("歌词"));
        }
        if let Some(token) = token.map(str::trim).filter(|value| !value.is_empty()) {
            let target = self.media_target(token, "lyric")?;
            return self.fetch_lyrics_url(target).await;
        }
        if let Some(raw) = url.map(str::trim).filter(|value| !value.is_empty()) {
            if settings.source != "api" {
                return Err(AppError::not_found("歌词"));
            }
            let target = Url::parse(raw).map_err(|_| AppError::bad_request("歌词地址无效"))?;
            // 直连歌词地址只允许落在管理员配置的任意一个音乐 API 域名下。
            let allowed = settings.api_urls.iter().any(|source| {
                Url::parse(source)
                    .map(|base| {
                        target.scheme() == base.scheme()
                            && target.host_str() == base.host_str()
                            && target.port_or_known_default() == base.port_or_known_default()
                    })
                    .unwrap_or(false)
            });
            if !allowed {
                return Err(AppError::forbidden("歌词地址不在音乐 API 域名下"));
            }
            return self.fetch_lyrics_url(target).await;
        }

        let rel = path
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::bad_request("缺少歌词地址"))?;
        if settings.source != "local" {
            return Err(AppError::not_found("歌词"));
        }
        let Some(root) = self.local_music_root(&settings) else {
            return Err(AppError::not_found("歌词"));
        };
        let file = resolve_local_path(&root.to_string_lossy(), rel, &["lrc"]).await?;
        let bytes = tokio::fs::read(&file)
            .await
            .map_err(|_| AppError::not_found("歌词"))?;
        if bytes.len() > MAX_LYRICS_BYTES {
            return Err(AppError::bad_request("歌词文件过大"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn fetch_lyrics_url(&self, target: Url) -> Result<String> {
        let response = self
            .http
            .get(target)
            .header(reqwest::header::USER_AGENT, MUSIC_REQUEST_UA)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|err| AppError::Config(format!("歌词请求失败：{err}")))?;
        if !response.status().is_success() {
            return Err(AppError::not_found("歌词"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| AppError::Config(format!("歌词读取失败：{err}")))?;
        if bytes.len() > MAX_LYRICS_BYTES {
            return Err(AppError::bad_request("歌词文件过大"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 校验服务端签发的媒体令牌。令牌里带 URL、类型与过期时间；
    /// 只有从管理员配置的 API 响应里签出去的地址才允许被代理。
    fn media_target(&self, token: &str, expected_kind: &str) -> Result<Url> {
        let raw = self
            .settings
            .verify_payload(token.trim())
            .map_err(|_| AppError::bad_request("媒体地址无效，请刷新歌单"))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| AppError::bad_request("媒体地址已损坏，请刷新歌单"))?;
        if value.get("kind").and_then(|kind| kind.as_str()) != Some(expected_kind) {
            return Err(AppError::bad_request("媒体类型不匹配"));
        }
        let expires = value.get("exp").and_then(|exp| exp.as_i64()).unwrap_or(0);
        if expires < Utc::now().timestamp() {
            return Err(AppError::not_found("媒体地址已过期，请刷新歌单"));
        }
        let raw_url = value
            .get("url")
            .and_then(|url| url.as_str())
            .ok_or_else(|| AppError::bad_request("媒体地址缺少 URL"))?;
        let url = Url::parse(raw_url).map_err(|_| AppError::bad_request("媒体地址无效"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(AppError::bad_request("媒体地址只支持 http / https"));
        }
        Ok(url)
    }

    /// 外部音轨/封面的代理目标。
    pub fn music_stream_target(&self, token: &str) -> Result<Url> {
        let raw = self
            .settings
            .verify_payload(token.trim())
            .map_err(|_| AppError::bad_request("媒体地址无效，请刷新歌单"))?;
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|_| AppError::bad_request("媒体地址已损坏，请刷新歌单"))?;
        let kind = value.get("kind").and_then(|kind| kind.as_str()).unwrap_or("");
        if kind != "audio" && kind != "cover" {
            return Err(AppError::bad_request("媒体类型不匹配"));
        }
        self.media_target(token, kind)
    }

    /// 本地音乐文件（音频或封面）。只允许配置目录内、扩展名在白名单里的文件。
    pub async fn resolve_local_music_file(&self, rel: &str) -> Result<PathBuf> {
        let settings = self.settings.music().await?;
        if !settings.enabled || settings.source != "local" {
            return Err(AppError::not_found("音乐文件"));
        }
        let Some(root) = self.local_music_root(&settings) else {
            return Err(AppError::not_found("音乐文件"));
        };
        resolve_local_path(
            &root.to_string_lossy(),
            rel,
            &[AUDIO_EXTS, IMAGE_EXTS].concat(),
        )
        .await
    }
}

fn string_field(item: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = item.get(*key) {
            if let Some(text) = value.as_str() {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_string());
                }
            }
            if value.is_number() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn artist_field(item: &serde_json::Value) -> String {
    if let Some(text) = string_field(item, &["artist", "author", "singer"]) {
        return text;
    }
    for key in ["artist", "artists", "singer"] {
        if let Some(array) = item.get(key).and_then(serde_json::Value::as_array) {
            let names: Vec<String> = array
                .iter()
                .filter_map(|entry| {
                    entry.as_str().map(str::to_string).or_else(|| {
                        entry
                            .get("name")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                })
                .filter(|value| !value.trim().is_empty())
                .collect();
            if !names.is_empty() {
                return names.join(" / ");
            }
        }
    }
    String::new()
}

fn duration_field(item: &serde_json::Value) -> i64 {
    for key in ["duration", "time", "interval"] {
        if let Some(value) = item.get(key).and_then(|value| value.as_i64()) {
            // 有的接口给毫秒；大于 3 小时的值按毫秒处理
            return if value > 3 * 60 * 60 {
                value / 1000
            } else {
                value
            };
        }
    }
    0
}

/// 把管理员填写的 Meting 地址拼成一次歌单请求。
///
/// 支持两种写法：
///   - `https://host/api`：直接追加 `server` / `type` / `id`；
///   - `https://host/api?server=:server&type=:type&id=:id&auth=:auth&r=:r`：
///     替换占位符，绝不留下重复的查询参数（Node 版 Meting-API 读第一个值）。
fn api_request_url(api_url: &str, platform: &str, playlist_id: &str) -> Result<Url> {
    let mut url = Url::parse(api_url).map_err(|_| {
        AppError::Config("音乐 API 地址无法解析，请填写完整的 http(s) 地址".into())
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::Config("音乐 API 地址只支持 http / https".into()));
    }

    let playlist_id = playlist_id.trim();
    let nonce = Utc::now().timestamp_millis().to_string();
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut has_server = false;
    let mut has_type = false;
    let mut has_id = false;
    for (key, value) in &mut pairs {
        match key.as_str() {
            "server" => {
                *value = platform.to_string();
                has_server = true;
            }
            "type" => {
                *value = "playlist".to_string();
                has_type = true;
            }
            "id" => {
                *value = playlist_id.to_string();
                has_id = true;
            }
            // 模板里的认证参数通常就是空值；保留字面量 `:auth` 会被对方当成令牌。
            "auth" if value == ":auth" => value.clear(),
            // `:r` 是防缓存的随机数，换成当前毫秒。
            "r" if value.is_empty() || value == ":r" => *value = nonce.clone(),
            _ => {}
        }
    }
    if !has_server {
        pairs.push(("server".to_string(), platform.to_string()));
    }
    if !has_type {
        pairs.push(("type".to_string(), "playlist".to_string()));
    }
    if !has_id {
        pairs.push(("id".to_string(), playlist_id.to_string()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url)
}

fn resolve_url(base: &Url, raw: &str) -> Option<String> {
    let parsed = base.join(raw.trim()).ok()?;
    if matches!(parsed.scheme(), "http" | "https") {
        Some(parsed.to_string())
    } else {
        None
    }
}

fn local_url(endpoint: &str, rel: &str) -> String {
    let mut url = Url::parse(&format!("http://music.local{endpoint}")).expect("静态地址");
    url.query_pairs_mut().append_pair("path", rel);
    format!("{}?{}", url.path(), url.query().unwrap_or_default())
}

/// 第三方歌词地址包成站内代理地址：前端只认站内路径，不直接跨域请求歌词。
fn scan_local(root: &Path) -> Vec<MusicTrack> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    walk(&root, &root, 0, &mut files);
    files.sort();
    files.truncate(MAX_TRACKS);

    let mut tracks = Vec::new();
    for (rel, absolute) in files {
        let Some(ext) = absolute.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !AUDIO_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        let stem = absolute
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("未命名曲目");
        let (artist, title) = match stem.split_once(" - ") {
            Some((artist, title)) if !artist.trim().is_empty() && !title.trim().is_empty() => {
                (artist.trim().to_string(), title.trim().to_string())
            }
            _ => (String::new(), stem.trim().to_string()),
        };
        let parent = absolute.parent().unwrap_or(&root);
        let album = if parent == root {
            String::new()
        } else {
            parent
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string()
        };
        let artist = if artist.is_empty() && !album.is_empty() {
            album.clone()
        } else {
            artist
        };
        let dir = absolute.parent().unwrap_or(&root);
        let cover = find_cover(&root, dir, stem);
        let lyric = find_lyric(&root, dir, stem);
        tracks.push(MusicTrack {
            id: format!("local:{rel}"),
            title: if title.is_empty() {
                "未命名曲目".into()
            } else {
                title
            },
            artist,
            album,
            duration_seconds: 0,
            cover_url: cover.map(|path| local_url("/api/v1/music/file", &path)),
            audio_url: local_url("/api/v1/music/file", &rel),
            lyric_url: lyric.map(|path| local_url("/api/v1/music/lyrics", &path)),
            lyrics: None,
        });
    }
    tracks
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(root, &path, depth + 1, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        out.push((rel, path));
    }
}

fn find_cover(root: &Path, dir: &Path, stem: &str) -> Option<String> {
    for name in ["cover", stem] {
        for ext in IMAGE_EXTS {
            for candidate_ext in [ext.to_string(), ext.to_uppercase()] {
                let path = dir.join(format!("{name}.{candidate_ext}"));
                if path.is_file() {
                    return path
                        .strip_prefix(root)
                        .ok()
                        .map(|rel| rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    None
}

fn find_lyric(root: &Path, dir: &Path, stem: &str) -> Option<String> {
    let path = dir.join(format!("{stem}.lrc"));
    if path.is_file() {
        path.strip_prefix(root)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
    } else {
        None
    }
}

async fn resolve_local_path(root: &str, rel: &str, allowed: &[&str]) -> Result<PathBuf> {
    let root = root.trim();
    if root.is_empty() {
        return Err(AppError::not_found("音乐目录"));
    }
    let rel = rel.replace('\\', "/");
    let rel_path = Path::new(&rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(AppError::forbidden("音乐文件路径不合法"));
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| AppError::not_found("音乐目录"))?;
    let candidate = root.join(rel_path);
    let file = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|_| AppError::not_found("音乐文件"))?;
    if !file.starts_with(&root) {
        return Err(AppError::forbidden("音乐文件路径不合法"));
    }
    let ext = file
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    if !allowed.contains(&ext.as_str()) {
        return Err(AppError::not_found("音乐文件"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(url: &Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn api_url_replaces_meting_template_instead_of_duplicating() {
        let url = api_request_url(
            "https://music.example.com/api?server=:server&type=:type&id=:id&auth=:auth&r=:r",
            "netease",
            " 2884035 ",
        )
        .expect("模板地址");
        let pairs = pairs(&url);
        assert_eq!(
            pairs.iter().filter(|(key, _)| key == "server").count(),
            1,
            "server 只能出现一次"
        );
        assert_eq!(
            pairs.iter().filter(|(key, _)| key == "type").count(),
            1,
            "type 只能出现一次"
        );
        assert_eq!(
            pairs.iter().filter(|(key, _)| key == "id").count(),
            1,
            "id 只能出现一次"
        );
        assert!(pairs.contains(&("server".into(), "netease".into())));
        assert!(pairs.contains(&("type".into(), "playlist".into())));
        assert!(pairs.contains(&("id".into(), "2884035".into())));
        assert!(pairs.contains(&("auth".into(), String::new())));
        let r = pairs
            .iter()
            .find(|(key, _)| key == "r")
            .map(|(_, value)| value)
            .expect("r 参数");
        assert!(!r.is_empty() && r != ":r");
    }

    #[test]
    fn api_url_appends_params_to_a_plain_endpoint() {
        let url =
            api_request_url("https://music.example.com/api", "tencent", "42").expect("普通地址");
        let pairs = pairs(&url);
        assert!(pairs.contains(&("server".into(), "tencent".into())));
        assert!(pairs.contains(&("type".into(), "playlist".into())));
        assert!(pairs.contains(&("id".into(), "42".into())));
    }

    #[test]
    fn api_url_overrides_existing_concrete_values() {
        let url = api_request_url(
            "https://music.example.com/api?server=netease&type=song&id=old&format=json",
            "tencent",
            "new-id",
        )
        .expect("已有参数的地址");
        let pairs = pairs(&url);
        assert!(pairs.contains(&("server".into(), "tencent".into())));
        assert!(pairs.contains(&("type".into(), "playlist".into())));
        assert!(pairs.contains(&("id".into(), "new-id".into())));
        assert!(pairs.contains(&("format".into(), "json".into())));
    }
}
