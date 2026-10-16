//! 缩略图兜底生成。
//!
//! **默认关闭**，而且这是服务端唯一碰像素的地方——设计稿把「服务端不承担图像
//! 缩放」当成硬约束，因为 CPU 一旦成为瓶颈，整机的内存预算也就守不住了。
//! 所以这条路只在两种情况下打开：
//!
//!   - 站点主要靠 API 上传（PicGo/PicList），客户端不可能带缩略图；
//!   - 旧数据迁移过来时缺缩略图。
//!
//! 生成的产物也是一份内容寻址的实体（`kind = 'thumb'`），与客户端生成的
//! 缩略图共用同一套去重逻辑：同一张图被两个人引用时只会有一份缩略图。
//!
//! 输出统一用 JPEG：`image` crate 能编码 JPEG 而不用拖进 libwebp 这类 C 依赖，
//! 而缩略图这个尺寸（最长边与客户端一致，1024px）上 JPEG 的体积仍然可接受，
//! 没必要为了格式再去引一个 WebP 编码器。

use std::sync::Arc;

use pixora_core::{AppError, AppState};
use pixora_repo::model::storage_kind;
use pixora_repo::repo::uploads::NewBlob;
use pixora_storage::cas::cas_key;

/// 缩略图最长边。
const MAX_EDGE: u32 = 1024;
/// JPEG 质量。80 是体积与观感的常见折中。
const QUALITY: u8 = 80;
/// 单张原图最大解码体积（超过就不兜底了，避免把内存吃满）。
const MAX_SOURCE_BYTES: u64 = 24 * 1024 * 1024;

/// 为缺少缩略图的图片补生成一批。返回生成数量。
pub async fn backfill(state: &Arc<AppState>, limit: i64) -> pixora_core::Result<u64> {
    let upload_settings = state.settings.upload().await?;
    if !upload_settings.thumb_server_fallback {
        // 没开启就什么都不做，也不打日志——这是常态
        return Ok(0);
    }

    let images = state
        .repo
        .list_images_missing_thumb(limit, true)
        .await?;
    if images.is_empty() {
        return Ok(0);
    }

    let mut generated = 0u64;
    for image in images {
        match generate_for(state, image.id, image.blob_id).await {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(err) => tracing::debug!(image_id = image.id, error = %err, "兜底生成缩略图跳过"),
        }
    }
    Ok(generated)
}

// ============================================================
// 缩略图重算（把已经有的换成新尺寸）
// ============================================================

/// 重算的每批张数。太多了会长时间占着 CPU，太小了又要等下一轮轮询。
const REGEN_BATCH: i64 = 20;

/// 推进一批：游标之后的图片按新尺寸重算缩略图。返回这一批处理了几张。
///
/// 与 `backfill` 的分工：那个只补「缺缩略图的」，而这个**替换**已经有的——
/// 客户端早期按 400px 生成，在宽屏上放大两倍多就是糊的，得重算一遍才跟得上。
/// 游标存在设置里，所以跑一半重启也不会从头再来。
pub async fn regenerate_batch(state: &Arc<AppState>, limit: i64) -> pixora_core::Result<u64> {
    let progress = state.thumb_rebuild_progress().await?;
    if !progress.running {
        return Ok(0);
    }
    let cursor = state
        .settings
        .raw(pixora_core::settings::keys::THUMB_REGEN_CURSOR)
        .await?
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(0);

    let images = state
        .repo
        .list_images_after_id(cursor, limit.clamp(1, REGEN_BATCH))
        .await?;
    if images.is_empty() {
        // 没有下一批了：直接把进度补满，别让「进行中」永远挂着
        state
            .settings
            .set(
                pixora_core::settings::keys::THUMB_REGEN_DONE,
                &progress.total.to_string(),
            )
            .await?;
        return Ok(0);
    }

    let mut processed = 0u64;
    let mut last_id = cursor;
    for image in &images {
        last_id = last_id.max(image.id);
        let old_thumb = image.thumb_blob_id;
        match make_thumb_for(state, image.id, image.blob_id, old_thumb).await {
            Ok(_) => processed += 1,
            Err(err) => tracing::debug!(image_id = image.id, error = %err, "重算缩略图跳过"),
        }
    }

    state
        .settings
        .set(
            pixora_core::settings::keys::THUMB_REGEN_CURSOR,
            &last_id.to_string(),
        )
        .await?;
    state
        .settings
        .set(
            pixora_core::settings::keys::THUMB_REGEN_DONE,
            &(progress.done + processed as i64).to_string(),
        )
        .await?;
    Ok(processed)
}

