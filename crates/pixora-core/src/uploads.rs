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
    /// 上传来源：IP 与终端。访客上传靠它留痕，登录用户顺手也记
    pub upload_ip: Option<String>,
    pub upload_agent: Option<String>,
    /// 「同一份内容已经在我的图库里就复用它」，不新增一条引用。
    ///
    /// 头像这类「一张图反复设」的场景才用它：去重只让物理文件不翻倍，图片记录
    /// 照样每传一次多一条，相册里就会并排站着两张一模一样的图。复用只补标签，
    /// 不动那张图原有的相册、可见性与标题——它本来就是用户自己放进图库的，
    /// 不该因为被设成头像就换位置
    pub reuse_existing: bool,
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
            upload_ip: None,
            upload_agent: None,
            reuse_existing: false,
        }
    }

    /// 记下这次上传的来源（路由从请求上下文里取）。
    pub fn with_origin(mut self, ip: Option<&str>, agent: Option<&str>) -> Self {
        self.upload_ip = ip.map(str::to_string);
        // UA 可能很长，留 255 就够认出「哪个浏览器 / 客户端」
        self.upload_agent = agent.map(|value| value.chars().take(255).collect::<String>());
        self
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
    /// 这次没有新建图片，而是复用了图库里已有的那张（见 `UploadInput::reuse_existing`）
    pub reused: bool,
}

/// 批量预检的结果。
#[derive(Debug, Clone)]
pub struct HashCheckOutcome {
    pub exist: Vec<String>,
    pub missing: Vec<String>,
}

/// 生效的上传策略（某个用户的）。
///
/// 解析顺序：**单用户覆盖 → 全局策略行 → 系统设置里的站点默认**。
/// 三项数值里 `0`、格式里的空串都表示「跟随上一层」——用户卡不填就跟随
/// 全局默认，全局默认不填就跟随系统设置，所以管理员改系统设置时，标了
/// 「跟随」的人会立刻跟着变，标了自定义的不受影响。界面展示的是解析后的
/// 生效值，不会让人看到 0。
#[derive(Debug, Clone)]
pub struct EffectiveUploadLimits {
    /// 单文件大小上限：同时是收 multipart 流的截断阈值
    pub max_file_bytes: i64,
    /// 单批最大张数：客户端队列据此限流
    pub max_batch: i64,
    /// 像素上限：解压炸弹防线
    pub max_pixels: i64,
    /// 长宽上限（像素），0 = 不限
    pub max_width: i64,
    pub max_height: i64,
    /// 允许的图片格式（扩展名，已归一化）
    pub formats: Vec<String>,
    /// 单用户并发上传数：客户端队列的上限
    pub max_concurrent_uploads: i64,
}

impl EffectiveUploadLimits {
    /// 三层解析的落点。管理页要一次算一页用户的生效值，逐行回查数据库
    /// 会把三层解析拆成 N 次查询，所以解析逻辑收在这里，谁都能直接用。
    pub fn resolve(
        site: &crate::settings::UploadSettings,
        global: &pixora_repo::model::RateLimitPolicy,
        own: Option<&pixora_repo::model::RateLimitPolicy>,
    ) -> Self {
        // 上层优先：用户覆盖在前，全局默认殿后
        let layers = [own, Some(global)];

        // 第一个非 0 的值生效；都没有就用站点默认
        let pick = |get: fn(&pixora_repo::model::RateLimitPolicy) -> i64, fallback: i64| {
            layers
                .iter()
                .flatten()
                .map(|policy| get(policy))
                .find(|value| *value > 0)
                .unwrap_or(fallback)
        };

        let formats = layers
            .iter()
            .flatten()
            .filter_map(|policy| policy.formats.as_deref())
            .map(crate::settings::parse_format_list)
            .find(|list| !list.is_empty())
            .unwrap_or_else(|| site.formats.clone());

        Self {
            max_file_bytes: pick(|policy| policy.max_file_size, site.max_file_bytes),
            max_batch: pick(|policy| policy.max_batch_size, site.max_batch),
            max_pixels: pick(|policy| policy.max_pixels, site.max_pixels),
            max_width: pick(|policy| policy.max_width, site.max_width),
            max_height: pick(|policy| policy.max_height, site.max_height),
            formats,
            max_concurrent_uploads: pick(
                |policy| policy.max_concurrent_uploads,
                pixora_repo::model::RateLimitPolicy::defaults().max_concurrent_uploads,
            ),
        }
    }
}

