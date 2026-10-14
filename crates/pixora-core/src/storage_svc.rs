//! 存储服务：后端配置 → 驱动实例 → 读写与容量。
//!
//! 三件事在这里收口：
//!
//!   1. **装配**：读 `storage_backends`，把每行变成一个驱动实例注册进注册表。
//!      密钥字段（Token、密码）在库里是密文，装配时解密。
//!   2. **回退**：远端后端失败时自动落本地，并把 `fallback_message` 带回去给前端提示
//!      （设计稿明确要求这个行为，用户得知道自己那张图其实存在服务器本地）。
//!   3. **容量**：按 `auto` / `manual` / `hidden` 三模式给出「该显示什么」。
//!      `auto` 探测到网络挂载（ossfs 之类）时不给数字，因为 `statfs` 返回的是伪值。

use std::path::PathBuf;
use std::sync::Arc;

use pixora_repo::{model::StorageBackend, Repo};
use pixora_storage::names as driver_names;
use pixora_storage::{
    CapacityProbe, DatabaseDriver, FsKind, GitHubDriver, LocalDriver, S3Config, S3Driver,
    SqlDialect, StorageDriver, StorageRegistry, TelegramConfig, TelegramDriver, WebDavConfig,
    WebDavDriver,
};

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::settings::{keys, Settings};

/// 需要加密存放的配置字段名。写入时加密，装配时解密。
pub const SECRET_FIELDS: &[&str] = &["token", "password", "bot_token", "secret_key", "access_key"];

/// 容量的对外形态。
#[derive(Debug, Clone)]
pub struct CapacityInfo {
    pub mode: String,
    pub total: Option<i64>,
    pub used: i64,
    pub source: String,
    pub note: String,
    pub probe: CapacityProbeLike,
}

/// 探测结果的可序列化形式（界面要展示 `f_type`）。
#[derive(Debug, Clone)]
pub struct CapacityProbeLike {
    pub fs_kind: String,
    pub fs_name: String,
    pub fs_type: String,
    pub trustworthy: bool,
    pub note: String,
}

impl From<CapacityProbe> for CapacityProbeLike {
    fn from(probe: CapacityProbe) -> Self {
        let kind = match probe.fs_kind {
            FsKind::Local => "local",
            FsKind::Network => "network",
            FsKind::Unknown => "unknown",
        };
        Self {
            fs_kind: kind.to_string(),
            fs_name: probe.fs_name.clone(),
            fs_type: format!("0x{:x}", probe.fs_type),
            trustworthy: probe.fs_kind.capacity_trustworthy(),
            note: pixora_storage::capacity::describe(&probe),
        }
    }
}

pub struct StorageService {
    repo: Repo,
    settings: Settings,
    config: Config,
    http: Arc<reqwest::Client>,
    registry: StorageRegistry,
}

impl StorageService {
    pub fn new(
        repo: Repo,
        settings: Settings,
        config: Config,
        http: Arc<reqwest::Client>,
    ) -> Self {
        Self {
            repo,
            settings,
            config,
            http,
            registry: StorageRegistry::new(),
        }
    }

    pub fn registry(&self) -> &StorageRegistry {
        &self.registry
    }

    /// 本地数据目录。物理路径只来自环境变量（ADR-0003）。
    pub fn local_root(&self) -> PathBuf {
        self.config.data_dir.clone()
    }

