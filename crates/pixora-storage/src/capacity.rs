//! 容量探测。
//!
//! 这里要修的是设计稿点名的那个坑：存储目录挂在 OSS 或网络盘上时，
//! `statfs` 拿到的是挂载点所在宿主机的容量，跟真实可用空间毫无关系
//! （ossfs 常见返回 16 EB 的伪值）。所以探测结果必须带上「文件系统类型」，
//! 让上层能把网络挂载识别出来并自动降级为「管理员手填总额」。
//!
//! 两个平台都实现：
//!   - Linux：`statfs` + `f_type` 对照表。注意容器里常见的是
//!     overlayfs（Docker 的默认存储驱动），它属于本地联合文件系统，
//!     必须算「可信」，否则一键部署出来的实例一律显示 0 B；
//!   - Windows：`GetDiskFreeSpaceExW` 问容量、`GetDriveTypeW` 判盘类型
//!     （固定盘/可移动盘算本地，网络驱动器算网络，光驱之类算未知）。
//! 其余 Unix（macOS / BSD）的 `statfs` 结构体与 Linux 不同，未适配，按未知降级。
//!
//! 认不出来的类型一律降级为「手动填写」：宁可让管理员手填，也不展示一个假数字。

use std::path::Path;

use crate::error::Result;

/// 文件系统的性质。只有 `Local` 的容量值可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// 本地块设备（ext4、xfs、btrfs、overlayfs、ntfs…），容量真实
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
    /// Unix 上是 `statfs` 的 `f_type`，Windows 上是盘类型（DriveType）；
    /// 界面会展示它，便于管理员核对
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

/// 「探测路径」：数据目录可能还没建，往上找到第一个存在的祖先再问。
/// 容量是按文件系统算的，同一个分区里换目录不影响结果。
#[cfg(any(target_os = "linux", windows))]
fn probe_target(path: &Path) -> &Path {
    let mut target = path;
    while !target.exists() {
        match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => target = parent,
            _ => break,
        }
    }
    target
}

#[cfg(target_os = "linux")]
pub fn probe(path: &Path) -> Result<CapacityProbe> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let target = probe_target(path);
    let c_path = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| crate::StorageError::config("local", "路径含空字节，无法探测容量"))?;

    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(crate::StorageError::Io(std::io::Error::last_os_error()));
    }

    let fs_type = stat.f_type as u64;
    // f_frsize 才是记账用的块大小；为 0 时退回 f_bsize（少数文件系统不填）
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    let total = stat.f_blocks as u64 * block_size;
    // f_bavail：非特权用户可用，和 df 显示的口径一致
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

/// `f_type` 与文件系统类型的对照表。数值来自各文件系统在内核里的魔术数。
#[cfg(target_os = "linux")]
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

    let (kind, name) = match fs_type {
        // 本地块设备
        0xEF53 => (FsKind::Local, "ext2/3/4"),
        0x58465342 => (FsKind::Local, "xfs"),
        0x9123683E => (FsKind::Local, "btrfs"),
        0x2FC12FC1 => (FsKind::Local, "zfs"),
        0xF2F52010 => (FsKind::Local, "f2fs"),
        0x3153464A => (FsKind::Local, "jfs"),
        0x52654973 => (FsKind::Local, "reiserfs"),
        0x2011BAB0 => (FsKind::Local, "exfat"),
        0x5346544E => (FsKind::Local, "ntfs"),
        0x4D44 => (FsKind::Local, "msdos/fat"),
        0x01021994 => (FsKind::Local, "tmpfs"),
        0x858458F6 => (FsKind::Local, "ramfs"),
        0x73717368 => (FsKind::Local, "squashfs（只读镜像层）"),
        // 容器里的联合文件系统：Docker 默认就是 overlayfs，容量是真实可用的
        0x794C7630 => (FsKind::Local, "overlayfs（容器联合文件系统）"),
        0x61756673 => (FsKind::Local, "aufs（容器联合文件系统）"),
        // 网络与对象存储挂载：容量是伪值
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

    (kind, name.to_string())
}

