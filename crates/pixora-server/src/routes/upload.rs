//! 上传：批量预检、multipart 收件、任务查询。
//!
//! ## 字段顺序：不假设，也不依赖
//!
//! `axum::extract::Multipart` 是**流式**的：`next_field()` 过了这个字段就回不去。
//! 而表单里 `file` 与 `tags` / `album_id` 的先后由客户端决定——
//! `curl -F file=… -F tags=…`（本仓库的 smoke 脚本就是这么发的）与
//! 浏览器里把文件放在 FormData 中间的写法都很常见。
//! 所以这里**先把文件落到临时文件、读完整张表单，再统一提交**：
//! 「要求客户端先发元数据」这种约定一旦被违反，表现不是报错而是**静默丢数据**
//! （标签空了、相册没进去，请求本身还是 200），不值得赌。
//!
//! ## 文件字段怎么接给领域层
//!
//! 理想写法是把 `Field` 直接包成 `Box<dyn AsyncRead + Send + Unpin>` 交给
//! `AppState::upload_file`（它自己流式落盘，不落内存）。做不到的原因是生命周期：
//! `Field<'_>` 借用着 `Multipart`，而 `Box<dyn Trait>` 的默认生命周期是 `'static`。
//! 于是采用「收件 → 临时文件 → 交给领域层」：
//!
//! ```text
//! Field ──chunk()──> {data_dir}/tmp/recv-* ──File──> upload_file
//! ```
//!
//! 收件过程中内存占用只有一个 64KiB 的块，大小上限在收的时候就开始判
//! （超了就删掉半成品并回 `FILE_TOO_LARGE`），不会出现「先把超大请求收完再拒绝」。
//! 多出来的一次同盘顺序写，是换取「任何字段顺序都对」的代价。

use std::sync::Arc;

use axum::extract::multipart::Field;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use pixora_core::uploads::{UploadInput, UploadOutcome};
use pixora_core::{AppError, AppState};
use pixora_repo::model::visibility;

use crate::http::{cookie_value, error_response, finish, request_context, CSRF_COOKIE};

use super::{require_user, PeerAddr};

/// 缩略图的内存上限。客户端生成的缩略图最长边 400px，正常在几十 KB；
/// 给到 4 MiB 是为了容忍 PNG 兜底，比这更大的说明客户端在做别的事。
const MAX_THUMB_BYTES: usize = 4 * 1024 * 1024;

/// 接收文件的目录名。与领域层自己的临时文件放在同一处
/// （`{data_dir}/tmp`），启动时的清理任务会把遗留文件一起扫掉。
const RECV_DIR: &str = "tmp";

/// 接收中的文件在磁盘上的中间态：`Drop` 时删除。
///
/// ## 为什么先把文件字段落到临时文件，而不是把它直接当读流交给领域层
///
///   1. **字段顺序不可控**。multipart 是单向流，`next_field()` 过了这个字段
///      就再也回不去了，而「文件在前、元数据在后」是常见写法
///      （`curl -F file=… -F tags=…`、浏览器里把文件加在 FormData 末尾之前）。
///      只有先把文件收下来，才能读完后面对 `tags` / `album_id` / `visibility`
///      再一起提交。反过来写（元数据必须先到）的表现是**静默丢数据**：
///      标签空了、相册没进去，而请求本身是 200。
///   2. **生命周期对不上**。`axum::extract::multipart::Field<'_>` 借用着
///      `Multipart`，而 `AppState::upload_file` 的入参是
///      `Box<dyn AsyncRead + Send + Unpin>`（trait object 默认 `'static`）。
///
/// 代价是一次额外的顺序写（同盘、默认不超过 20MiB），换来的是「任何字段顺序都对」；
/// 收件过程中内存占用只有一个 64KiB 的块。
struct ReceiveFile {
    path: std::path::PathBuf,
}

impl ReceiveFile {
    /// 把文件字段流式写进临时文件。超过上限立刻停手并删掉半成品——
    /// 不能先把整份收完再判，否则一个超大请求就能把磁盘写满。
    async fn write(
        state: &AppState,
        mut field: Field<'_>,
        limit: i64,
        label: &str,
    ) -> Result<Self, AppError> {
        use tokio::io::AsyncWriteExt;

        let dir = state.storage.local_root().join(RECV_DIR);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| AppError::internal(format!("临时目录不可用：{err}")))?;
        let path = dir.join(format!(
            "recv-{}-{}",
            std::process::id(),
            pixora_repo::model::now().timestamp_micros()
        ));

        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|err| AppError::internal(format!("创建临时文件失败：{err}")))?;
        let mut total: u64 = 0;

        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|err| AppError::bad_request(format!("读取{label}失败：{err}")))?
        {
            total += chunk.len() as u64;
            if limit > 0 && total > limit as u64 {
                // 半成品立刻删掉；错误码与领域层的同一条（FILE_TOO_LARGE → 413）
                let _ = tokio::fs::remove_file(&path).await;
                return Err(AppError::FileTooLarge {
                    size: total,
                    limit: limit as u64,
                });
            }
            file.write_all(&chunk)
                .await
                .map_err(|err| AppError::internal(format!("写入临时文件失败：{err}")))?;
        }

        file.flush()
            .await
            .map_err(|err| AppError::internal(format!("刷新临时文件失败：{err}")))?;
        file.sync_all()
            .await
            .map_err(|err| AppError::internal(format!("落盘临时文件失败：{err}")))?;
        drop(file);
        Ok(Self { path })
    }

    /// 以只读方式打开，交给领域层读。返回的句柄是 `'static` 的
    /// （`tokio::fs::File` 自己持有文件描述符，不借用请求）。
    async fn reader(&self) -> Result<tokio::fs::File, AppError> {
        tokio::fs::File::open(&self.path)
            .await
            .map_err(|err| AppError::internal(format!("打开临时文件失败：{err}")))
    }
}