    /// 按库里的后端配置重建注册表。
    pub async fn reload(&self) -> Result<()> {
        let backends = self.repo.list_backends().await?;
        self.registry.clear();

        if backends.is_empty() {
            return self.install_local_fallback().await;
        }

        let mut default_key: Option<String> = None;
        for backend in &backends {
            let key = pixora_storage::registry::instance_key(&backend.driver, Some(backend.id));
            match self.build_driver(backend).await {
                Ok(driver) => {
                    if backend.is_default {
                        default_key = Some(key.clone());
                    }
                    self.registry.register(key, driver);
                }
                Err(err) => {
                    // 单个后端装配失败不该拖垮整个服务：日志记下来，其余后端照常工作
                    tracing::warn!(
                        backend_id = backend.id,
                        driver = %backend.driver,
                        error = %err,
                        "存储后端装配失败，跳过该实例"
                    );
                }
            }
        }

        // 默认后端：库里的标记优先，否则退回本地
        match default_key {
            Some(key) => self.registry.set_default(&key),
            None => {
                let local_key = pixora_storage::registry::instance_key(driver_names::LOCAL, None);
                if self.registry.get(&local_key).is_none() {
                    self.install_local_fallback().await?;
                }
                self.registry.set_default(&local_key);
            }
        }
        Ok(())
    }

    /// 兜底：只装本地目录驱动。全新实例（一张配置表都没有）走这条路。
    pub async fn install_local_fallback(&self) -> Result<()> {
        let root = self.local_root();
        std::fs::create_dir_all(&root).map_err(|err| {
            AppError::internal(format!("数据目录 {} 不可用：{err}", root.display()))
        })?;
        let driver = Arc::new(LocalDriver::new(root));
        let key = driver_names::LOCAL;
        self.registry.register(key, driver);
        self.registry.set_default(key);
        Ok(())
    }

    /// 一个后端行 → 一个驱动实例。
    async fn build_driver(&self, backend: &StorageBackend) -> Result<Arc<dyn StorageDriver>> {
        let config = self.decrypt_config(&backend.config)?;
        let instance = pixora_storage::registry::instance_key(&backend.driver, Some(backend.id));

        let driver: Arc<dyn StorageDriver> = match backend.driver.as_str() {
            driver_names::LOCAL => {
                let root = self.local_root();
                std::fs::create_dir_all(&root).ok();
                Arc::new(LocalDriver::new(root))
            }
            driver_names::DATABASE => {
                let dialect = match self.config.db_driver {
                    pixora_repo::DbDriver::Postgres => SqlDialect::Postgres,
                    pixora_repo::DbDriver::MySql => SqlDialect::MySql,
                    pixora_repo::DbDriver::Sqlite => SqlDialect::Sqlite,
                };
                Arc::new(
                    DatabaseDriver::new(self.repo.db().pool().clone(), dialect)
                        .with_instance(instance),
                )
            }
            driver_names::GITHUB => {
                let parsed: pixora_storage::github::GitHubConfig = serde_json::from_value(config)
                    .map_err(|err| {
                        AppError::bad_request(format!("GitHub 后端配置不完整：{err}"))
                    })?;
                Arc::new(GitHubDriver::new(parsed, self.http.clone()).with_instance(instance))
            }
            driver_names::WEBDAV => {
                let parsed: WebDavConfig = serde_json::from_value(config)
                    .map_err(|err| AppError::bad_request(format!("WebDAV 后端配置不完整：{err}")))?;
                Arc::new(WebDavDriver::new(parsed, self.http.clone()).with_instance(instance))
            }
            driver_names::TELEGRAM => {
                let parsed: TelegramConfig = serde_json::from_value(config)
                    .map_err(|err| AppError::bad_request(format!("Telegram 后端配置不完整：{err}")))?;
                Arc::new(TelegramDriver::new(parsed)?.with_instance(instance))
            }
            driver_names::S3 => {
                let parsed: S3Config = serde_json::from_value(config)
                    .map_err(|err| AppError::bad_request(format!("S3 后端配置不完整：{err}")))?;
                Arc::new(S3Driver::new(parsed, self.http.clone()).with_instance(instance))
            }
            other => {
                return Err(AppError::bad_request(format!("未知的存储驱动：{other}")));
            }
        };
        Ok(driver)
    }