impl AppState {
    /// 解析某个用户生效的上传策略。
    pub async fn effective_upload_limits(&self, user: &User) -> Result<EffectiveUploadLimits> {
        let site = self.settings.upload().await?;
        let global = self.repo.global_policy().await?;
        let own = self.repo.user_policy(user.id).await?;
        Ok(EffectiveUploadLimits::resolve(&site, &global, own.as_ref()))
    }

    /// 内容审核这一步。没开审核、或这个角色不过审时立刻返回，不读盘不发请求。
    async fn screen_before_store(
        &self,
        user: &User,
        collected: &Collected,
        input: &UploadInput,
        mime: &str,
        thumb: Option<&BufferedThumb>,
        ip: Option<&str>,
    ) -> Result<()> {
        let settings = self.settings.moderation().await?;
        if !settings.enabled || !settings.applies_to(user.is_admin()) {
            return Ok(());
        }

        // 送原作还是送缩略图：原图在体积上限内就送原图，
        // 超了就退回缩略图（长边 1024px，判内容足够，四家都收得下）
        if collected.size as usize <= crate::moderation::MAX_SUBMIT_BYTES {
            let bytes = collected.read_all().await?;
            return self
                .review_upload(
                    user,
                    crate::moderation::ScreenInput::Image {
                        bytes: &bytes,
                        mime,
                    },
                    &input.original_name,
                    ip,
                )
                .await;
        }
        if let Some(thumb) = thumb {
            return self
                .review_upload(
                    user,
                    crate::moderation::ScreenInput::Image {
                        bytes: &thumb.bytes,
                        mime: thumb.mime,
                    },
                    &input.original_name,
                    ip,
                )
                .await;
        }

        // 又大又没有缩略图：没有可送的内容。按 fail_open 的口径处理
        if settings.fail_open {
            tracing::warn!(
                size = collected.size,
                "图片超过送审体积上限且没有缩略图，本次跳过内容审核"
            );
            return Ok(());
        }
        Err(AppError::ContentRejected {
            label: "无法送审".into(),
            score: 0.0,
            provider: settings.provider,
        })
    }
}

/// 已经在内存里的缩略图。审核与存储都要用，所以读一次留着。
pub struct BufferedThumb {
    bytes: Vec<u8>,
    mime: &'static str,
}

/// 把缩略图读进内存。超过 2MiB 直接拒绝——客户端生成的缩略图只有几十 KB，
/// 这个上限是防止有人往这个字段里塞别的东西。
async fn read_thumb(
    thumb: Option<(Box<dyn tokio::io::AsyncRead + Send + Unpin>, i64)>,
) -> Result<Option<BufferedThumb>> {
    use tokio::io::AsyncReadExt;

    let Some((mut reader, size_hint)) = thumb else {
        return Ok(None);
    };
    let mut bytes = Vec::with_capacity(size_hint.clamp(0, 2 * 1024 * 1024) as usize);
    reader.read_to_end(&mut bytes).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(AppError::bad_request("缩略图过大"));
    }
    // MIME 在这里嗅一次：审核要带上它，存储那边也用同一份结论
    let mime = media::sniff(&bytes)
        .ok_or_else(|| AppError::InvalidImage("缩略图格式无法识别".into()))?
        .mime;
    Ok(Some(BufferedThumb { bytes, mime }))
}

impl AppState {
    /// 批量预检：客户端把一批哈希一次发过来，服务端只回哪些已存在。
    ///
    /// 这是省带宽的关键一步——重复文件连上传请求都不发。判定口径要跟着
    /// **这个账号图库里活着的引用**走：只看 blobs 表的话，用户删光图片后
    /// 重传同一批会被整批跳过。
    ///
    /// 条数上限也在这里：一次预检请求就是客户端的一批，超过生效的单批次
    /// 最大张数直接退回，客户端据此把批次切小。
    pub async fn check_hashes(&self, user: &User, hashes: &[String]) -> Result<HashCheckOutcome> {
        if hashes.is_empty() {
            return Ok(HashCheckOutcome {
                exist: Vec::new(),
                missing: Vec::new(),
            });
        }
        // 协议层的硬顶：比这更大的一次性预检没有实际意义，客户端本来就按批发
        const CEILING: usize = 1000;
        if hashes.len() > CEILING {
            return Err(AppError::bad_request(format!(
                "单次最多预检 {CEILING} 个哈希"
            )));
        }
        // 预检只是「这些内容库里有没有」的查询，不承担入队准入：用户选多少
        // 都不能在这里被截断或拒绝，真正的队列由上传接口按条处理。
        for hash in hashes {
            if hash.len() != 64 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(AppError::bad_request(format!("哈希格式不正确：{hash}")));
            }
        }
        let normalized: Vec<String> = hashes.iter().map(|hash| hash.to_lowercase()).collect();
        // 访客上传共用同一个访客账号。若把访客库的哈希命中情况回给浏览器，
        // 知道图片内容的人就能靠「开始上传」确认访客库里有没有这张图。
        // 访客一律按「都缺」处理，真正的去重交给上传接口在收到文件后做。
        if crate::guest::is_guest(user) {
            return Ok(HashCheckOutcome {
                exist: Vec::new(),
                missing: normalized,
            });
        }
        let check = self.repo.existing_hashes(user.id, &normalized).await?;
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
        self.ensure_ip_allowed(ip, crate::ipguard::scope::UPLOAD)
            .await?;

