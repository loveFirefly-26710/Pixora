//! S3 兼容对象存储后端（含阿里云 OSS、MinIO）。
//!
//! 只用最小操作集：PUT / GET / DELETE / HEAD，认证走 AWS Signature V4。
//! 签名是这类后端唯一容易写错的地方，所以关键步骤都拆成了纯函数并配了
//! 已知向量测试——签名错了会得到晦涩的 403，不如先在本地测准。
//!
//! 阿里云 OSS 的兼容要点：`region` 用 `cn-hangzhou` 这类值，签名版本必须
//! 是 v4（v1 已停用），并且必须用虚拟主机风格或路径风格二选一——
//! 这里默认路径风格（`endpoint/bucket/key`），MinIO 与 OSS 都支持。

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::io::AsyncRead;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

const DRIVER: &str = crate::names::S3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct S3Config {
    /// 服务地址，例如 `https://oss-cn-hangzhou.aliyuncs.com` 或 MinIO 的 `http://127.0.0.1:9000`
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    /// 密钥，库里加密存放
    pub secret_key: String,
    /// 桶内的路径前缀
    #[serde(default)]
    pub prefix: String,
    /// 路径风格（MinIO、OSS 都支持；AWS 新桶可能只支持虚拟主机风格）
    #[serde(default = "default_true")]
    pub path_style: bool,
    /// 公网访问前缀，配了它直链就能直接指向对象存储，绕过本站
    #[serde(default)]
    pub public_base_url: Option<String>,
}

fn default_true() -> bool {
    true
}

pub struct S3Driver {
    config: S3Config,
    client: Arc<reqwest::Client>,
    instance: String,
}