    /// 解密配置里的敏感字段。非敏感字段原样保留。
    fn decrypt_config(&self, config: &serde_json::Value) -> Result<serde_json::Value> {
        let Some(object) = config.as_object() else {
            return Ok(serde_json::json!({}));
        };
        let mut out = serde_json::Map::new();
        for (key, value) in object {
            let is_secret = SECRET_FIELDS.contains(&key.as_str());
            let text = value.as_str().unwrap_or_default();
            if is_secret && text.starts_with("v1.") {
                // 密文里的内容只有主密钥能解；解不开就明确报错，
                // 而不是把密文当明文喂给驱动（那样会得到一个 401 而看不出原因）
                let master = self.config.require_master_key()?;
                let plain = master.decrypt_str(text)?;
                out.insert(key.clone(), serde_json::Value::String(plain));
            } else {
                out.insert(key.clone(), value.clone());
            }
        }
        Ok(serde_json::Value::Object(out))
    }

    /// 默认驱动。上传的新文件往这里写。
    pub fn default_driver(&self) -> Result<Arc<dyn StorageDriver>> {
        self.registry
            .default_driver()
            .ok_or_else(|| AppError::StorageUnavailable("没有可用的默认存储后端".into()))
    }

    /// 按「当初存在哪」找回驱动。
    pub fn resolve(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
    ) -> Result<Arc<dyn StorageDriver>> {
        self.registry.resolve(driver_name, backend_id).ok_or_else(|| {
            AppError::StorageUnavailable(format!("存储后端 {driver_name} 未装配或已删除"))
        })
    }

    /// 写入对象，远端失败自动回退本地。
    ///
    /// 返回 `(实际落点, 回退提示)`。回退提示非空时前端要告诉用户
    /// 「远端后端失败了，这张图存在服务器本地」——不说的话用户会以为
    /// 自己的图进了 GitHub，排查时完全找不到方向。
    pub async fn store_object(
        &self,
        key: &str,
        reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        size: u64,
        mime: &str,
        prefer_remote: bool,
    ) -> Result<(ResolvedDriver, Option<String>)> {
        let default = self.default_driver()?;

        if !prefer_remote || default.name() == driver_names::LOCAL {
            let stored = default
                .put(key, reader, size, mime)
                .await
                .map_err(|err| AppError::from(err))?;
            return Ok((
                ResolvedDriver {
                    driver: default,
                    key: stored.key,
                },
                None,
            ));
        }

        // 远端优先：失败且可重试就落本地
        let driver_name = default.name();
        match read_fully(reader, size).await {
            Ok(bytes) => match default.put(key, Box::new(std::io::Cursor::new(bytes.clone())), size, mime).await {
                Ok(stored) => Ok((
                    ResolvedDriver {
                        driver: default,
                        key: stored.key,
                    },
                    None,
                )),
                Err(err) if err.is_retryable() => {
                    tracing::warn!(driver = driver_name, error = %err, "远端存储失败，回退本地");
                    let local = self.local_driver()?;
                    let stored = local
                        .put(key, Box::new(std::io::Cursor::new(bytes)), size, mime)
                        .await
                        .map_err(AppError::from)?;
                    Ok((
                        ResolvedDriver {
                            driver: local,
                            key: stored.key,
                        },
                        Some(format!(
                            "远端后端 {} 暂时不可用（{}），这张图已存到服务器本地",
                            driver_name,
                            brief(&err.to_string())
                        )),
                    ))
                }
                Err(err) => Err(AppError::from(err)),
            },
            Err(err) => Err(err),
        }
    }

    fn local_driver(&self) -> Result<Arc<dyn StorageDriver>> {
        self.registry
            .get(driver_names::LOCAL)
            .or_else(|| self.registry.get_by_driver(driver_names::LOCAL))
            .ok_or_else(|| AppError::StorageUnavailable("本地回退目录不可用".into()))
    }