/// 其余 Unix（macOS / BSD）：`statfs` 结构体与 Linux 不同（没有 `f_frsize`，
/// `f_type` 也不是同一套魔数），未做适配。按未知处理，界面上退回管理员手填。
/// 项目支持的目标平台是 Linux（生产）与 Windows（开发）。
#[cfg(all(unix, not(target_os = "linux")))]
pub fn probe(_path: &Path) -> Result<CapacityProbe> {
    Ok(CapacityProbe::unknown(
        "当前平台未适配容量探测（仅 Linux 与 Windows），请到后台手填系统总额",
    ))
}

/// Windows：`GetDiskFreeSpaceExW` 问容量，`GetDriveTypeW` 判本地还是网络盘。
#[cfg(windows)]
pub fn probe(path: &Path) -> Result<CapacityProbe> {
    use std::os::windows::ffi::OsStrExt;

    // Win32 的盘类型
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_available: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            volume_name: *mut u16,
            volume_name_size: u32,
            serial: *mut u32,
            max_component_len: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_size: u32,
        ) -> i32;
    }

    let target = probe_target(path);
    let wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: 入参是以 NUL 结尾的宽字符串；三个出参都是有效的栈上 u64。
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_available, &mut total, &mut total_free)
    };
    if ok == 0 {
        return Err(crate::StorageError::Io(std::io::Error::last_os_error()));
    }

    // 盘符根（`E:\`）或 UNC 根（`\\server\share`）：卷类型与文件系统名都按根路径问。
    //
    // canonicalize 给的是 `\\?\C:\...` 与 `\\?\UNC\server\share\...` 这两种形式：
    // `\\?\` 这段前缀（四个字符，带尾部反斜杠）Win32 的盘符 API 不认，要先摘掉；
    // UNC 那种摘完还得把 `\\server\share` 拼回去。不处理这两点，
    // GetDriveTypeW 会答「不是根目录」，容量就白降级了。
    let full = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf())
        .to_string_lossy()
        .replace('/', "\\");
    let stripped = full.strip_prefix(r"\\?\").unwrap_or(&full).to_string();
    let stripped = stripped
        .strip_prefix("UNC\\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or(stripped);
    let root = if stripped.len() >= 2 && stripped.as_bytes()[1] == b':' {
        format!("{}\\", &stripped[..2])
    } else if let Some(rest) = stripped.strip_prefix("\\\\") {
        let mut parts = rest.split('\\');
        match (parts.next(), parts.next()) {
            (Some(server), Some(share)) if !server.is_empty() && !share.is_empty() => {
                format!(r"\\{server}\{share}")
            }
            _ => stripped.clone(),
        }
    } else {
        stripped.clone()
    };
    let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: 同样是 NUL 结尾的宽字符串；输出缓冲区都是有效的栈上数组。
    let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };

    let mut fs_name_buffer = [0u16; 64];
    let mut volume_name = [0u16; 64];
    let mut serial: u32 = 0;
    let mut max_len: u32 = 0;
    let mut flags: u32 = 0;
    let info_ok = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            volume_name.as_mut_ptr(),
            volume_name.len() as u32,
            &mut serial,
            &mut max_len,
            &mut flags,
            fs_name_buffer.as_mut_ptr(),
            fs_name_buffer.len() as u32,
        )
    };
    let fs_name = if info_ok != 0 {
        let end = fs_name_buffer
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(fs_name_buffer.len());
        String::from_utf16_lossy(&fs_name_buffer[..end])
    } else {
        String::new()
    };

    let (fs_kind, label) = match drive_type {
        DRIVE_FIXED => (
            FsKind::Local,
            if fs_name.is_empty() {
                "固定盘".to_string()
            } else {
                format!("{fs_name}（固定盘）")
            },
        ),
        DRIVE_REMOVABLE => (FsKind::Local, format!("{fs_name}（可移动盘）")),
        DRIVE_RAMDISK => (FsKind::Local, format!("{fs_name}（内存盘）")),
        DRIVE_REMOTE => (FsKind::Network, format!("{fs_name}（网络驱动器）")),
        DRIVE_CDROM => (FsKind::Unknown, format!("{fs_name}（光驱）")),
        _ => (FsKind::Unknown, fs_name),
    };

    Ok(CapacityProbe {
        total,
        available: free_available,
        fs_kind,
        fs_type: drive_type as u64,
        fs_name: label,
    })
}

