//! 驱动的公共类型：能力声明、返回值与元信息。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::io::AsyncRead;

use crate::error::Result;

/// 驱动能力。上层靠它决定策略，而不是判断驱动名字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// 支持硬链接（本地文件系统支持；对象存储与网络挂载不支持）
    pub hardlink: bool,
    /// 能探测容量（`statfs` 之类；网络挂载探测到的是伪值，见 `capacity` 模块）
    pub capacity_probe: bool,
    /// 支持流式读写（大文件不必整份进内存）
    pub streaming: bool,
    /// 内容可被公网直接访问（GitHub 的 raw 链接就是），生成直链时用得上
    pub public_url: bool,
}

impl Capabilities {
    pub const fn local() -> Self {
        Self {
            hardlink: true,
            capacity_probe: true,
            streaming: true,
            public_url: false,
        }
    }

    pub const fn blob_backed() -> Self {
        Self {
            hardlink: false,
            capacity_probe: false,
            streaming: true,
            public_url: false,
        }
    }

    pub const fn remote(public_url: bool) -> Self {
        Self {
            hardlink: false,
            capacity_probe: false,
            streaming: false,
            public_url,
        }
    }
}

/// 写入结果。`key` 可能与传入的不同——远端驱动会用它自己的命名空间
/// （比如 Telegram 的文件 id），调用方要把返回的 key 落库。
#[derive(Debug, Clone)]
pub struct StoredObject {
    pub key: String,
    pub size: u64,
    pub driver: &'static str,
}

/// 对象元信息。
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub content_type: Option<String>,
}

/// 存储驱动接口。核心系统只依赖这个 trait。
///
/// 所有实现都必须 `Send + Sync`，参数用 `Box<dyn AsyncRead + Send + Unpin>`——
/// 这两条在动手写并发驱动之前就定死了，避免后期大面积返工。
#[async_trait]
pub trait StorageDriver: Send + Sync {
    /// 驱动名，与 `storage_backends.driver` 取值一致。
    fn name(&self) -> &'static str;

    /// 实例标识：同一类型挂了多个后端时用它区分（如 `github#2`）。
    /// 单实例驱动可以直接返回驱动名。
    fn instance_key(&self) -> String {
        self.name().to_string()
    }

    async fn put(
        &self,
        key: &str,
        reader: Box<dyn AsyncRead + Send + Unpin>,
        size: u64,
        mime: &str,
    ) -> Result<StoredObject>;

    async fn get(&self, key: &str) -> Result<Box<dyn AsyncRead + Send + Unpin>>;

    async fn delete(&self, key: &str) -> Result<()>;

    async fn stat(&self, key: &str) -> Result<ObjectInfo>;

    fn capabilities(&self) -> Capabilities;

    /// 连通性自检。管理后台的「测试连接」按钮调用它；
    /// 返回一句人话描述（比如「连通，仓库 example-org/img-bed」）。
    async fn health(&self) -> Result<String> {
        Ok(format!("{} 就绪", self.name()))
    }

    /// 本地文件系统上的真实路径。只有本地驱动会返回 `Some`——
    /// 上层据此决定能不能走 `X-Accel-Redirect` 让 Caddy 直接发文件，
    /// 进程完全不参与数据传输。
    fn local_path(&self, _key: &str) -> Option<std::path::PathBuf> {
        None
    }

    /// 在「用户/相册」镜像树里建一个硬链接。返回 `false` 表示文件系统不支持，
    /// 上层降级为「只维护数据库虚拟树」。
    async fn link_view(&self, _key: &str, _view_rel: &str) -> Result<bool> {
        Ok(false)
    }

    /// 删掉镜像树里的链接（物理文件由引用计数归零后的 GC 处理）。
    async fn unlink_view(&self, _view_rel: &str) -> Result<()> {
        Ok(())
    }
}