    pub async fn open_object(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
        key: &str,
    ) -> Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        let driver = self.resolve(driver_name, backend_id)?;
        Ok(driver.get(key).await?)
    }

    pub async fn delete_object(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
        key: &str,
    ) -> Result<()> {
        let driver = self.resolve(driver_name, backend_id)?;
        driver.delete(key).await.map_err(AppError::from)
    }

    /// 在「用户/相册」镜像树里建硬链接。返回 false 表示文件系统不支持，
    /// 上层降级为只维护数据库虚拟树。
    pub async fn link_view(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
        key: &str,
        view_rel: &str,
    ) -> Result<bool> {
        let driver = self.resolve(driver_name, backend_id)?;
        if !driver.capabilities().hardlink {
            return Ok(false);
        }
        Ok(driver.link_view(key, view_rel).await?)
    }

    pub async fn unlink_view(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
        view_rel: &str,
    ) -> Result<()> {
        let driver = self.resolve(driver_name, backend_id)?;
        driver.unlink_view(view_rel).await.map_err(AppError::from)
    }

    /// 清掉已经空了的镜像目录（本地磁盘才有真实目录，远端驱动是空实现）。
    pub async fn remove_view_dir(
        &self,
        driver_name: &str,
        backend_id: Option<i64>,
        view_rel: &str,
    ) -> Result<()> {
        let driver = self.resolve(driver_name, backend_id)?;
        driver
            .remove_view_dir(view_rel)
            .await
            .map_err(AppError::from)
    }

    /// 本地文件系统路径。有它就能让 Caddy 直接发文件，进程不碰数据。
    pub async fn local_path(&self, driver_name: &str, backend_id: Option<i64>, key: &str) -> Option<PathBuf> {
        let driver = self.resolve(driver_name, backend_id).ok()?;
        driver.local_path(key)
    }

    /// 容量三模式。这里是设计稿点名要修的那个坑：
    /// `auto` 模式下探测到网络挂载就不给数字，`manual` 用手填总额，
    /// `hidden` 干脆不显示。已用量一律由库里的 blob 汇总，不依赖文件系统。
    pub async fn capacity(&self) -> Result<CapacityInfo> {
        let mode = self.settings.capacity_mode().await?;
        let used = self.repo.sum_blob_bytes().await?;

        if mode == "hidden" {
            return Ok(CapacityInfo {
                mode,
                total: None,
                used,
                source: "hidden".into(),
                note: "容量展示已关闭".into(),
                probe: CapacityProbeLike {
                    fs_kind: "unknown".into(),
                    fs_name: "未探测".into(),
                    fs_type: "0x0".into(),
                    trustworthy: false,
                    note: "容量展示已关闭".into(),
                },
            });
        }

        let probe = pixora_storage::capacity::probe(&self.local_root())
            .unwrap_or_else(|_| CapacityProbe::unknown("探测失败"));

        if mode == "manual" {
            let total = self.settings.manual_total().await?;
            let updated_at = self
                .repo
                .get_setting(keys::CAPACITY_MANUAL_TOTAL)
                .await
                .ok()
                .flatten()
                .map(|setting| setting.updated_at.format("%Y-%m-%d %H:%M").to_string());
            return Ok(CapacityInfo {
                mode,
                total,
                used,
                source: "manual".into(),
                note: updated_at
                    .map(|at| format!("系统总额为管理员手填（{at}），已用量按库内文件实时汇总"))
                    .unwrap_or_else(|| "系统总额为管理员手填，已用量按库内文件实时汇总".into()),
                probe: probe.into(),
            });
        }

        // auto：只有本地文件系统的探测值可信
        if !probe.fs_kind.capacity_trustworthy() {
            let manual = self.settings.manual_total().await?;
            return Ok(CapacityInfo {
                mode,
                total: manual,
                used,
                source: if manual.is_some() {
                    "auto→manual"
                } else {
                    "auto→unavailable"
                }
                .into(),
                note: format!(
                    "{}。容量已降级为{}",
                    pixora_storage::capacity::describe(&probe),
                    if manual.is_some() {
                        "管理员手填总额"
                    } else {
                        "不可展示，请到「配额与容量」手填总额"
                    }
                ),
                probe: probe.into(),
            });
        }

        Ok(CapacityInfo {
            mode,
            total: Some(probe.total as i64),
            used,
            source: "auto".into(),
            note: pixora_storage::capacity::describe(&probe),
            probe: probe.into(),
        })
    }

    /// 后端连通性自检。
    pub async fn test_backend(&self, backend_id: i64) -> Result<String> {
        let backend = self
            .repo
            .find_backend(backend_id)
            .await?
            .ok_or_else(|| AppError::not_found("存储后端"))?;
        let driver = self.build_driver(&backend).await?;
        let message = driver.health().await?;
        Ok(message)
    }

    /// 把库里的配置加密后写回（敏感字段加密，其余原样）。
    pub fn encrypt_config(&self, config: &serde_json::Value) -> Result<String> {
        let mut out = serde_json::Map::new();
        if let Some(object) = config.as_object() {
            for (key, value) in object {
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                if SECRET_FIELDS.contains(&key.as_str()) {
                    if text.trim().is_empty() {
                        // 留空表示「不改」，写回时保持原值由调用方处理
                        out.insert(key.clone(), serde_json::Value::String(String::new()));
                        continue;
                    }
                    // 已经是密文就不再加密一次（界面上回显的是掩码，不会带密文）
                    if text.starts_with("v1.") {
                        out.insert(key.clone(), serde_json::Value::String(text));
                        continue;
                    }
                    let key_ref = self.config.require_master_key()?;
                    let encrypted = key_ref.encrypt_str(&text)?;
                    out.insert(key.clone(), serde_json::Value::String(encrypted));
                } else {
                    out.insert(key.clone(), serde_json::Value::String(text));
                }
            }
        }
        Ok(serde_json::to_string(&serde_json::Value::Object(out))
            .map_err(|err| AppError::internal(err.to_string()))?)
    }

    /// 把密文字段替换成掩码，用于把配置回显给管理界面。
    pub fn mask_config(config: &serde_json::Value) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if let Some(object) = config.as_object() {
            for (key, value) in object {
                if SECRET_FIELDS.contains(&key.as_str()) {
                    let is_set = value.as_str().map(|text| !text.is_empty()).unwrap_or(true);
                    out.insert(key.clone(), serde_json::Value::Bool(is_set));
                } else {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        serde_json::Value::Object(out)
    }
}

/// 写入结果：实际落点的驱动与它的键（远端驱动可能改写键）。
pub struct ResolvedDriver {
    pub driver: Arc<dyn StorageDriver>,
    pub key: String,
}

impl ResolvedDriver {
    pub fn driver_name(&self) -> &'static str {
        self.driver.name()
    }

    pub fn backend_id(&self) -> Option<i64> {
        self.driver
            .instance_key()
            .split_once('#')
            .and_then(|(_, id)| id.parse::<i64>().ok())
    }
}

