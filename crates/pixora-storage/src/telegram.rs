//! Telegram 后端：Bot API 把文件发到频道/群组。
//!
//! 与其它驱动最大的不同是「存储键不是路径」：Telegram 用 `file_id` 标识文件，
//! 而且 `file_path` 会随时间变化。所以驱动返回的 key 形如：
//!
//! ```text
//! tg/{chat_id}/{message_id}/{file_id}
//! ```
//!
//! 把 chat 与 message 一起记下来，删除时才能调 `deleteMessage` 真正把消息撤掉
//! （只删 file_id 是删不掉的）。这串 key 全部由服务端在响应里拿到后生成，
//! 不掺任何用户输入。
//!
//! 设计稿要求「通过代理访问」：`proxy` 字段直接喂给 reqwest。

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncRead;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

const DRIVER: &str = crate::names::TELEGRAM;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TelegramConfig {
    /// Bot Token（形如 `123456:ABC-DEF...`），库里加密存放
    pub bot_token: String,
    /// 目标频道或群组的 chat id，形如 `-1001234567890`
    pub chat_id: String,
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// 代理地址，形如 `http://127.0.0.1:7890` 或 `socks5://…`
    #[serde(default)]
    pub proxy: Option<String>,
}

fn default_api_base() -> String {
    "https://api.telegram.org".to_string()
}

pub struct TelegramDriver {
    config: TelegramConfig,
    instance: String,
}

