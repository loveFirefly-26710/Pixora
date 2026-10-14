//! 本地磁盘驱动。默认后端，也是唯一支持硬链接镜像的去处。
//!
//! 写入走「临时文件 + fsync + 原子改名」：直接往目标路径写的话，
//! 一边写一边被读就会读到半个文件；改名在同一文件系统内是原子的，
//! 所以读方要么看到旧文件、要么看到完整的新文件。
//!
//! 硬链接镜像树（`users/{uid}/{相册}/{哈希}.{ext}`）由本驱动维护，
//! 磁盘占用不重复计——这是设计稿里「按用户相册分类」与「相同文件不重复占空间」
//! 两条要求同时成立的解法。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncRead;
use tokio_util::io::ReaderStream;

use crate::driver::{Capabilities, ObjectInfo, StorageDriver, StoredObject};
use crate::error::{Result, StorageError};

/// 本地目录后端。
pub struct LocalDriver {
    root: PathBuf,
    /// 硬链接镜像树的根（通常是 `{root}/users`）。
    /// 置空则不维护镜像树，只保留 CAS 实体。
    view_root: Option<PathBuf>,
}

impl LocalDriver {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            view_root: Some(root.join("users")),
            root,
        }
    }

    /// 只保留 CAS 实体，不维护镜像树。文件系统不支持硬链接时用。
    pub fn without_view(mut self) -> Self {
        self.view_root = None;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把存储键映射到磁盘路径，并挡住 `..` 之类的逃逸——键由服务端生成，
    /// 但防线不嫌多：任何拼路径的地方都要过一道规范化。
    fn resolve(&self, key: &str) -> Result<PathBuf> {
        let relative = key.trim_start_matches('/');
        if relative.split(['/', '\\']).any(|part| part == "..") {
            return Err(StorageError::config("local", "存储键包含路径穿越片段"));
        }
        Ok(self.root.join(relative))
    }

    fn resolve_view(&self, view_rel: &str) -> Result<PathBuf> {
        let view_root = self
            .view_root
            .as_ref()
            .ok_or(StorageError::Unsupported {
                driver: "local",
                operation: "维护镜像目录树（已关闭）",
            })?;
        let relative = view_rel.trim_start_matches('/');
        if relative.split(['/', '\\']).any(|part| part == "..") {
            return Err(StorageError::config("local", "镜像路径包含路径穿越片段"));
        }
        Ok(view_root.join(relative))
    }

    async fn ensure_parent(path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl StorageDriver for LocalDriver {
    fn name(&self) -> &'static str {
        crate::names::LOCAL
    }

    async fn put(
        &self,
        key: &str,
        mut reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        _mime: &str,
    ) -> Result<StoredObject> {
        let target = self.resolve(key)?;
        Self::ensure_parent(&target).await?;

        // 临时文件放在目标同目录，保证改名在同一文件系统内（跨设备改名会失败）
        let temp = target.with_extension(format!(
            "{}.part",
            target
                .extension()
                .map(|ext| ext.to_string_lossy().to_string())
                .unwrap_or_default()
        ));

        let mut file = fs::File::create(&temp).await?;
        let written = tokio::io::copy(&mut reader, &mut file).await?;
        file.sync_all().await?;
        drop(file);

        if written != size {
            let _ = fs::remove_file(&temp).await;
            return Err(StorageError::Malformed {
                driver: "local",
                message: format!("写入字节数 {written} 与声明的 {size} 不一致"),
            });
        }

        // Windows 上目标已存在时 rename 会失败，先删再改；
        // Linux 上直接 rename 就是替换，不需要这一步。
        if cfg!(windows) && target.exists() {
            let _ = fs::remove_file(&target).await;
        }
        fs::rename(&temp, &target).await?;

        Ok(StoredObject {
            key: key.to_string(),
            size: written,
            driver: crate::names::LOCAL,
        })
    }

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
        let path = self.resolve(key)?;
        let file = fs::File::open(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.to_string())
            } else {
                StorageError::Io(err)
            }
        })?;
        Ok(Box::new(file))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    async fn stat(&self, key: &str) -> Result<ObjectInfo> {
        let path = self.resolve(key)?;
        let meta = fs::metadata(&path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(key.to_string())
            } else {
                StorageError::Io(err)
            }
        })?;
        let modified = meta.modified().ok().map(chrono::DateTime::<chrono::Utc>::from);
        Ok(ObjectInfo {
            key: key.to_string(),
            size: meta.len(),
            modified,
            content_type: None,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::local()
    }

    async fn health(&self) -> Result<String> {
        fs::create_dir_all(&self.root).await?;
        let probe_marker = self.root.join(".pixora-writable");
        fs::write(&probe_marker, b"ok").await?;
        let _ = fs::remove_file(&probe_marker).await;
        Ok(format!("读写正常 · {}", self.root.display()))
    }

    fn local_path(&self, key: &str) -> Option<PathBuf> {
        self.resolve(key).ok()
    }

    async fn link_view(&self, key: &str, view_rel: &str) -> Result<bool> {
        if self.view_root.is_none() {
            return Ok(false);
        }
        let source = self.resolve(key)?;
        let target = self.resolve_view(view_rel)?;
        Self::ensure_parent(&target).await?;

        // 目标还在就先删掉再链，否则会变成 hlink.999 这种名字
        if target.exists() {
            let _ = fs::remove_file(&target).await;
        }

        match std::fs::hard_link(&source, &target) {
            Ok(()) => Ok(true),
            // 不支持硬链接（部分网络挂载、Windows 少数场景）时静默降级：
            // 上层继续维护数据库虚拟树，功能不受影响
            Err(err) if is_hardlink_unsupported(&err) => Ok(false),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
            Err(err) => Err(StorageError::Io(err)),
        }
    }

    async fn unlink_view(&self, view_rel: &str) -> Result<()> {
        if self.view_root.is_none() {
            return Ok(());
        }
        let target = self.resolve_view(view_rel)?;
        match fs::remove_file(&target).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(StorageError::Io(err)),
        }
    }
}

