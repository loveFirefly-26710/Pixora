//! 容量探测。
//!
//! 这里要修的是设计稿点名的那个坑：存储目录挂在 OSS 或网络盘上时，
//! `statfs` 拿到的是挂载点所在宿主机的容量，跟真实可用空间毫无关系
//! （ossfs 常见返回 16 EB 的伪值）。所以探测结果必须带上「文件系统类型」，
//! 让上层能把网络挂载识别出来并自动降级为「管理员手填总额」。
//!
//! 探测逻辑读 `f_type` 对照已知网络文件系统类型表；管理员也可以在界面上
//! 直接指定后端类型覆盖自动探测。两条路都堵死误判。
//!
//! 非 Unix 平台（本机是 Windows 开发机）不返回容量，直接走「手动填写」分支——
//! 生产环境跑在 Linux 容器里，`statfs` 才是真实路径。

use std::path::Path;

use crate::error::Result;

/// 文件系统的性质。只有 `Local` 的容量值可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// 本地块设备（ext4、xfs、btrfs、ntfs…），容量真实
    Local,
    /// 网络或对象存储挂载（fuse、nfs、cifs、ossfs…），容量是伪值
    Network,
    /// 认不出来或平台不支持，交由管理员决定
    Unknown,
}

impl FsKind {
    pub fn label(self) -> &'static str {
        match self {
            FsKind::Local => "本地文件系统",
            FsKind::Network => "网络或对象存储挂载",
            FsKind::Unknown => "未知",
        }
    }

    /// 容量值是否可以直接展示给管理员。
    pub fn capacity_trustworthy(self) -> bool {
        self == FsKind::Local
    }
}

/// 探测结果。
#[derive(Debug, Clone)]
pub struct CapacityProbe {
    pub total: u64,
    pub available: u64,
    pub fs_kind: FsKind,
    /// `statfs` 的 `f_type` 原始值，界面会展示它，便于管理员核对
    pub fs_type: u64,
    /// 人类可读的文件系统名
    pub fs_name: String,
}

impl CapacityProbe {
    /// 探测不到或不可信时的空结果。
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            total: 0,
            available: 0,
            fs_kind: FsKind::Unknown,
            fs_type: 0,
            fs_name: reason.into(),
        }
    }

    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.used() as f64 / self.total as f64) * 100.0
    }
}

#[cfg(unix)]
pub fn probe(path: &Path) -> Result<CapacityProbe> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| crate::StorageError::config("local", "路径含空字节，无法探测容量"))?;

    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(crate::StorageError::Io(std::io::Error::last_os_error()));
    }

    let fs_type = stat.f_type as u64;
    let block_size = stat.f_bsize as u64;
    let total = stat.f_blocks as u64 * block_size;
    let available = stat.f_bavail as u64 * block_size;

    let (fs_kind, fs_name) = classify(fs_type);
    Ok(CapacityProbe {
        total,
        available,
        fs_kind,
        fs_type,
        fs_name,
    })
}

#[cfg(not(unix))]
pub fn probe(_path: &Path) -> Result<CapacityProbe> {
    Ok(CapacityProbe::unknown("当前平台不支持 statfs 探测，请用手动填写"))
}

/// `f_type` 与文件系统类型的对照表。数值来自各文件系统在内核里的魔术数。
#[cfg(unix)]
fn classify(fs_type: u64) -> (FsKind, String) {
    const NFS: u64 = 0x6969;
    const CIFS: u64 = 0xFF53_4D42;
    const SMB2: u64 = 0xFE53_4D42;
    const FUSE: u64 = 0x6573_5546;
    const AFS: u64 = 0x5346_414F;
    const NINE_P: u64 = 0x0102_1997;
    const CEPH: u64 = 0x00C3_6400;
    const LUSTRE: u64 = 0x0BD0_0BD0;
    const OCFS2: u64 = 0x7461_6331;
    const CODA: u64 = 0x7375_7245;
    const OSSFS_HINT: u64 = FUSE;

    let (kind, name) = match fs_type {
        0xEF53 => (FsKind::Local, "ext4"),
        0x58465342 => (FsKind::Local, "xfs"),
        0x9123683E => (FsKind::Local, "btrfs"),
        0x2011BAB0 => (FsKind::Local, "exfat"),
        0x4D44 => (FsKind::Local, "msdos/fat"),
        0x01021994 => (FsKind::Local, "tmpfs"),
        NFS => (FsKind::Network, "nfs"),
        CIFS => (FsKind::Network, "cifs"),
        SMB2 => (FsKind::Network, "smb2"),
        FUSE => (FsKind::Network, "fuse（ossfs / rclone / sshfs 等）"),
        AFS => (FsKind::Network, "afs / glusterfs"),
        NINE_P => (FsKind::Network, "9p"),
        CEPH => (FsKind::Network, "cephfs"),
        LUSTRE => (FsKind::Network, "lustre"),
        OCFS2 => (FsKind::Network, "ocfs2 / gfs2"),
        CODA => (FsKind::Network, "coda"),
        _ => (FsKind::Unknown, "未知文件系统"),
    };

    if fs_type == OSSFS_HINT {
        // ossfs 是 FUSE 的典型用户，单独点出来——设计稿里举的就是它
        return (kind, name);
    }
    (kind, name.to_string())
}

/// 把探测结果压成一句话，给管理界面直接展示。
pub fn describe(probe: &CapacityProbe) -> String {
    if probe.fs_kind == FsKind::Unknown && probe.total == 0 {
        return probe.fs_name.clone();
    }
    if probe.fs_kind.capacity_trustworthy() {
        format!(
            "{} · 总容量 {:.1} GB · 可用 {:.1} GB",
            probe.fs_name,
            probe.total as f64 / 1024.0 / 1024.0 / 1024.0,
            probe.available as f64 / 1024.0 / 1024.0 / 1024.0
        )
    } else {
        format!(
            "{}（f_type=0x{:x}）· statfs 返回 {:.1} GB，这是挂载伪值，不是真实容量，已降级为手动填写",
            probe.fs_name,
            probe.fs_type,
            probe.total as f64 / 1024.0 / 1024.0 / 1024.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_probe_is_not_trustworthy() {
        let probe = CapacityProbe::unknown("测试");
        assert!(!probe.fs_kind.capacity_trustworthy());
        assert_eq!(probe.used_percent(), 0.0);
        assert!(describe(&probe).contains("测试"));
    }

    #[cfg(unix)]
    #[test]
    fn network_filesystems_are_detected() {
        // ossfs 挂在 FUSE 上，正是设计稿举的那个例子
        assert_eq!(classify(0x6573_5546).0, FsKind::Network);
        assert_eq!(classify(0x6969).0, FsKind::Network);
        assert_eq!(classify(0xEF53).0, FsKind::Local);
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_falls_back_to_manual() {
        let probe = probe(Path::new(".")).unwrap();
        assert_eq!(probe.fs_kind, FsKind::Unknown);
        assert_eq!(probe.total, 0);
    }
}
