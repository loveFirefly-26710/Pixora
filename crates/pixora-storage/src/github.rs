//! GitHub 仓库后端：走 Contents API 推送到仓库。
//!
//! 两个坑要处理：
//!   1. 同名文件已存在时，PUT 必须带上它当前的 `sha`，否则 API 拒绝覆盖。
//!      所以「先试 PUT、遇到 422 再查 sha 重试」是必需的流程，不能省。
//!   2. 仓库里的路径就是文件真实身份，删除也要先拿 sha。
//!
//! 读侧走 API 的 raw 媒体类型而不是 `raw.githubusercontent.com`：
//! 后者有 CDN 缓存，刚推上去就删掉的图片可能还能被访问到。

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

const DRIVER: &str = crate::names::GITHUB;

/// 后端配置。字段与后台「后端配置」表单一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubConfig {
    /// Personal Access Token，需要 repo 读写权限；库里加密存放
    pub token: String,
    pub owner: String,
    pub repo: String,
    /// 仓库内路径前缀，默认 `images`
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// API 根地址，GitHub Enterprise 可改
    #[serde(default = "default_api_base")]
    pub api_base: String,
}

fn default_prefix() -> String {
    "images".to_string()
}
fn default_branch() -> String {
    "main".to_string()
}
fn default_api_base() -> String {
    "https://api.github.com".to_string()
}

pub struct GitHubDriver {
    config: GitHubConfig,
    client: Arc<reqwest::Client>,
    instance: String,
}

impl GitHubDriver {
    pub fn new(config: GitHubConfig, client: Arc<reqwest::Client>) -> Self {
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

    /// 仓库内的完整路径。
    fn remote_path(&self, key: &str) -> String {
        let prefix = self.config.prefix.trim_matches('/');
        let key = key.trim_start_matches('/');
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}/{key}")
        }
    }

    fn contents_url(&self, remote_path: &str) -> String {
        format!(
            "{}/repos/{}/{}/contents/{}",
            self.config.api_base.trim_end_matches('/'),
            self.config.owner,
            self.config.repo,
            remote_path
        )
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "Pixora")
    }

    /// 取文件当前的 sha。不存在返回 None。
    async fn current_sha(&self, remote_path: &str) -> Result<Option<String>> {
        let url = format!("{}?ref={}", self.contents_url(remote_path), self.config.branch);
        let response = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(StorageError::remote(
                DRIVER,
                response.status().as_u16(),
                response.text().await.unwrap_or_default(),
            ));
        }
        let body: ContentsResponse = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        Ok(Some(body.sha))
    }
}

#[derive(Debug, Deserialize)]
struct ContentsResponse {
    sha: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Serialize)]
struct PutBody<'a> {
    message: &'a str,
    content: String,
    branch: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[async_trait]
impl StorageDriver for GitHubDriver {
    fn name(&self) -> &'static str {
        DRIVER
    }

    fn instance_key(&self) -> String {
        self.instance.clone()
    }

    async fn put(
        &self,
        key: &str,
        mut reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        _mime: &str,
    ) -> Result<StoredObject> {
        use tokio::io::AsyncReadExt;

        // Contents API 只接受整份内容，没有分块上传的余地
        const MAX_INLINE: u64 = 32 * 1024 * 1024;
        if size > MAX_INLINE {
            return Err(StorageError::TooLarge {
                size,
                limit: MAX_INLINE,
            });
        }
        let mut content = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut content).await?;

        let remote_path = self.remote_path(key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&content);

        let mut attempt_sha = None;
        for attempt in 0..2 {
            let body = PutBody {
                message: "Pixora: 上传图片",
                content: encoded.clone(),
                branch: &self.config.branch,
                sha: attempt_sha.clone(),
            };
            let response = self
                .request(reqwest::Method::PUT, &self.contents_url(&remote_path))
                .json(&body)
                .send()
                .await
                .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;

            let status = response.status();
            if status.is_success() {
                return Ok(StoredObject {
                    key: key.to_string(),
                    size: content.len() as u64,
                    driver: DRIVER,
                });
            }

            // 422/409 说明文件已存在，第二趟带上 sha 覆盖
            if attempt == 0
                && (status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
                    || status == reqwest::StatusCode::CONFLICT)
            {
                attempt_sha = self.current_sha(&remote_path).await?;
                if attempt_sha.is_none() {
                    return Err(StorageError::remote(
                        DRIVER,
                        status.as_u16(),
                        "文件存在但取不到 sha，无法覆盖",
                    ));
                }
                continue;
            }

            return Err(StorageError::remote(
                DRIVER,
                status.as_u16(),
                response.text().await.unwrap_or_default(),
            ));
        }

        Err(StorageError::remote(DRIVER, 422, "覆盖已存在的文件失败"))
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let url = format!(
            "{}?ref={}",
            self.contents_url(&self.remote_path(key)),
            self.config.branch
        );
        let response = self
            .request(reqwest::Method::GET, &url)
            .header("Accept", "application/vnd.github.raw+json")
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
        let bytes = response
            .bytes()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        Ok(Box::new(std::io::Cursor::new(bytes.to_vec())))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let remote_path = self.remote_path(key);
        let Some(sha) = self.current_sha(&remote_path).await? else {
            return Ok(());
        };
        let body = serde_json::json!({
            "message": "Pixora: 删除图片",
            "sha": sha,
            "branch": self.config.branch,
        });
        let response = self
            .request(reqwest::Method::DELETE, &self.contents_url(&remote_path))
            .json(&body)
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
        let url = format!(
            "{}?ref={}",
            self.contents_url(&self.remote_path(key)),
            self.config.branch
        );
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
        let body: ContentsResponse = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        Ok(ObjectInfo {
            key: key.to_string(),
            size: body.size,
            modified: None,
            content_type: None,
        })
    }

    fn capabilities(&self) -> Capabilities {
        // raw 链接是公网可访问的，直链可以直接指过去
        Capabilities::remote(true)
    }

    async fn health(&self) -> Result<String> {
        let url = format!(
            "{}/repos/{}/{}",
            self.config.api_base.trim_end_matches('/'),
            self.config.owner,
            self.config.repo
        );
        let response = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        if !response.status().is_success() {
            return Err(StorageError::remote(
                DRIVER,
                response.status().as_u16(),
                "仓库不可访问，检查 Token 权限与仓库名",
            ));
        }
        Ok(format!(
            "连通 · {}/{} · 分支 {}",
            self.config.owner, self.config.repo, self.config.branch
        ))
    }

    async fn link_view(&self, _key: &str, _view_rel: &str) -> Result<bool> {
        // 远端后端没有本地目录树可言，明确回 false 让上层走虚拟树
        Ok(false)
    }
}
