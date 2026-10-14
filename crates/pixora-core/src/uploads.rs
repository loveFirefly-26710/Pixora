//! 上传服务：预检、落盘、去重、建引用。
//!
//! 这是整套设计里最省资源的一环，顺序很关键：
//!
//! ```text
//! 客户端算哈希 → 批量预检 → 只传服务器没有的 → 服务端流式收 → 落盘 → 建引用
//! ```
//!
//! 三件事值得专门说明：
//!
//!   1. **不把文件读进内存**。收到字节先流式写进数据目录下的临时文件（顺手算哈希），
//!      落盘成功后由存储驱动从临时文件读——对象存储驱动自己要整份内容，那是它的约束，
//!      但本地驱动与数据库驱动都不需要。
//!   2. **始终服务端算一遍哈希**。设计稿说「客户端带上哈希就不必再算」是为了省 CPU，
//!      但我们本来就要把字节过一遍临时文件，顺路 tee 进 SHA-256 的成本极小；
//!      而信任客户端哈希的代价是 CAS 可能被写坏（客户端声称存在、实际内容不同）。
//!      两者不一致时以实测为准，并留下日志线索。
//!   3. **去重跳过的文件不占上传额度**。它与内容寻址存储的设计咬合：没写盘就不该算配额。
//!      `upload_jobs.dedup` 上区分，限流统计只看 `dedup = false` 的行。

use std::path::PathBuf;

use chrono::Utc;
use pixora_repo::model::{storage_kind, visibility, Image, User};
use pixora_repo::repo::jobs::NewJob;
use pixora_repo::repo::uploads::NewBlob;
use pixora_storage::cas::{cas_key, view_rel_path};

use crate::audit::actions;
use crate::error::{AppError, Result};
use crate::media;
use crate::AppState;

/// 客户端提交的一次上传。
#[derive(Debug, Clone)]
pub struct UploadInput {
    pub album_id: Option<i64>,
    pub tags: Vec<String>,
    pub visibility: String,
    pub original_name: String,
    /// 客户端算好的哈希（可选）
    pub client_sha256: Option<String>,
    pub content_type: Option<String>,
}

impl UploadInput {
    pub fn new(original_name: impl Into<String>) -> Self {
        Self {
            album_id: None,
            tags: Vec::new(),
            visibility: visibility::PRIVATE.to_string(),
            original_name: original_name.into(),
            client_sha256: None,
            content_type: None,
        }
    }

    fn normalized_visibility(&self) -> String {
        if self.visibility.eq_ignore_ascii_case(visibility::PUBLIC) {
            visibility::PUBLIC.to_string()
        } else {
            visibility::PRIVATE.to_string()
        }
    }
}

/// 一次上传的结果。
#[derive(Debug, Clone)]
pub struct UploadOutcome {
    pub image_id: i64,
    pub job_id: i64,
    pub sha256: String,
    pub ext: String,
    pub size: i64,
    pub dedup: bool,
    pub storage_type: String,
    pub fallback_message: Option<String>,
    pub tags: Vec<String>,
    pub url: String,
    pub thumb_url: Option<String>,
}

/// 批量预检的结果。
#[derive(Debug, Clone)]
pub struct HashCheckOutcome {
    pub exist: Vec<String>,
    pub missing: Vec<String>,
}