impl TelegramDriver {
    /// 代理在构造客户端时决定，所以这里自己建 client，不共用全局连接池。
    pub fn new(config: TelegramConfig) -> Result<Self> {
        Ok(Self {
            config,
            instance: DRIVER.to_string(),
        })
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    fn client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .user_agent("Pixora")
            .timeout(std::time::Duration::from_secs(120));
        if let Some(proxy) = self.config.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|err| StorageError::config(DRIVER, format!("代理地址无效：{err}")))?,
            );
        }
        builder
            .build()
            .map_err(|err| StorageError::config(DRIVER, format!("HTTP 客户端构建失败：{err}")))
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            self.config.api_base.trim_end_matches('/'),
            self.config.bot_token,
            method
        )
    }

    /// 从存储键里解析出 (chat_id, message_id, file_id)。
    fn parse_key(key: &str) -> Result<(String, String, String)> {
        let mut parts = key.splitn(4, '/');
        let prefix = parts.next().unwrap_or_default();
        let chat = parts.next().unwrap_or_default();
        let message = parts.next().unwrap_or_default();
        let file = parts.next().unwrap_or_default();
        if prefix != "tg" || chat.is_empty() || message.is_empty() || file.is_empty() {
            return Err(StorageError::malformed(
                DRIVER,
                format!("存储键格式应为 tg/chat/message/file_id，实际为 {key}"),
            ));
        }
        Ok((chat.to_string(), message.to_string(), file.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SentDocument {
    message_id: i64,
    document: Option<TelegramDocument>,
}

#[derive(Debug, Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_size: Option<u64>,
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
    file_size: Option<u64>,
}

#[async_trait]
impl StorageDriver for TelegramDriver {
    fn name(&self) -> &'static str {
        DRIVER
    }

    fn instance_key(&self) -> String {
        self.instance.clone()
    }

    async fn put(
        &self,
        _key: &str,
        mut reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        mime: &str,
    ) -> Result<StoredObject> {
        use tokio::io::AsyncReadExt;

        // Bot API 单文件上限 50MB（普通 bot），且上传要走 multipart，必须整份在内存里
        const MAX_UPLOAD: u64 = 50 * 1024 * 1024;
        if size > MAX_UPLOAD {
            return Err(StorageError::TooLarge {
                size,
                limit: MAX_UPLOAD,
            });
        }
        let mut content = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut content).await?;

        let client = self.client()?;
        let part = reqwest::multipart::Part::bytes(content)
            .file_name("image")
            .mime_str(mime)
            .map_err(|err| StorageError::config(DRIVER, err.to_string()))?;
        let form = reqwest::multipart::Form::new()
            .text("chat_id", self.config.chat_id.clone())
            .text("disable_notification", "true")
            .part("document", part);

        let response = client
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;

        let status = response.status();
        let body: ApiResponse<SentDocument> = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;

        if !status.is_success() || !body.ok {
            let message = body.description.unwrap_or_else(|| "未知错误".into());
            // 429 会带 retry_after，交给上层退避
            let code = if status.as_u16() == 200 { 429 } else { status.as_u16() };
            return Err(StorageError::remote(DRIVER, code, message));
        }

        let sent = body
            .result
            .ok_or_else(|| StorageError::malformed(DRIVER, "响应里没有 result"))?;
        let document = sent
            .document
            .ok_or_else(|| StorageError::malformed(DRIVER, "响应里没有 document"))?;

        // 键里带上 chat 与 message，删除时才能撤回消息
        let key = format!("tg/{}/{}/{}", self.config.chat_id, sent.message_id, document.file_id);
        Ok(StoredObject {
            key,
            size: document.file_size.unwrap_or(size),
            driver: DRIVER,
        })
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let (_, _, file_id) = Self::parse_key(key)?;
        let client = self.client()?;

        let response = client
            .get(self.api_url("getFile"))
            .query(&[("file_id", file_id)])
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        let status = response.status();
        let body: ApiResponse<TelegramFile> = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        if !body.ok {
            return Err(StorageError::remote(
                DRIVER,
                status.as_u16(),
                body.description.unwrap_or_else(|| "getFile 失败".into()),
            ));
        }
        let file_path = body
            .result
            .and_then(|file| file.file_path)
            .ok_or_else(|| StorageError::malformed(DRIVER, "getFile 没有返回 file_path"))?;

        let download_url = format!(
            "{}/file/bot{}/{}",
            self.config.api_base.trim_end_matches('/'),
            self.config.bot_token,
            file_path
        );
        let file_response = client
            .get(download_url)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        if file_response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_string()));
        }
        if !file_response.status().is_success() {
            return Err(StorageError::remote(
                DRIVER,
                file_response.status().as_u16(),
                "下载文件失败",
            ));
        }
        let bytes = file_response
            .bytes()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        Ok(Box::new(std::io::Cursor::new(bytes.to_vec())))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let (chat, message, _) = Self::parse_key(key)?;
        let client = self.client()?;
        let response = client
            .post(self.api_url("deleteMessage"))
            .form(&[("chat_id", chat.as_str()), ("message_id", message.as_str())])
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        let body: ApiResponse<bool> = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        // 消息早已被手工删除的情况不算失败
        if body.ok {
            return Ok(());
        }
        let description = body.description.unwrap_or_default();
        if description.contains("message to delete not found") {
            return Ok(());
        }
        Err(StorageError::remote(DRIVER, 200, description))
    }

    async fn stat(&self, key: &str) -> Result<ObjectInfo> {
        let (_, _, file_id) = Self::parse_key(key)?;
        let client = self.client()?;
        let response = client
            .get(self.api_url("getFile"))
            .query(&[("file_id", file_id)])
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        let body: ApiResponse<TelegramFile> = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        if !body.ok {
            return Err(StorageError::NotFound(key.to_string()));
        }
        Ok(ObjectInfo {
            key: key.to_string(),
            size: body.result.and_then(|file| file.file_size).unwrap_or(0),
            modified: None,
            content_type: None,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::remote(false)
    }

    async fn health(&self) -> Result<String> {
        let client = self.client()?;
        let response = client
            .get(self.api_url("getMe"))
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        #[derive(Deserialize)]
        struct Me {
            username: Option<String>,
        }
        let body: ApiResponse<Me> = response
            .json()
            .await
            .map_err(|err| StorageError::malformed(DRIVER, err.to_string()))?;
        if !body.ok {
            return Err(StorageError::config(
                DRIVER,
                body.description.unwrap_or_else(|| "Token 无效".into()),
            ));
        }
        let username = body
            .result
            .and_then(|me| me.username)
            .unwrap_or_else(|| "未知".into());
        Ok(format!("连通 · Bot @{username} · 频道 {}", self.config.chat_id))
    }

    async fn link_view(&self, _key: &str, _view_rel: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let key = "tg/-1001234567890/42/BAACAgEAAxkBAAIC";
        let (chat, message, file) = TelegramDriver::parse_key(key).unwrap();
        assert_eq!(chat, "-1001234567890");
        assert_eq!(message, "42");
        assert_eq!(file, "BAACAgEAAxkBAAIC");
    }

    #[test]
    fn malformed_keys_are_rejected() {
        assert!(TelegramDriver::parse_key("blobs/aa/abc.jpg").is_err());
        assert!(TelegramDriver::parse_key("tg//42/x").is_err());
        assert!(TelegramDriver::parse_key("tg/1/2/").is_err());
    }

    #[test]
    fn api_url_shape() {
        let driver = TelegramDriver::new(TelegramConfig {
            bot_token: "123:ABC".into(),
            chat_id: "-100".into(),
            api_base: "https://api.telegram.org".into(),
            proxy: None,
        })
        .unwrap();
        assert_eq!(
            driver.api_url("sendDocument"),
            "https://api.telegram.org/bot123:ABC/sendDocument"
        );
    }
}
