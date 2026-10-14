//! CAS 键布局与镜像树路径。
//!
//! 直链对外只暴露一层目录加哈希文件名，真实位置藏在 `blobs/{前2位}/{sha256}.{ext}`，
//! 前两位分成 256 个桶，免得一个目录里堆几十万个文件（大多数文件系统的目录索引
//! 在这种量级上会明显变慢）。

/// 内容寻址的存储键。`sha256` 必须是小写十六进制。
pub fn cas_key(sha256: &str, ext: &str) -> String {
    let prefix = sha256.get(0..2).unwrap_or("00");
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() {
        format!("blobs/{prefix}/{sha256}")
    } else {
        format!("blobs/{prefix}/{sha256}.{ext}")
    }
}

/// 镜像树里的相对路径：`users/{uid}/{相册 slug}/{哈希}.{ext}`。
///
/// 这棵树是给人看的（文件管理器里按用户、按相册翻），磁盘占用靠硬链接
/// 与 CAS 实体共享，不重复计费。
pub fn view_rel_path(uid: &str, album_slug: &str, sha256: &str, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    let file = if ext.is_empty() {
        sha256.to_string()
    } else {
        format!("{sha256}.{ext}")
    };
    format!("users/{uid}/{album_slug}/{file}")
}

/// 相册 slug 的净化：只留字母数字与中日韩字符、连字符、下划线，
/// 其余替换成下划线。物理目录名要能跨平台安全落盘。
pub fn sanitize_slug(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let keep = ch.is_alphanumeric() || ch == '-' || ch == '_';
        if keep {
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push('_');
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "album".to_string()
    } else {
        trimmed.chars().take(64).collect()
    }
}

/// 从存储键里取扩展名，给「按扩展名分类」这类展示用。
pub fn ext_of(key: &str) -> &str {
    key.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_layout_matches_design_doc() {
        let sha = "a3f9c1b8d2e4f6a7b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2";
        assert_eq!(
            cas_key(sha, "webp"),
            "blobs/a3/a3f9c1b8d2e4f6a7b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2.webp"
        );
        assert_eq!(cas_key(sha, ".jpg"), cas_key(sha, "jpg"));
        assert!(cas_key("ab", "").ends_with("/ab"));
    }

    #[test]
    fn view_path_keeps_readable_chinese_album() {
        assert_eq!(
            view_rel_path("1001", "旅行", "a3f9c1b8", "webp"),
            "users/1001/旅行/a3f9c1b8.webp"
        );
    }

    #[test]
    fn slug_sanitizing_is_filesystem_safe() {
        assert_eq!(sanitize_slug("旅行"), "旅行");
        assert_eq!(sanitize_slug("my album/2024"), "my_album_2024");
        assert_eq!(sanitize_slug("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_slug("   "), "album");
        assert!(sanitize_slug("..").len() <= 64);
    }

    #[test]
    fn ext_parsing() {
        assert_eq!(ext_of("blobs/ab/abcdef.jpg"), "jpg");
        assert_eq!(ext_of("no-extension"), "");
    }
}