/// 重算单张：读原图 → 缩放编码 → 存实体 → 换掉旧的那条引用。
async fn make_thumb_for(
    state: &Arc<AppState>,
    image_id: i64,
    blob_id: i64,
    old_thumb_id: Option<i64>,
) -> pixora_core::Result<bool> {
    let Some(blob) = state.repo.find_blob_by_id(blob_id).await? else {
        return Ok(false);
    };
    if blob.size > MAX_SOURCE_BYTES as i64 {
        return Ok(false);
    }
    let mut reader = state
        .storage
        .open_object(&blob.storage_driver, blob.backend_id, &blob.storage_key)
        .await?;
    let mut source = Vec::with_capacity(blob.size.clamp(0, 8 * 1024 * 1024) as usize);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut source)
        .await
        .map_err(|err| AppError::internal(format!("读取原图失败：{err}")))?;

    let encoded = tokio::task::spawn_blocking(move || encode_thumb(&source))
        .await
        .map_err(|err| AppError::internal(format!("缩略图任务失败：{err}")))?
        .map_err(AppError::InvalidImage)?;

    // 已经有了就不用再存一份（同一张图被多人引用时很常见）
    let sha256 = pixora_crypto::hash::sha256_hex(&encoded);
    let thumb_id = match state
        .repo
        .find_blob_by_sha(&sha256, storage_kind::THUMB)
        .await?
    {
        Some(existing) => existing.id,
        None => {
            let key = cas_key(&sha256, "jpg");
            let (resolved, _) = state
                .storage
                .store_object(
                    &key,
                    Box::new(std::io::Cursor::new(encoded.clone())),
                    encoded.len() as u64,
                    "image/jpeg",
                    false,
                )
                .await?;
            let driver_name = resolved.driver_name().to_string();
            let backend_id = resolved.backend_id();
            state
                .repo
                .insert_blob_if_absent(&NewBlob {
                    sha256,
                    kind: storage_kind::THUMB.to_string(),
                    size: encoded.len() as i64,
                    mime_type: "image/jpeg".to_string(),
                    ext: "jpg".to_string(),
                    width: None,
                    height: None,
                    storage_driver: driver_name,
                    storage_key: resolved.key,
                    backend_id,
                })
                .await?
                .id
        }
    };

    state
        .repo
        .replace_image_thumb(image_id, thumb_id, old_thumb_id)
        .await?;
    Ok(true)
}

/// 单张：读原图 → 缩放 → 编码 → 存成实体 → 挂到图片上。
async fn generate_for(state: &Arc<AppState>, image_id: i64, blob_id: i64) -> pixora_core::Result<bool> {
    let Some(blob) = state.repo.find_blob_by_id(blob_id).await? else {
        return Ok(false);
    };
    if blob.size > MAX_SOURCE_BYTES as i64 {
        return Ok(false);
    }

    // 读取原图字节。远端驱动会把它整份取回来，这也是只在本地驱动上兜底的原因。
    let mut reader = state
        .storage
        .open_object(&blob.storage_driver, blob.backend_id, &blob.storage_key)
        .await?;
    let mut source = Vec::with_capacity(blob.size.clamp(0, 8 * 1024 * 1024) as usize);
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut source)
        .await
        .map_err(|err| AppError::internal(format!("读取原图失败：{err}")))?;

    // 解码与缩放是 CPU 活，放到阻塞线程池，别占着异步运行时
    let encoded = tokio::task::spawn_blocking(move || encode_thumb(&source))
        .await
        .map_err(|err| AppError::internal(format!("缩略图任务失败：{err}")))?
        .map_err(AppError::InvalidImage)?;

    let sha256 = pixora_crypto::hash::sha256_hex(&encoded);
    // 同一张图可能被多个用户引用，缩略图同样要走去重
    if let Some(existing) = state
        .repo
        .find_blob_by_sha(&sha256, storage_kind::THUMB)
        .await?
    {
        state.repo.attach_image_thumb(image_id, existing.id).await?;
        return Ok(true);
    }

    let key = cas_key(&sha256, "jpg");
    let (resolved, _) = state
        .storage
        .store_object(
            &key,
            Box::new(std::io::Cursor::new(encoded.clone())),
            encoded.len() as u64,
            "image/jpeg",
            false,
        )
        .await?;
    let driver_name = resolved.driver_name().to_string();
    let backend_id = resolved.backend_id();

    let thumb = state
        .repo
        .insert_blob_if_absent(&NewBlob {
            sha256,
            kind: storage_kind::THUMB.to_string(),
            size: encoded.len() as i64,
            mime_type: "image/jpeg".to_string(),
            ext: "jpg".to_string(),
            width: None,
            height: None,
            storage_driver: driver_name,
            storage_key: resolved.key,
            backend_id,
        })
        .await?;

    state.repo.attach_image_thumb(image_id, thumb.id).await?;
    Ok(true)
}