impl Drop for ReceiveFile {
    fn drop(&mut self) {
        // 领域层已经读完并关掉了句柄；这里删掉的是我们自己的那份中间文件
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 把一个小字段读进内存（缩略图这种几十 KB 的东西不值得为它落盘）。
async fn read_small_field(
    mut field: Field<'_>,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>, AppError> {
    let mut buffer = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|err| AppError::bad_request(format!("读取{label}失败：{err}")))?
    {
        if buffer.len() + chunk.len() > limit {
            return Err(AppError::bad_request(format!("{label}过大")));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

// ============================================================
// 端点
// ============================================================

/// `POST /api/v1/blobs/exists` — 上传前预检。
///
/// 条数上限由领域层拦（`check_hashes`）：1000 是协议硬顶，再按这个账号生效的
/// 单批次最大张数与队列深度阈值各卡一道。路由只负责把 JSON 数组翻译成
/// `Vec<String>`，非字符串元素直接丢掉而不是报错——
/// 前端可能把 `null` 混进来，为这个让整批预检失败不划算。
pub async fn blobs_exists(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "upload") {
        return resp;
    }

    let hashes: Vec<String> = body
        .get("hashes")
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    match state.check_hashes(&auth.user, &hashes).await {
        Ok(outcome) => finish::<serde_json::Value>(Ok(serde_json::json!({
            "exist": outcome.exist,
            "missing": outcome.missing,
        }))),
        Err(err) => error_response(err),
    }
}

/// `POST /api/v1/upload` — 标准上传端点（multipart/form-data）。
pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    mut multipart: Multipart,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "upload") {
        return resp;
    }

    match collect_upload(&state, &auth.user, &mut multipart, rectx.ip.as_deref()).await {
        Ok(Some(outcome)) => {
            finish::<serde_json::Value>(Ok(upload_payload(&outcome, csrf_of(&headers))))
        }
        Ok(None) => error_response(AppError::bad_request("请求里没有图片文件（字段名应为 file）")),
        Err(err) => error_response(err),
    }
}

/// 上传请求体的字段状态机。`collect_upload` 边读边填，
/// 返回后由路由判断「文件字段在不在」。
struct UploadParts {
    album_id: Option<i64>,
    tags: Vec<String>,
    visibility: String,
    sha256: Option<String>,
    original_name: Option<String>,
    thumb: Option<Vec<u8>>,
}

impl UploadParts {
    fn new() -> Self {
        Self {
            album_id: None,
            tags: Vec::new(),
            visibility: visibility::PRIVATE.to_string(),
            sha256: None,
            original_name: None,
            thumb: None,
        }
    }
}

/// 逐字段读 multipart：普通字段进 `UploadParts`，文件字段先落到临时文件，
/// 等整个表单读完、元数据齐了再交给领域层。
///
/// 返回 `Ok(None)` 表示「整个表单里没有文件字段」，
/// 由调用方决定报错文案。
async fn collect_upload(
    state: &Arc<AppState>,
    user: &pixora_repo::model::User,
    multipart: &mut Multipart,
    ip: Option<&str>,
) -> Result<Option<UploadOutcome>, AppError> {
    let mut parts = UploadParts::new();
    // 文件字段的附加信息只能在它自己身上读到，先存下来
    let mut file_name = String::new();
    let mut content_type: Option<String> = None;
    let mut received: Option<ReceiveFile> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::bad_request(format!("表单解析失败：{err}")))?
    {
        // 字段名统一小写：老客户端里出现 `File` / `Image` 的大写写法并不罕见
        let name = field
            .name()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        match name.as_str() {
            // ---------- 普通字段 ----------
            "tags" => {
                let text = field
                    .text()
                    .await
                    .map_err(|err| AppError::bad_request(format!("读取 tags 失败：{err}")))?;
                parts.tags = super::split_csv(&text);
            }
            "visibility" => {
                let text = field
                    .text()
                    .await
                    .map_err(|err| AppError::bad_request(format!("读取 visibility 失败：{err}")))?;
                parts.visibility = text.trim().to_string();
            }
            "album_id" => {
                let text = field
                    .text()
                    .await
                    .map_err(|err| AppError::bad_request(format!("读取 album_id 失败：{err}")))?;
                let text = text.trim();
                // 空串是「不指定相册」，不是「相册 0」
                if !text.is_empty() {
                    parts.album_id = Some(
                        text.parse::<i64>()
                            .map_err(|_| AppError::bad_request("album_id 必须是数字"))?,
                    );
                }
            }
            "sha256" => {
                let text = field
                    .text()
                    .await
                    .map_err(|err| AppError::bad_request(format!("读取 sha256 失败：{err}")))?;
                let text = text.trim().to_ascii_lowercase();
                if !text.is_empty() {
                    parts.sha256 = Some(text);
                }
            }
            "original_name" => {
                let text = field
                    .text()
                    .await
                    .map_err(|err| AppError::bad_request(format!("读取 original_name 失败：{err}")))?;
                let text = text.trim().to_string();
                if !text.is_empty() {
                    parts.original_name = Some(text);
                }
            }
            "thumb" => {
                // 缩略图很小（客户端生成，最长边 400px），进内存比落盘划算
                let buffer = read_small_field(field, MAX_THUMB_BYTES, "缩略图").await?;
                if !buffer.is_empty() {
                    parts.thumb = Some(buffer);
                }
            }
            // ---------- 文件字段：先收下来，读完后面的元数据再提交 ----------
            "file" | "image" => {
                // multipart 的 filename / content-type 只有在这个字段上拿得到，
                // 后面回不来，所以先记下来
                file_name = field.file_name().unwrap_or_default().to_string();
                content_type = field.content_type().map(str::to_string);

                // 单文件上限按「生效的上传策略」取（单用户覆盖 → 全局默认 → 系统设置）：
                // 这是收流时的截断阈值，比全局值更紧的用户覆盖必须在这一步就拦住，
                // 否则超限的字节已经落到临时文件里了
                let limit = state
                    .effective_upload_limits(user)
                    .await
                    .map(|limits| limits.max_file_bytes)
                    .unwrap_or_default();
                received = Some(ReceiveFile::write(state, field, limit, "图片文件").await?);
            }
            // 未知字段忽略：老客户端的表单里可能有 `source`、`key` 之类的附加项
            _ => {}
        }
    }