/// 把探测结果压成一句话，给管理界面直接展示。
pub fn describe(probe: &CapacityProbe) -> String {
    let gb = |bytes: u64| bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    if probe.fs_kind == FsKind::Unknown && probe.total == 0 {
        return probe.fs_name.clone();
    }
    if probe.fs_kind.capacity_trustworthy() {
        // 三个数一起给：已用是整盘口径（含宿主机上与本项目无关的占用），
        // 只看「本项目占了多少」看不出磁盘快满没满
        format!(
            "{} · 总容量 {:.1} GB · 已用 {:.1} GB · 可用 {:.1} GB",
            probe.fs_name,
            gb(probe.total),
            gb(probe.used()),
            gb(probe.available)
        )
    } else if probe.fs_kind == FsKind::Network {
        format!(
            "{} · 文件系统报的 {:.1} GB 是挂载伪值，不是真实容量，已降级为手动填写",
            probe.fs_name,
            gb(probe.total)
        )
    } else {
        format!(
            "{}（类型码 0x{:x}）· 认不出这个文件系统，容量已降级为手动填写",
            probe.fs_name, probe.fs_type
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

    #[cfg(target_os = "linux")]
    #[test]
    fn network_filesystems_are_detected() {
        // ossfs 挂在 FUSE 上，正是设计稿举的那个例子
        assert_eq!(classify(0x6573_5546).0, FsKind::Network);
        assert_eq!(classify(0x6969).0, FsKind::Network);
        assert_eq!(classify(0xEF53).0, FsKind::Local);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn container_filesystems_count_as_local() {
        // Docker 的默认存储驱动是 overlayfs：漏了它，一键部署出来的实例
        // 在「配额与容量」页一律显示 0 B。aufs 是它的前身，同理。
        assert_eq!(classify(0x794C7630).0, FsKind::Local);
        assert_eq!(classify(0x61756673).0, FsKind::Local);
        // 只读镜像层与内存盘也是真实容量
        assert_eq!(classify(0x73717368).0, FsKind::Local);
        assert_eq!(classify(0x01021994).0, FsKind::Local);
    }

    /// 两个平台都要能真问到容量：探到 0 就说明这条路又断了。
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn probing_an_existing_directory_reports_capacity() {
        let dir = std::env::temp_dir();
        let probe = probe(&dir).expect("探测系统临时目录");
        assert!(
            probe.total > 0,
            "总容量不该是 0（探测到的是 {}：{}）",
            probe.fs_name,
            describe(&probe)
        );
        assert!(probe.available <= probe.total);
    }

    /// 盘类型要认得出来：本机的盘（含临时目录所在的盘）不该是「未知」——
    /// 认不出来就说明根路径解析错了，容量会白降级。
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn drive_type_is_recognized() {
        let probe = probe(&std::env::temp_dir()).expect("探测系统临时目录");
        assert_ne!(
            probe.fs_kind,
            FsKind::Unknown,
            "认不出盘类型（类型码 {}）：{}",
            probe.fs_type,
            describe(&probe)
        );
    }

    /// 不存在的目录要往最近的祖先上找：数据目录第一次启动时还没建。
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn probe_walks_up_to_an_existing_ancestor() {
        let mut path = std::env::temp_dir();
        path.push("pixora-not-created-yet");
        path.push("deeper");
        assert!(!path.exists());
        let probe = probe(&path).expect("按祖先目录探测");
        assert!(probe.total > 0);
    }
}