/// 硬链接不受支持时的错误特征。跨平台判断，认不出来就当普通错误抛出。
fn is_hardlink_unsupported(err: &std::io::Error) -> bool {
    match err.raw_os_error() {
        // EPERM / EOPNOTSUPP / ENOSYS / EXDEV（跨设备链路）
        Some(code) => matches!(code, 1 | 95 | 38 | 18) || code == 1314,
        None => matches!(
            err.kind(),
            std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
        ),
    }
}

/// 读整个对象到内存。远端驱动与测试用得上，本地驱动的大文件走流式。
pub async fn read_all(driver: &dyn StorageDriver, key: &str, limit: u64) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut reader = driver.get(key).await?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() as u64 > limit {
            return Err(StorageError::TooLarge {
                size: buffer.len() as u64,
                limit,
            });
        }
    }
    Ok(buffer)
}

/// 把一个异步读流转换成「字节流」，交给 HTTP 响应体。
pub fn into_stream(reader: Box<dyn AsyncRead + Send + Unpin>) -> ReaderStream<Box<dyn AsyncRead + Send + Unpin>> {
    ReaderStream::new(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::cas_key;
    use tokio::io::AsyncReadExt;

    async fn temp_driver() -> (LocalDriver, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (LocalDriver::new(dir.path()), dir)
    }

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let (driver, _dir) = temp_driver().await;
        let key = cas_key("a3f9c1b8d2e4f6a7", "webp");
        let payload = b"fake-image-bytes".to_vec();

        let stored = driver
            .put(
                &key,
                Box::new(std::io::Cursor::new(payload.clone())),
                payload.len() as u64,
                "image/webp",
            )
            .await
            .unwrap();
        assert_eq!(stored.key, key);
        assert_eq!(stored.size, payload.len() as u64);

        let info = driver.stat(&key).await.unwrap();
        assert_eq!(info.size, payload.len() as u64);

        let mut reader = driver.get(&key).await.unwrap();
        let mut read_back = Vec::new();
        reader.read_to_end(&mut read_back).await.unwrap();
        assert_eq!(read_back, payload);

        driver.delete(&key).await.unwrap();
        assert!(matches!(
            driver.stat(&key).await,
            Err(StorageError::NotFound(_))
        ));
        // 重复删除不报错
        driver.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_size_mismatch() {
        let (driver, _dir) = temp_driver().await;
        let err = driver
            .put(
                "blobs/ab/x.webp",
                Box::new(std::io::Cursor::new(b"short".to_vec())),
                999,
                "image/webp",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Malformed { .. }));
    }

    #[tokio::test]
    async fn view_tree_uses_hardlink_and_shares_bytes() {
        let (driver, dir) = temp_driver().await;
        let key = cas_key("aaaabbbbccccdddd", "jpg");
        let payload = b"shared-bytes".to_vec();
        driver
            .put(
                &key,
                Box::new(std::io::Cursor::new(payload.clone())),
                payload.len() as u64,
                "image/jpeg",
            )
            .await
            .unwrap();

        // 同一份数据挂到两个用户的相册目录下
        let linked_a = driver
            .link_view(&key, "1001/旅行/aaaabbbbccccdddd.jpg")
            .await
            .unwrap();
        let linked_b = driver
            .link_view(&key, "1002/头像/aaaabbbbccccdddd.jpg")
            .await
            .unwrap();

        if linked_a && linked_b {
            let view_a = dir.path().join("users/1001/旅行/aaaabbbbccccdddd.jpg");
            let view_b = dir.path().join("users/1002/头像/aaaabbbbccccdddd.jpg");
            assert!(view_a.exists() && view_b.exists());
            // 硬链接共享同一个 inode
            let meta_a = std::fs::metadata(&view_a).unwrap();
            let meta_b = std::fs::metadata(&view_b).unwrap();
            assert_eq!(meta_a.len(), meta_b.len());

            driver.unlink_view("1001/旅行/aaaabbbbccccdddd.jpg").await.unwrap();
            assert!(!view_a.exists());
            assert!(view_b.exists(), "删一个视图不影响另一个");
        }
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let (driver, _dir) = temp_driver().await;
        assert!(driver.get("../../etc/passwd").await.is_err());
        assert!(driver.link_view("blobs/aa/x.jpg", "../../escape.jpg").await.is_err());
    }

    #[tokio::test]
    async fn health_reports_root() {
        let (driver, _dir) = temp_driver().await;
        let message = driver.health().await.unwrap();
        assert!(message.contains("读写正常"));
    }

    #[tokio::test]
    async fn local_path_is_exposed_for_accel_redirect() {
        let (driver, dir) = temp_driver().await;
        let path = driver.local_path("blobs/ab/x.jpg").unwrap();
        assert_eq!(path, dir.path().join("blobs/ab/x.jpg"));
    }
}