/// 把流读进内存。远端驱动（GitHub 的 base64、Telegram 的 multipart）都需要整份内容。
async fn read_fully(
    mut reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    expected: u64,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut buffer = Vec::with_capacity(expected as usize);
    reader
        .read_to_end(&mut buffer)
        .await
        .map_err(|err| AppError::internal(format!("读取上传流失败：{err}")))?;
    Ok(buffer)
}

fn brief(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.chars().count() > 80 {
        format!("{}…", trimmed.chars().take(80).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_fields_are_masked_for_display() {
        let config = serde_json::json!({
            "token": "v1.abc",
            "owner": "example-org",
            "repo": "img-bed",
        });
        let masked = StorageService::mask_config(&config);
        assert_eq!(masked["token"], serde_json::Value::Bool(true));
        assert_eq!(masked["owner"], "example-org");
    }

    #[test]
    fn probe_like_formats_fs_type_as_hex() {
        let probe = CapacityProbe {
            total: 100,
            available: 40,
            fs_kind: FsKind::Network,
            fs_type: 0x65735546,
            fs_name: "fuse".into(),
        };
        let like: CapacityProbeLike = probe.into();
        assert_eq!(like.fs_type, "0x65735546");
        assert!(!like.trustworthy);
        assert!(like.note.contains("伪值") || like.note.contains("降级"));
    }
}