/// 纯计算部分：解码 → 等比缩放到最长边 1024px → 编码 JPEG。
///
/// 单独抽出来是为了能写单测：给一段构造出来的 PNG，验证输出是能被解回来的 JPEG。
fn encode_thumb(source: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Cursor;

    let image = image::ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|err| format!("无法识别图片格式：{err}"))?
        .decode()
        .map_err(|err| format!("解码失败：{err}"))?;

    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err("图片尺寸为 0".into());
    }

    // 等比缩放到最长边 1024：不放大，小图保持原尺寸
    let scale = (MAX_EDGE as f32 / width.max(height) as f32).min(1.0);
    let source_rgb = image.to_rgb8();
    let resized = if scale < 1.0 {
        let target_w = ((width as f32 * scale).round() as u32).max(1);
        let target_h = ((height as f32 * scale).round() as u32).max(1);
        image::imageops::resize(
            &source_rgb,
            target_w,
            target_h,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        source_rgb
    };

    let mut out = Vec::new();
    // encode_image 自己从图里读尺寸，不必再传一遍（传错会写出损坏的文件）
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, QUALITY);
    encoder
        .encode_image(&resized)
        .map_err(|err| format!("编码 JPEG 失败：{err}"))?;
    Ok(out)
}

/// 管理后台「重新生成缩略图」按钮的实现。
pub async fn rebuild_all(state: &Arc<AppState>) -> pixora_core::Result<RebuildOutcome> {
    let enabled = state.settings.upload().await?.thumb_server_fallback;
    if !enabled {
        return Ok(RebuildOutcome {
            enabled: false,
            pending: 0,
        });
    }
    // 不在这里同步跑完：缺的图可能成千上万，交给后台任务按轮次处理
    let pending = state.repo.list_images_missing_thumb(500, true).await?.len() as i64;
    Ok(RebuildOutcome {
        enabled: true,
        pending,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildOutcome {
    pub enabled: bool,
    pub pending: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张纯色 PNG，验证缩略图生成链路。
    fn sample_png(width: u32, height: u32) -> Vec<u8> {
        let buffer = image::RgbImage::from_pixel(width, height, image::Rgb([120, 180, 240]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(buffer)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Png,
            )
            .expect("编码 PNG");
        out
    }

    #[test]
    fn thumbnail_is_jpeg_and_bounded() {
        let source = sample_png(1600, 1200);
        let thumb = encode_thumb(&source).expect("生成缩略图");

        // 输出是可解回来的 JPEG
        let decoded = image::ImageReader::new(std::io::Cursor::new(&thumb))
            .with_guessed_format()
            .expect("识别格式")
            .decode()
            .expect("解码缩略图");
        assert!(thumb.starts_with(&[0xFF, 0xD8, 0xFF]), "JPEG 魔数");
        assert_eq!(decoded.width(), MAX_EDGE);
        assert_eq!(decoded.height(), MAX_EDGE * 3 / 4, "等比缩放：1600×1200 → 1024×768");
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let source = sample_png(120, 90);
        let thumb = encode_thumb(&source).expect("生成缩略图");
        let decoded = image::ImageReader::new(std::io::Cursor::new(&thumb))
            .with_guessed_format()
            .expect("识别格式")
            .decode()
            .expect("解码缩略图");
        assert_eq!((decoded.width(), decoded.height()), (120, 90));
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(encode_thumb(b"not an image at all").is_err());
    }
}