impl AppState {
    /// 批量预检：客户端一次发上千个哈希，服务端只回哪些已存在。
    ///
    /// 这是省带宽的关键一步——重复文件连上传请求都不发。
    pub async fn check_hashes(&self, hashes: &[String]) -> Result<HashCheckOutcome> {
        if hashes.is_empty() {
            return Ok(HashCheckOutcome {
                exist: Vec::new(),
                missing: Vec::new(),
            });
        }
        if hashes.len() > 1000 {
            return Err(AppError::bad_request("单次最多预检 1000 个哈希"));
        }
        for hash in hashes {
            if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(AppError::bad_request(format!("哈希格式不正确：{hash}")));
            }
        }
        let normalized: Vec<String> = hashes.iter().map(|hash| hash.to_lowercase()).collect();
        let check = self.repo.existing_hashes(&normalized).await?;
        Ok(HashCheckOutcome {
            exist: check.exist,
            missing: check.missing,
        })
    }

    /// 收下一个文件并建立引用。
    ///
    /// `reader` 是请求体的文件部分；`thumb` 是客户端生成的缩略图（可选），
    /// 第二项是它的字节数（multipart 不一定给得准，只作为预分配提示）。
    pub async fn upload_file(
        &self,
        user: &User,
        input: UploadInput,
        reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        thumb: Option<(Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64)>,
        ip: Option<&str>,
    ) -> Result<UploadOutcome> {
        self.ensure_ip_allowed(ip, crate::ipguard::scope::UPLOAD).await?;

        let upload_settings = self.settings.upload().await?;
        let policy = self.repo.effective_policy(user).await?;

        // 背压：服务端队列深度超阈值就拒，客户端按 Retry-After 退避
        let depth = self.repo.queue_depth().await?;
        if upload_settings.queue_depth > 0 && depth.total() >= upload_settings.queue_depth {
            return Err(AppError::QueueFull { retry_after: 5 });
        }

        // 先把字节落到临时文件：大小上限在流的过程中就能判，不必等收完
        let temp = TempUpload::begin(&self.storage.local_root()).await?;
        let collected = temp
            .write_all(reader, upload_settings.max_file_bytes)
            .await?;

        if collected.size == 0 {
            return Err(AppError::InvalidImage("文件为空".into()));
        }

        // 格式校验：白名单只是第一道，真实格式看魔数
        let sniffed = media::sniff(&collected.head)
            .ok_or_else(|| AppError::InvalidImage("无法识别的图片格式".into()))?;
        let ext = sniffed.ext.to_string();
        let normalized_ext = media::normalize_ext(&ext);
        let declared_ext = media::normalize_ext(&media::ext_from_name(&input.original_name));
        if !upload_settings.formats.contains(&normalized_ext) {
            return Err(AppError::UnsupportedFormat(ext.clone()));
        }
        // 扩展名与真实格式不符时以真实格式为准（有人把 .png 改成 .jpg）
        if !declared_ext.is_empty() && declared_ext != normalized_ext {
            tracing::debug!(
                declared = %declared_ext,
                detected = %normalized_ext,
                "扩展名与真实格式不一致，按真实格式处理"
            );
        }

        // 尺寸与像素上限：解压炸弹防线
        let (width, height) = media::dimensions(&collected.metadata_bytes(), sniffed.mime)
            .unwrap_or((0, 0));
        if width > 0 && height > 0 {
            media::check_pixels(width, height, upload_settings.max_pixels as u64)?;
        }

        // 配额与限流：都在拿到真实大小之后判
        crate::quota::ensure_quota_available(self, user, collected.size as i64).await?;
        self.limits
            .check_upload(user, &policy, 1, collected.size as i64)
            .await?;

        let job = self
            .repo
            .create_job(&NewJob {
                owner_id: user.id,
                album_id: input.album_id,
                sha256: Some(collected.sha256.clone()),
                original_name: input.original_name.clone(),
                size: collected.size as i64,
            })
            .await?;
        self.repo.mark_job_running(job.id).await?;

        let result = self
            .store_and_link(
                user,
                &input,
                &collected,
                sniffed.mime,
                &ext,
                width,
                height,
                thumb,
            )
            .await;

        match result {
            Ok(outcome) => {
                self.repo
                    .mark_job_done(job.id, outcome.image_id, &outcome.sha256, outcome.dedup)
                    .await?;
                // 只有真正落盘的新文件才消耗上传额度（去重跳过的不算）
                if !outcome.dedup {
                    self.limits
                        .record_upload(user.id, 1, collected.size as i64)
                        .await?;
                }
                self.audit_detail(
                    actions::UPLOAD,
                    Some(user.id),
                    &outcome.sha256,
                    serde_json::json!({
                        "dedup": outcome.dedup,
                        "size": outcome.size,
                        "ext": outcome.ext,
                        "storage": outcome.storage_type,
                    }),
                    ip,
                )
                .await;
                Ok(UploadOutcome {
                    job_id: job.id,
                    ..outcome
                })
            }
            Err(err) => {
                let _ = self.repo.mark_job_failed(job.id, &err.message()).await;
                if err.status() >= 500 {
                    self.audit_detail(
                        actions::UPLOAD_FAIL,
                        Some(user.id),
                        &input.original_name,
                        serde_json::json!({ "code": err.code(), "message": err.message() }),
                        ip,
                    )
                    .await;
                    let _ = self
                        .ip_guard
                        .record_failure(ip, actions::UPLOAD_FAIL)
                        .await;
                }
                Err(err)
            }
        }
        // 临时文件由 TempUpload 的 Drop 清掉
    }

    /// 落盘（或命中已有实体）并建立图片引用。
    #[allow(clippy::too_many_arguments)]
    async fn store_and_link(
        &self,
        user: &User,
        input: &UploadInput,
        collected: &Collected,
        mime: &str,
        ext: &str,
        width: u32,
        height: u32,
        thumb: Option<(Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64)>,
    ) -> Result<UploadOutcome> {
        let size = collected.size as i64;

        if let Some(client_hash) = input.client_sha256.as_deref() {
            let client_hash = client_hash.trim().to_lowercase();
            if client_hash.len() == 64 && client_hash != collected.sha256 {
                tracing::warn!(
                    client = %client_hash,
                    computed = %collected.sha256,
                    "客户端哈希与实测不一致，按实测结果处理"
                );
            }
        }

        let key = cas_key(&collected.sha256, ext);
        let existing = self
            .repo
            .find_blob_by_sha(&collected.sha256, storage_kind::ORIGINAL)
            .await?;

        let (blob, dedup, fallback_message) = match existing {
            Some(blob) => {
                // 命中去重：文件已在服务器上，不重复占盘也不消耗带宽
                (blob, true, None)
            }
            None => {
                let reader = collected.open().await?;
                let prefer_remote = self
                    .storage
                    .default_driver()
                    .map(|driver| driver.name() != pixora_storage::names::LOCAL)
                    .unwrap_or(false);
                let (resolved, fallback) = self
                    .storage
                    .store_object(&key, reader, collected.size, mime, prefer_remote)
                    .await?;
                let stored_key = resolved.key.clone();
                let blob = self
                    .repo
                    .insert_blob_if_absent(&NewBlob {
                        sha256: collected.sha256.clone(),
                        kind: storage_kind::ORIGINAL.to_string(),
                        size,
                        mime_type: mime.to_string(),
                        ext: ext.to_string(),
                        width: Some(width as i32).filter(|value| *value > 0),
                        height: Some(height as i32).filter(|value| *value > 0),
                        storage_driver: resolved.driver_name().to_string(),
                        storage_key: stored_key,
                        backend_id: resolved.backend_id(),
                    })
                    .await?;
                (blob, false, fallback)
            }
        };

        // 缩略图也是内容寻址的一份实体，同样去重
        let thumb_blob_id = match thumb {
            Some((reader, thumb_size)) => self.store_thumb(reader, thumb_size).await.unwrap_or(None),
            None => None,
        };

        let album_slug = match input.album_id {
            Some(album_id) => {
                let album = self
                    .repo
                    .find_album_by_id(album_id)
                    .await?
                    .ok_or_else(|| AppError::not_found("相册"))?;
                if album.owner_id != user.id {
                    return Err(AppError::forbidden("相册不属于当前账号"));
                }
                Some(album.slug.clone())
            }
            None => None,
        };

        let image = self
            .repo
            .create_image_reference(
                &pixora_repo::repo::images::NewImage {
                    owner_id: user.id,
                    album_id: input.album_id,
                    blob_id: blob.id,
                    thumb_blob_id,
                    original_name: input.original_name.clone(),
                    title: None,
                    visibility: input.normalized_visibility(),
                },
                size,
                &input.tags,
            )
            .await?;

        // 镜像目录树：文件系统支持硬链接才建，不支持就只维护数据库虚拟树
        if let Some(slug) = album_slug.as_deref() {
            self.link_view_for(
                &blob.storage_driver,
                blob.backend_id,
                &blob.storage_key,
                &user.uid,
                slug,
                &collected.sha256,
                ext,
            )
            .await;
        }

        let url = self.direct_url(&collected.sha256, ext, false).await;
        let thumb_url = if image.thumb_blob_id.is_some() {
            Some(self.direct_url(&collected.sha256, ext, true).await)
        } else {
            None
        };

        Ok(UploadOutcome {
            image_id: image.id,
            job_id: 0,
            sha256: collected.sha256.clone(),
            ext: ext.to_string(),
            size,
            dedup,
            storage_type: blob.storage_driver.clone(),
            fallback_message,
            tags: input.tags.clone(),
            url,
            thumb_url,
        })
    }

    /// 存缩略图。同样内容寻址 + 去重，失败只返回 None（不毁掉整次上传）。
    async fn store_thumb(
        &self,
        mut reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        size_hint: i64,
    ) -> Result<Option<i64>> {
        use tokio::io::AsyncReadExt;

        // 缩略图都很小，直接读进内存即可（客户端生成的是 WebP，最长边 400px）
        let mut buffer = Vec::with_capacity(size_hint.clamp(0, 2 * 1024 * 1024) as usize);
        reader.read_to_end(&mut buffer).await?;
        if buffer.is_empty() {
            return Ok(None);
        }
        if buffer.len() > 2 * 1024 * 1024 {
            return Err(AppError::bad_request("缩略图过大"));
        }

        let sniffed = media::sniff(&buffer)
            .ok_or_else(|| AppError::InvalidImage("缩略图格式无法识别".into()))?;
        if !matches!(sniffed.mime, "image/webp" | "image/png" | "image/jpeg") {
            return Err(AppError::UnsupportedFormat(sniffed.ext.to_string()));
        }

        let sha256 = pixora_crypto::hash::sha256_hex(&buffer);
        let ext = sniffed.ext;
        if let Some(existing) = self
            .repo
            .find_blob_by_sha(&sha256, storage_kind::THUMB)
            .await?
        {
            return Ok(Some(existing.id));
        }

        let key = cas_key(&sha256, ext);
        let (resolved, _) = self
            .storage
            .store_object(
                &key,
                Box::new(std::io::Cursor::new(buffer.clone())),
                buffer.len() as u64,
                sniffed.mime,
                false,
            )
            .await?;

        // 先把驱动名与实例 id 取出来，后面 resolved.key 会被移动走
        let driver_name = resolved.driver_name().to_string();
        let backend_id = resolved.backend_id();
        let blob = self
            .repo
            .insert_blob_if_absent(&NewBlob {
                sha256,
                kind: storage_kind::THUMB.to_string(),
                size: buffer.len() as i64,
                mime_type: sniffed.mime.to_string(),
                ext: ext.to_string(),
                width: None,
                height: None,
                storage_driver: driver_name,
                storage_key: resolved.key,
                backend_id,
            })
            .await?;
        Ok(Some(blob.id))
    }

    /// 建镜像树的链接。失败只记日志——虚拟目录树照常工作。
    #[allow(clippy::too_many_arguments)]
    pub async fn link_view_for(
        &self,
        driver: &str,
        backend_id: Option<i64>,
        storage_key: &str,
        uid: &str,
        album_slug: &str,
        sha256: &str,
        ext: &str,
    ) {
        let view = view_rel_path(uid, album_slug, sha256, ext);
        match self
            .storage
            .link_view(driver, backend_id, storage_key, &view)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(driver, "文件系统不支持硬链接，镜像目录树降级为数据库虚拟树");
            }
            Err(err) => tracing::warn!(error = %err, "建立镜像链接失败"),
        }
    }

    /// 删镜像树里的链接。物理文件由引用计数归零后的 GC 处理。
    pub async fn unlink_view_for(
        &self,
        driver: &str,
        backend_id: Option<i64>,
        uid: &str,
        album_slug: &str,
        sha256: &str,
        ext: &str,
    ) {
        let view = view_rel_path(uid, album_slug, sha256, ext);
        if let Err(err) = self.storage.unlink_view(driver, backend_id, &view).await {
            tracing::debug!(error = %err, "删除镜像链接失败（可能本来就没有）");
        }
    }

    /// 把已经落盘的引用补齐镜像树链接（对账或迁移时用）。
    pub async fn relink_view(&self, image: &Image, uid: &str, album_slug: &str) {
        let Ok(Some(blob)) = self.repo.find_blob_by_id(image.blob_id).await else {
            return;
        };
        self.link_view_for(
            &blob.storage_driver,
            blob.backend_id,
            &blob.storage_key,
            uid,
            album_slug,
            &blob.sha256,
            &blob.ext,
        )
        .await;
    }

    /// 兼容旧版本图床调用的响应体，字段名与老脚本对齐。
    pub fn legacy_response(outcome: &UploadOutcome) -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "message": if outcome.dedup { "服务器已有相同文件，未重复上传" } else { "上传成功" },
            "url": outcome.url,
            "storage_type": outcome.storage_type,
            "tags": outcome.tags.join(","),
        })
    }
}