        // 访客上传只有公开：那个账号登不进来，私有图片传完谁也看不见——
        // 连它自己都进不去，等于白传。所以这里按账号定，不听客户端传了什么
        let mut input = input;
        if crate::guest::is_guest(user) {
            input.visibility = visibility::PUBLIC.to_string();
        }
        // 原始文件名只用于展示与下载：去掉路径、控制字符，并压到库列宽以内。
        // 不处理时，PG/MySQL 会在插入阶段报「值太长」，SQLite 则会把整段塞进去。
        input.original_name = clean_original_name(&input.original_name);

        let upload_settings = self.settings.upload().await?;
        let policy = self.repo.effective_policy(user).await?;
        // 单文件上限、像素上限、格式白名单按「生效的上传策略」判：
        // 单用户覆盖优先，没覆盖才落到全局默认与系统设置
        let limits = self.effective_upload_limits(user).await?;

        // 背压：服务端队列深度超阈值就拒，客户端按 Retry-After 退避
        let depth = self.repo.queue_depth().await?;
        if upload_settings.queue_depth > 0 && depth.total() >= upload_settings.queue_depth {
            return Err(AppError::QueueFull { retry_after: 5 });
        }

        // 先把字节落到临时文件：大小上限在流的过程中就能判，不必等收完
        let temp = TempUpload::begin(&self.storage.local_root()).await?;
        let mut collected = temp.write_all(reader, limits.max_file_bytes).await?;

        if collected.size == 0 {
            return Err(AppError::InvalidImage("文件为空".into()));
        }

        // 格式校验：白名单只是第一道，真实格式看魔数
        let mut sniffed = media::sniff(&collected.head)
            .ok_or_else(|| AppError::InvalidImage("无法识别的图片格式".into()))?;
        // SVG 是唯一能携带脚本的“图片”，必须在入库前净化；直链上的 CSP
        // 只是第二道防线，不能拿它替代净化（下载后换宿主、换反代就没了）。
        if sniffed.mime == "image/svg+xml" {
            collected.sanitize_svg_in_place().await?;
            sniffed = media::sniff(&collected.head)
                .ok_or_else(|| AppError::InvalidImage("SVG 净化后不再是有效图片".into()))?;
            if sniffed.mime != "image/svg+xml" {
                return Err(AppError::InvalidImage("SVG 净化后格式异常".into()));
            }
        }
        let ext = sniffed.ext.to_string();
        let normalized_ext = media::normalize_ext(&ext);
        let declared_ext = media::normalize_ext(&media::ext_from_name(&input.original_name));
        if !limits.formats.contains(&normalized_ext) {
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
        let (width, height) =
            media::dimensions(&collected.metadata_bytes(), sniffed.mime).unwrap_or((0, 0));
        if width > 0 && height > 0 {
            media::check_pixels(width, height, limits.max_pixels as u64)?;
            media::check_dimensions(width, height, limits.max_width, limits.max_height)?;
        }

        // 缩略图先读进内存：内容审核要用它，存储也要用它。
        // 原图超过各家接口的体积上限时，送审的就是这张长边 1024px 的缩略图。
        let thumb_bytes = read_thumb(thumb).await?;

        // 内容审核：在写任何一行数据库之前拦下违规内容
        self.screen_before_store(
            user,
            &collected,
            &input,
            sniffed.mime,
            thumb_bytes.as_ref(),
            ip,
        )
        .await?;

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
                thumb_bytes,
            )
            .await;