impl S3Driver {
    pub fn new(config: S3Config, client: Arc<reqwest::Client>) -> Self {
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

    /// 桶内对象键。
    fn object_key(&self, key: &str) -> String {
        let prefix = self.config.prefix.trim_matches('/');
        let key = key.trim_start_matches('/');
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}/{key}")
        }
    }

    /// 归一化的请求地址与用于签名的 path。
    fn request_target(&self, object_key: &str) -> (String, String) {
        let endpoint = self.config.endpoint.trim_end_matches('/');
        let encoded_key = object_key
            .split('/')
            .map(|segment| uri_encode(segment, true))
            .collect::<Vec<_>>()
            .join("/");

        if self.config.path_style {
            let path = format!("/{}/{}", self.config.bucket, encoded_key);
            (format!("{endpoint}{path}"), path)
        } else {
            // 虚拟主机风格：bucket 进域名
            let host = endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            let scheme = if endpoint.starts_with("http://") { "http" } else { "https" };
            let path = format!("/{encoded_key}");
            (
                format!("{scheme}://{}.{}{path}", self.config.bucket, host),
                path,
            )
        }
    }

    fn host_header(&self, url: &str) -> String {
        url.trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// 签一个请求，返回带签名头的 RequestBuilder。
    fn sign(
        &self,
        method: &str,
        url: &str,
        canonical_path: &str,
        payload_hash: &str,
        content_type: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> reqwest::RequestBuilder {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let host = self.host_header(url);

        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), host),
            ("x-amz-content-sha256".into(), payload_hash.to_string()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        if let Some(content_type) = content_type {
            headers.push(("content-type".into(), content_type.to_string()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect();
        let signed_headers: String = headers
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{method}\n{canonical_path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let signature = sign_v4(
            &self.config.secret_key,
            &date_stamp,
            &self.config.region,
            "s3",
            string_to_sign.as_bytes(),
        );

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key
        );

        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut request = self
            .client
            .request(method, url)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header("Authorization", authorization);
        if let Some(content_type) = content_type {
            request = request.header("Content-Type", content_type.to_string());
        }
        request
    }
}

/// AWS Signature V4 的派生密钥与签名。
fn sign_v4(
    secret: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
    string_to_sign: &[u8],
) -> String {
    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    hex::encode(hmac(&k_signing, string_to_sign))
}

/// SigV4 规定的 URI 编码：`/` 不编码（路径分隔符），其余按 RFC 3986 unreserved 之外全编。
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 空载荷与普通载荷的 sha256。
fn payload_hash(content: &[u8]) -> String {
    hex::encode(Sha256::digest(content))
}

#[async_trait]
impl StorageDriver for S3Driver {
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
        mime: &str,
    ) -> Result<StoredObject> {
        use tokio::io::AsyncReadExt;

        // SigV4 要求先知道载荷哈希，所以要么整份读进内存，要么走 UNSIGNED-PAYLOAD。
        // 后者会失去完整性校验，这里选择内存内计算（图床单文件上限 20MB，可控）。
        let mut content = Vec::with_capacity(size as usize);
        reader.read_to_end(&mut content).await?;
        let hash = payload_hash(&content);

        let (url, path) = self.request_target(&self.object_key(key));
        let request = self.sign(
            "PUT",
            &url,
            &path,
            &hash,
            Some(mime),
            chrono::Utc::now(),
        );
        let response = request
            .body(content)
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;

        if response.status().is_success() {
            return Ok(StoredObject {
                key: key.to_string(),
                size,
                driver: DRIVER,
            });
        }
        Err(StorageError::remote(
            DRIVER,
            response.status().as_u16(),
            response.text().await.unwrap_or_default(),
        ))
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let (url, path) = self.request_target(&self.object_key(key));
        let request = self.sign(
            "GET",
            &url,
            &path,
            &payload_hash(b""),
            None,
            chrono::Utc::now(),
        );
        let response = request
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        match response.status().as_u16() {
            404 => return Err(StorageError::NotFound(key.to_string())),
            status if (200..300).contains(&status) => {}
            status => {
                return Err(StorageError::remote(
                    DRIVER,
                    status,
                    response.text().await.unwrap_or_default(),
                ))
            }
        }
        let stream = response.bytes_stream();
        let reader = tokio_util::io::StreamReader::new(
            stream.map_err(|err| std::io::Error::other(err.to_string())),
        );
        Ok(Box::new(reader))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let (url, path) = self.request_target(&self.object_key(key));
        let request = self.sign(
            "DELETE",
            &url,
            &path,
            &payload_hash(b""),
            None,
            chrono::Utc::now(),
        );
        let response = request
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        // S3 的 DELETE 是幂等的：对象不存在也返回 204
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
        let (url, path) = self.request_target(&self.object_key(key));
        let request = self.sign(
            "HEAD",
            &url,
            &path,
            &payload_hash(b""),
            None,
            chrono::Utc::now(),
        );
        let response = request
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
        Ok(ObjectInfo {
            key: key.to_string(),
            size,
            modified: None,
            content_type,
        })
    }

    fn capabilities(&self) -> Capabilities {
        // 配了公网前缀就能直接给直链
        Capabilities::remote(self.config.public_base_url.is_some())
    }

    async fn health(&self) -> Result<String> {
        let (url, path) = self.request_target("");
        let head_url = format!("{url}/");
        let request = self.sign(
            "HEAD",
            &head_url,
            &format!("{path}/"),
            &payload_hash(b""),
            None,
            chrono::Utc::now(),
        );
        let response = request
            .send()
            .await
            .map_err(|err| StorageError::remote(DRIVER, 0, err.to_string()))?;
        match response.status().as_u16() {
            200 => Ok(format!(
                "连通 · {}/{} · {}",
                self.config.endpoint, self.config.bucket, self.config.region
            )),
            403 => Err(StorageError::config(
                DRIVER,
                "鉴权失败，检查 AccessKey / SecretKey 与区域",
            )),
            404 => Err(StorageError::config(DRIVER, "桶不存在或权限不足")),
            status => Err(StorageError::remote(DRIVER, status, "连通性检查失败")),
        }
    }

    async fn link_view(&self, _key: &str, _view_rel: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver() -> S3Driver {
        S3Driver::new(
            S3Config {
                endpoint: "https://oss-cn-hangzhou.aliyuncs.com".into(),
                region: "cn-hangzhou".into(),
                bucket: "pixora".into(),
                access_key: "AKIDEXAMPLE".into(),
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
                prefix: "images".into(),
                path_style: true,
                public_base_url: None,
            },
            Arc::new(reqwest::Client::new()),
        )
    }

    #[test]
    fn uri_encoding_matches_sigv4_rules() {
        assert_eq!(uri_encode("abc-_.~", true), "abc-_.~");
        assert_eq!(uri_encode("a b", true), "a%20b");
        assert_eq!(uri_encode("旅行", true), "%E6%97%85%E8%A1%8C");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a/b", false), "a/b");
    }

    /// AWS 官方文档里的派生密钥测试向量（`docs.aws.amazon.com` SigV4 示例）。
    #[test]
    fn signing_key_derivation_matches_aws_example() {
        // 密钥派生：用 AWS 文档给出的中间值反推签名密钥
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let signature = sign_v4(
            secret,
            "20150830",
            "us-east-1",
            // 用文档里的 iam 服务向量核对派生密钥链；s3 走的是同一条链
            "iam",
            b"AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\nf536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59",
        );
        // 该向量的期望签名（AWS 文档「签名计算」示例）
        assert_eq!(
            signature,
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn object_key_joins_prefix() {
        let driver = driver();
        assert_eq!(driver.object_key("blobs/a3/x.webp"), "images/blobs/a3/x.webp");
    }

    #[test]
    fn path_style_url_contains_bucket() {
        let driver = driver();
        // request_target 收到的是已经拼过前缀的对象键
        let (url, path) = driver.request_target(&driver.object_key("blobs/a3/x.webp"));
        assert_eq!(
            url,
            "https://oss-cn-hangzhou.aliyuncs.com/pixora/images/blobs/a3/x.webp"
        );
        assert_eq!(path, "/pixora/images/blobs/a3/x.webp");
    }

    #[test]
    fn virtual_host_style_moves_bucket_into_host() {
        let mut driver = driver();
        driver.config.path_style = false;
        let (url, path) = driver.request_target(&driver.object_key("a.webp"));
        assert!(url.starts_with("https://pixora.oss-cn-hangzhou.aliyuncs.com/"), "{url}");
        assert_eq!(path, "/images/a.webp");
    }
}