/// 临时文件的生命周期封装：构造时创建，Drop 时删除。
struct TempUpload {
    path: PathBuf,
    dir: PathBuf,
}

impl TempUpload {
    async fn begin(data_root: &std::path::Path) -> Result<Self> {
        let dir = data_root.join("tmp");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| AppError::internal(format!("临时目录不可用：{err}")))?;
        let name = format!(
            "upload-{}-{}",
            Utc::now().timestamp_micros(),
            std::process::id()
        );
        Ok(Self {
            path: dir.join(name),
            dir,
        })
    }

    /// 流式写入并同时算哈希。超过上限立刻停下——不等把整份收完。
    async fn write_all(
        &self,
        mut reader: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        max_bytes: i64,
    ) -> Result<Collected> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut file = tokio::fs::File::create(&self.path)
            .await
            .map_err(|err| AppError::internal(format!("创建临时文件失败：{err}")))?;

        let mut hasher = pixora_crypto::hash::Sha256Stream::new();
        let mut head = Vec::with_capacity(64);
        let mut metadata = Vec::with_capacity(1024 * 1024);
        let mut total: u64 = 0;
        let mut chunk = vec![0u8; 64 * 1024];

        loop {
            let read = reader
                .read(&mut chunk)
                .await
                .map_err(|err| AppError::bad_request(format!("读取上传内容失败：{err}")))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if max_bytes > 0 && total > max_bytes as u64 {
                return Err(AppError::FileTooLarge {
                    size: total,
                    limit: max_bytes as u64,
                });
            }
            let slice = &chunk[..read];
            hasher.update(slice);
            file.write_all(slice)
                .await
                .map_err(|err| AppError::internal(format!("写入临时文件失败：{err}")))?;

            if head.len() < 64 {
                head.extend_from_slice(&slice[..slice.len().min(64 - head.len())]);
            }
            // 头部 1MiB 足够解析常见格式的尺寸：PNG 的 IHDR 在 24 字节，
            // JPEG 的 SOF 段藏在 EXIF 之后也不过几十 KB，
            // ISOBMFF（avif/heic）的 ispe 同样在这个量级
            if metadata.len() < 1024 * 1024 {
                metadata
                    .extend_from_slice(&slice[..slice.len().min(1024 * 1024 - metadata.len())]);
            }
        }

        file.flush()
            .await
            .map_err(|err| AppError::internal(format!("刷新临时文件失败：{err}")))?;
        file.sync_all()
            .await
            .map_err(|err| AppError::internal(format!("落盘临时文件失败：{err}")))?;
        drop(file);

        Ok(Collected {
            path: self.path.clone(),
            size: total,
            sha256: hasher.finalize_hex(),
            head,
            metadata,
        })
    }
}