        match result {
            Ok(outcome) => {
                self.repo
                    .mark_job_done(job.id, outcome.image_id, &outcome.sha256, outcome.dedup)
                    .await?;
                // 只有真正落盘的新文件才消耗上传额度（去重跳过的不算）。
                // 管理员豁免配额，也不把他们的上传算进全站总量。
                if !outcome.dedup && !user.is_admin() {
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
                        // 访客上传落到访客账号上：审计里标出来，
                        // 翻记录时才分得清哪些是游客传的
                        "guest": user.uid == crate::guest::GUEST_UID,
                        "agent": input.upload_agent,
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
                    let _ = self.ip_guard.record_failure(ip, actions::UPLOAD_FAIL).await;
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
        thumb_bytes: Option<BufferedThumb>,
    ) -> Result<UploadOutcome> {
        let size = collected.size as i64;
        // 与 GC 共用一把按内容哈希分片的锁：从查重到建图片引用期间，
        // 同一实体的「引用归零回收」必须等我们走完。
        let _blob_guard = self.blob_lock(&collected.sha256).lock().await;

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

        // 「已有就复用」：同一份内容已经在这个账号的图库里，就引用那一张。
        // 头像反复设同一张图时，相册里不该并排站着两张一模一样的。
        // 放在建缩略图、建图片行之前——复用本来就不需要这两步
        if input.reuse_existing {
            if let Some(existing) = self.repo.find_live_image_by_blob(user.id, blob.id).await? {
                let tags = self
                    .merge_image_tags(existing.id, user.id, &input.tags)
                    .await?;
                let url = self.direct_url(&collected.sha256, ext, false).await;
                let thumb_url = if existing.thumb_blob_id.is_some() {
                    Some(self.direct_url(&collected.sha256, ext, true).await)
                } else {
                    None
                };
                return Ok(UploadOutcome {
                    image_id: existing.id,
                    job_id: 0,
                    sha256: collected.sha256.clone(),
                    ext: ext.to_string(),
                    size,
                    // 内容本来就在站里，对限流与审计来说和命中去重是一回事
                    dedup: true,
                    reused: true,
                    storage_type: blob.storage_driver.clone(),
                    fallback_message,
                    tags,
                    url,
                    thumb_url,
                });
            }
        }

        // 缩略图也是内容寻址的一份实体，同样去重
        let thumb_blob_id = match thumb_bytes {
            Some(thumb) => self.store_thumb(thumb).await.unwrap_or(None),
            None => None,
        };

        // 没指定相册就落进「默认相册」：界面上那句「不指定相册就等于进默认相册」
        // 得在服务端也成立，否则这类图不挂在任何相册下，用户在相册页里找不到，
        // 会以为上传丢了。真找不到默认相册（被删了之类）才留空。
        let album = match input.album_id {
            Some(album_id) => {
                let album = self
                    .repo
                    .find_album_by_id(album_id)
                    .await?
                    .ok_or_else(|| AppError::not_found("相册"))?;
                if album.owner_id != user.id {
                    return Err(AppError::forbidden("相册不属于当前账号"));
                }
                Some(album)
            }
            None => {
                self.repo
                    .find_album_by_slug(user.id, pixora_repo::repo::albums::DEFAULT_ALBUM_SLUG)
                    .await?
            }
        };
        let album_id = album.as_ref().map(|item| item.id);
        let album_slug = album.as_ref().map(|item| item.slug.clone());

        let image = self
            .repo
            .create_image_reference(
                &pixora_repo::repo::images::NewImage {
                    owner_id: user.id,
                    album_id,
                    blob_id: blob.id,
                    thumb_blob_id,
                    original_name: input.original_name.clone(),
                    title: None,
                    visibility: input.normalized_visibility(),
                    upload_ip: input.upload_ip.clone(),
                    upload_agent: input.upload_agent.clone(),
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
            reused: false,
            storage_type: blob.storage_driver.clone(),
            fallback_message,
            tags: input.tags.clone(),
            url,
            thumb_url,
        })
    }

    /// 把这次带来的标签并到已有图片上，返回合并后的标签表。
    ///
    /// 只加不减：那张图原有的标签是用户自己打上去的，复用（比如设成头像）
    /// 不该顺手抹掉它们。
    async fn merge_image_tags(
        &self,
        image_id: i64,
        owner_id: i64,
        extra: &[String],
    ) -> Result<Vec<String>> {
        let mut current = self
            .repo
            .tags_for_images(&[image_id])
            .await?
            .remove(&image_id)
            .unwrap_or_default();
        let missing: Vec<String> = extra
            .iter()
            .filter(|name| !name.trim().is_empty() && !current.contains(name))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(current);
        }
        current.extend(missing);
        self.repo
            .set_image_tags(image_id, owner_id, &current)
            .await?;
        Ok(current)
    }

    /// 存缩略图。同样内容寻址 + 去重，失败只返回 None（不毁掉整次上传）。
    async fn store_thumb(&self, thumb: BufferedThumb) -> Result<Option<i64>> {
        let buffer = thumb.bytes;
        if buffer.is_empty() {
            return Ok(None);
        }

        let sniffed = media::sniff(&buffer)
            .ok_or_else(|| AppError::InvalidImage("缩略图格式无法识别".into()))?;
        if !matches!(sniffed.mime, "image/webp" | "image/png" | "image/jpeg") {
            return Err(AppError::UnsupportedFormat(sniffed.ext.to_string()));
        }
        // 缩略图也要过像素上限：4 MiB 的 PNG 可以解出几万像素见方，
        // 列表页一口气渲染几十张时，浏览器和服务端都会为它买单。
        let (thumb_w, thumb_h) = media::dimensions(&buffer, sniffed.mime)
            .ok_or_else(|| AppError::InvalidImage("缩略图尺寸无法识别".into()))?;
        media::check_pixels(thumb_w, thumb_h, 16_000_000)?;
        media::check_dimensions(thumb_w, thumb_h, 8192, 8192)?;

        let sha256 = pixora_crypto::hash::sha256_hex(&buffer);
        let ext = sniffed.ext;
        if let Some(existing) = self
            .repo
            .find_blob_by_sha(&sha256, storage_kind::THUMB)
            .await?
        {
            return Ok(Some(existing.id));
        }
        // 记下这张缩略图自己的像素尺寸：判断「是不是该按新尺寸重算」全靠它
        let (thumb_width, thumb_height) = (
            Some(thumb_w as i32).filter(|v| *v > 0),
            Some(thumb_h as i32).filter(|v| *v > 0),
        );

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
                width: thumb_width,
                height: thumb_height,
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
        // 最后一张删掉之后目录会空着：顺手收掉，别让磁盘上留一串空壳目录
        self.prune_view_dir(driver, backend_id, uid, album_slug)
            .await;
    }

    /// 清掉已经空了的镜像目录（只删空目录，绝不递归）。
    pub async fn prune_view_dir(
        &self,
        driver: &str,
        backend_id: Option<i64>,
        uid: &str,
        album_slug: &str,
    ) {
        let view = format!("users/{uid}/{album_slug}");
        if let Err(err) = self
            .storage
            .remove_view_dir(driver, backend_id, &view)
            .await
        {
            tracing::debug!(error = %err, "清理镜像空目录失败");
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
}

/// 临时文件的生命周期封装：构造时创建，Drop 时删除。
struct TempUpload {
    path: PathBuf,
    dir: PathBuf,
}

/// 原始文件名的展示形态：只留文件名本身，去掉控制字符，截到 255 字符。
fn clean_original_name(raw: &str) -> String {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(255)
        .collect::<String>();
    if name.is_empty() {
        "upload".to_string()
    } else {
        name
    }
}

impl TempUpload {
    async fn begin(data_root: &std::path::Path) -> Result<Self> {
        let dir = data_root.join("tmp");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| AppError::internal(format!("临时目录不可用：{err}")))?;
        // 同一个进程里并发上传同一时刻并不罕见；只用时间戳 + pid 会撞名，
        // 两个请求互相覆盖对方写到一半的文件。
        let suffix = pixora_crypto::token::random_hex(8)?;
        let name = format!(
            "upload-{}-{}-{suffix}",
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
                metadata.extend_from_slice(&slice[..slice.len().min(1024 * 1024 - metadata.len())]);
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

    /// 整份读进内存。内容审核要把它 base64 之后送给第三方服务，
    /// 只能整份读；调用方先按体积门槛判过（见 `screen_before_store`）。
    pub async fn read_all(&self) -> Result<Vec<u8>> {
        tokio::fs::read(&self.path)
            .await
            .map_err(|err| AppError::internal(format!("读取临时文件失败：{err}")))
    }

    /// 就地把 SVG 净化后写回临时文件，并重算哈希/大小/头部。
    ///
    /// 净化会改变内容，哈希也必须跟着变：去重、直链、审计都以这个哈希为准。
    /// 只在嗅探到 SVG 时调用，其他格式不读盘。
    async fn sanitize_svg_in_place(&mut self) -> Result<()> {
        let bytes = self.read_all().await?;
        let cleaned = media::sanitize_svg(&bytes);
        let sniffed = media::sniff(&cleaned)
            .ok_or_else(|| AppError::InvalidImage("SVG 净化后无法识别".into()))?;
        if sniffed.mime != "image/svg+xml" {
            return Err(AppError::InvalidImage("SVG 净化后格式异常".into()));
        }
        tokio::fs::write(&self.path, &cleaned)
            .await
            .map_err(|err| AppError::internal(format!("写回净化后的 SVG 失败：{err}")))?;
        self.size = cleaned.len() as u64;
        self.sha256 = pixora_crypto::hash::sha256_hex(&cleaned);
        self.head = cleaned[..cleaned.len().min(64)].to_vec();
        self.metadata = cleaned[..cleaned.len().min(1024 * 1024)].to_vec();
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use pixora_repo::model::RateLimitPolicy;

    /// 站点默认（系统设置那一层）
    fn site_settings() -> crate::settings::UploadSettings {
        crate::settings::UploadSettings {
            max_file_bytes: 100 * 1024 * 1024,
            max_batch: 1000,
            max_pixels: 80_000_000,
            max_width: 0,
            max_height: 0,
            queue_depth: 500,
            formats: vec!["jpg".into(), "png".into()],
            thumb_client: true,
            thumb_server_fallback: false,
        }
    }

    #[test]
    fn input_visibility_normalizes() {
        let mut input = UploadInput::new("a.jpg");
        assert_eq!(input.normalized_visibility(), "private");
        input.visibility = "PUBLIC".into();
        assert_eq!(input.normalized_visibility(), "public");
        input.visibility = "whatever".into();
        assert_eq!(input.normalized_visibility(), "private", "非法值按私有处理");
    }

    /// 三层解析：单用户覆盖 → 全局策略行 → 系统设置；0 与空格式串表示「跟随」。
    #[test]
    fn effective_limits_resolve_user_then_global_then_site() {
        let site = site_settings();
        // 全局策略行什么也没覆盖（0 / 空）：全部落到系统设置
        let global = RateLimitPolicy::defaults();
        let none = EffectiveUploadLimits::resolve(&site, &global, None);
        assert_eq!(none.max_file_bytes, site.max_file_bytes);
        assert_eq!(none.max_batch, site.max_batch);
        assert_eq!(none.max_pixels, site.max_pixels);
        assert_eq!(none.formats, site.formats);

        // 全局策略行设了单文件上限：没被单独覆盖的用户跟着它走
        let global = RateLimitPolicy {
            max_file_size: 30 * 1024 * 1024,
            ..RateLimitPolicy::defaults()
        };
        let inherited = EffectiveUploadLimits::resolve(&site, &global, None);
        assert_eq!(inherited.max_file_bytes, 30 * 1024 * 1024);
        assert_eq!(
            inherited.max_pixels, site.max_pixels,
            "没设的字段继续跟随系统设置"
        );

        // 用户覆盖优先；他只设了格式，其它字段继续按全局与系统设置解析
        let own = RateLimitPolicy {
            formats: Some("webp, jpeg".into()),
            ..RateLimitPolicy::defaults()
        };
        let resolved = EffectiveUploadLimits::resolve(&site, &global, Some(&own));
        assert_eq!(resolved.max_file_bytes, 30 * 1024 * 1024);
        assert_eq!(
            resolved.formats,
            vec!["webp".to_string(), "jpg".to_string()],
            "jpeg 要归一化成 jpg"
        );

        // 像素上限只认第一个非 0 的值：用户设了就轮不到全局
        let own = RateLimitPolicy {
            max_pixels: 4_000_000,
            ..RateLimitPolicy::defaults()
        };
        let resolved = EffectiveUploadLimits::resolve(&site, &global, Some(&own));
        assert_eq!(resolved.max_pixels, 4_000_000);

        // 全是空值时格式回落到系统设置；认不出来的扩展名会被丢掉
        let own = RateLimitPolicy {
            formats: Some("webp, not-a-format".into()),
            ..RateLimitPolicy::defaults()
        };
        let resolved = EffectiveUploadLimits::resolve(&site, &global, Some(&own));
        assert_eq!(resolved.formats, vec!["webp".to_string()]);
    }
}
