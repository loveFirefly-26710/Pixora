//! WebDAV 后端：PUT 上传，Basic Auth 认证。
//!
//! WebDAV 服务端的实现差异很大（Nextcloud、Alist、群晖各有脾气），
//! 所以这里只用最小子集：PUT / GET / DELETE / HEAD。
//! 路径不需要预先创建目录——PUT 到不存在的目录时会失败，
//! 那种情况下我们把错误原样带给上层，由它决定是否回退本地。

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

const DRIVER: &str = crate::names::WEBDAV;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebDavConfig {
    /// 根地址，例如 `https://dav.example.com/remote.php/dav/files/me`
    pub base_url: String,
    pub username: String,
    /// 密码或应用专用密码，库里加密存放
    pub password: String,
    /// 根地址下的路径前缀
    #[serde(default)]
    pub prefix: String,
}

pub struct WebDavDriver {
    config: WebDavConfig,
    client: Arc<reqwest::Client>,
    instance: String,
}

impl WebDavDriver {
    pub fn new(config: WebDavConfig, client: Arc<reqwest::Client>) -> Self {
        Self {
            config,
            client,
            instance: DRIVER.to_string(),
        }
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    /// 拼接目标 URL。路径分段逐段做百分号编码，
    /// 不编码会踩到中文相册名、`#` 之类的字符。
    fn url_for(&self, key: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let prefix = self.config.prefix.trim_matches('/');
        let key = key.trim_start_matches('/');

        let mut segments: Vec<String> = Vec::new();
        if !prefix.is_empty() {
            segments.extend(prefix.split('/').filter(|s| !s.is_empty()).map(encode_segment));
        }
        segments.extend(key.split('/').filter(|s| !s.is_empty()).map(encode_segment));

        format!("{base}/{}", segments.join("/"))
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .basic_auth(&self.config.username, Some(&self.config.password))
            .header("User-Agent", "Pixora")
    }
}

fn encode_segment(segment: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    // 只保留 RFC 3986 里的 unreserved 字符，其余全部编码
    const SET: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(segment, SET).to_string()
}

#[async_trait]
impl StorageDriver for WebDavDriver {
    fn name(&self) -> &'static str {
        DRIVER
    }

    fn instance_key(&self) -> String {
        self.instance.clone()
    }

    async fn put(
        &self,
        key: &str,
        reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        mime: &str,
    ) -> Result<StoredObject> {
        let url = self.url_for(key);
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(reader));
        let response = self
            .request(reqwest::Method::PUT, &url)
            .header("Content-Type", mime)
            .header("Content-Length", size.to_string())
            .body(body)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;

        let status = response.status();
        // 201 新建、204 覆盖，都算成功
        if status.is_success() {
            return Ok(StoredObject {
                key: key.to_string(),
                size,
                driver: DRIVER,
            });
        }
        Err(StorageError::remote(
            DRIVER,
            status.as_u16(),
            response.text().await.unwrap_or_default(),
        ))
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let url = self.url_for(key);
        let response = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_string()));
        }
        if !response.status().is_success() {
            return Err(StorageError::remote(
                DRIVER,
                response.status().as_u16(),
                response.text().await.unwrap_or_default(),
            ));
        }
        // 流式回传，不把整图读进内存
        let stream = response.bytes_stream();
        let reader = tokio_util::io::StreamReader::new(
            stream.map_err(|err| std::io::Error::other(err.to_string())),
        );
        Ok(Box::new(reader))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let url = self.url_for(key);
        let response = self
            .request(reqwest::Method::DELETE, &url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(StorageError::remote(
            DRIVER,
            response.status().as_u16(),
            response.text().await.unwrap_or_default(),
        ))
    }

    async fn stat(&self, key: &str) -> Result<ObjectInfo> {
        let url = self.url_for(key);
        let response = self
            .request(reqwest::Method::HEAD, &url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_string()));
        }
        if !response.status().is_success() {
            return Err(StorageError::remote(
                DRIVER,
                response.status().as_u16(),
                "HEAD 请求失败",
            ));
        }
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let modified = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc));
        Ok(ObjectInfo {
            key: key.to_string(),
            size,
            modified,
            content_type,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::remote(false)
    }

    async fn health(&self) -> Result<String> {
        // PROPFIND Depth: 0 是最通用的「验证地址与凭据」手段
        let url = self
            .url_for("")
            .trim_end_matches('/')
            .to_string()
            .to_string();
        let response = self
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").expect("合法方法名"),
                if url.is_empty() { &self.config.base_url } else { &url },
            )
            .header("Depth", "0")
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::MULTI_STATUS {
            return Ok(format!("连通 · {}", self.config.base_url));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(StorageError::config(DRIVER, "用户名或密码不正确"));
        }
        Err(StorageError::remote(DRIVER, status.as_u16(), "PROPFIND 失败"))
    }

    async fn link_view(&self, _key: &str, _view_rel: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(base: &str, prefix: &str) -> WebDavDriver {
        WebDavDriver::new(
            WebDavConfig {
                base_url: base.to_string(),
                username: "u".into(),
                password: "p".into(),
                prefix: prefix.to_string(),
            },
            Arc::new(reqwest::Client::new()),
        )
    }

    #[test]
    fn url_joins_prefix_and_encodes_chinese() {
        let driver = driver("https://dav.example.com/dav/", "pixora");
        let url = driver.url_for("blobs/a3/abc.webp");
        assert_eq!(url, "https://dav.example.com/dav/pixora/blobs/a3/abc.webp");

        let chinese = driver.url_for("users/1001/旅行/abc.webp");
        assert!(chinese.contains("%E6%97%85%E8%A1%8C"), "{chinese}");
    }

    #[test]
    fn url_without_prefix_is_base_plus_key() {
        let driver = driver("https://dav.example.com", "");
        assert_eq!(
            driver.url_for("blobs/a3/abc.webp"),
            "https://dav.example.com/blobs/a3/abc.webp"
        );
    }
}