    let Some(received) = received else {
        return Ok(None);
    };

    let input = UploadInput {
        album_id: parts.album_id,
        tags: parts.tags.clone(),
        visibility: parts.visibility.clone(),
        // 表单没给原始文件名就退回 multipart 的 filename，最后兜底一个占位名：
        // 它只是内部记录，空字符串会让列表页显示成空白行
        original_name: parts
            .original_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(file_name).filter(|value| !value.trim().is_empty()))
            .unwrap_or_else(|| "upload".to_string()),
        client_sha256: parts.sha256.clone(),
        content_type,
    };
    let thumb = parts.thumb.take().map(|bytes| {
        let len = bytes.len() as i64;
        (
            Box::new(std::io::Cursor::new(bytes)) as Box<dyn tokio::io::AsyncRead + Send + Unpin>,
            len,
        )
    });

    // 到这里元数据齐了：把临时文件当读流交给领域层（它自己再流式落到内容寻址目录）
    let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = Box::new(received.reader().await?);
    let outcome = state.upload_file(user, input, reader, thumb, ip).await?;
    Ok(Some(outcome))
}

/// 标准上传响应体。
pub fn upload_payload(outcome: &UploadOutcome, csrf_token: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "id": outcome.image_id,
        "job_id": outcome.job_id,
        "url": outcome.url,
        "thumb_url": outcome.thumb_url,
        "sha256": outcome.sha256,
        "size": outcome.size,
        "dedup": outcome.dedup,
        "storage_type": outcome.storage_type,
        "fallback_message": outcome.fallback_message,
        "tags": outcome.tags,
        "csrf_token": csrf_token,
    })
}

fn csrf_of(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, CSRF_COOKIE)
}

/// `GET /api/v1/queue/{job_id}`：任务状态。
///
/// 不是本人的任务按 `NOT_FOUND` 处理，而不是 `FORBIDDEN`——
/// 否则这个端点会变成「某个 job id 存不存在」的探针。
pub async fn queue_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    method: Method,
    peer: PeerAddr,
    Path(job_id): Path<i64>,
) -> Response {
    let rectx = request_context(&state, &headers, peer.addr()).await;
    let auth = match require_user(&state, &headers, rectx.ip.as_deref(), &method).await {
        Ok(auth) => auth,
        Err(resp) => return resp,
    };
    if let Err(resp) = super::ensure_scope(&auth, "read") {
        return resp;
    }

    let job = match state.repo.find_job(job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => return error_response(AppError::not_found("上传任务")),
        Err(err) => return error_response(err.into()),
    };
    if job.owner_id != auth.user.id && !auth.is_admin() {
        return error_response(AppError::not_found("上传任务"));
    }

    finish::<serde_json::Value>(Ok(serde_json::json!({
        "id": job.id,
        "status": job.status,
        "dedup": job.dedup,
        "image_id": job.image_id,
        "error": job.error,
    })))
}