impl Drop for TempUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // 目录空了顺手删掉，避免 data/tmp 里留下空目录
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// 收完的上传内容。
pub struct Collected {
    path: PathBuf,
    pub size: u64,
    pub sha256: String,
    head: Vec<u8>,
    metadata: Vec<u8>,
}

impl Collected {
    /// 头 64 字节，给魔数嗅探用。
    pub fn head_bytes(&self) -> &[u8] {
        &self.head
    }

    /// 头 1MiB，给尺寸解析用。
    pub fn metadata_bytes(&self) -> Vec<u8> {
        self.metadata.clone()
    }

    /// 打开临时文件作为存储驱动的输入流。
    pub async fn open(&self) -> Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|err| AppError::internal(format!("重新打开临时文件失败：{err}")))?;
        Ok(Box::new(file))
    }
}

/// 清理遗留的临时文件。进程启动时调一次，避免上次崩溃留下的残渣堆积。
pub async fn purge_stale_temp_files(data_root: &std::path::Path, older_than_seconds: i64) -> u64 {
    let dir = data_root.join("tmp");
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(older_than_seconds.max(60) as u64);
    let mut removed = 0u64;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.modified().map(|at| at < cutoff).unwrap_or(false)
            && tokio::fs::remove_file(entry.path()).await.is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// 兼容旧图床响应体的自由函数入口。
///
/// 路由层按 `legacy_response(&outcome)` 引入，实际实现挂在 `AppState` 上
/// （它要读站点配置拼直链），这里只做转发，避免两套口径。
pub fn legacy_response(outcome: &UploadOutcome) -> serde_json::Value {
    AppState::legacy_response(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_visibility_normalizes() {
        let mut input = UploadInput::new("a.jpg");
        assert_eq!(input.normalized_visibility(), "private");
        input.visibility = "PUBLIC".into();
        assert_eq!(input.normalized_visibility(), "public");
        input.visibility = "whatever".into();
        assert_eq!(input.normalized_visibility(), "private", "非法值按私有处理");
    }
}
