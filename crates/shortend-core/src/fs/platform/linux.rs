use std::fs::File;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use rustix::fs::{Advice, OFlags, StatVfsMountFlags};

use super::VolumeProbe;
pub use super::unix::{create_staged, free_space, rename_noreplace, sync_dir, volume_id};
use crate::fs::FsType;

/// Reads straight through after `drop_cache`; on Linux a plain file is enough.
pub type UncachedReader = File;

pub fn volume_probe(path: &Path) -> io::Result<VolumeProbe> {
    let meta = std::fs::metadata(path)?;
    let sfs = rustix::fs::statfs(path)?;
    let svfs = rustix::fs::statvfs(path)?;
    let mut fs_type = fs_type_from_magic(sfs.f_type as u64);
    if fs_type == FsType::Other("fuseblk".into())
        && let Some(real) = fuse_backing_type(path)
    {
        fs_type = real;
    }
    Ok(VolumeProbe {
        dev: meta.dev(),
        fs_uuid: fs_uuid(meta.dev()),
        fs_type,
        block_size: svfs.f_frsize.max(512),
        read_only: svfs.f_flag.contains(StatVfsMountFlags::RDONLY),
    })
}

fn fs_type_from_magic(magic: u64) -> FsType {
    match magic {
        0xEF53 => FsType::Ext4,
        0x5846_5342 => FsType::Xfs,
        0x9123_683E => FsType::Btrfs,
        0x2011_BAB0 => FsType::ExFat,
        0x4d44 => FsType::Vfat,
        0x7366_746e => FsType::Ntfs, // ntfs3 kernel driver
        0xFE53_4D42 | 0xFF53_4D42 | 0x517B => FsType::Smb,
        0x6969 => FsType::Nfs,
        0x0102_1994 => FsType::Other("tmpfs".into()),
        0x6573_5546 => FsType::Other("fuseblk".into()), // FUSE: ntfs-3g, exfat-fuse, apfs-fuse…
        0x482b => FsType::HfsPlus,
        0x4253_584e => FsType::Apfs, // "NXSB", the out-of-tree linux-apfs-rw driver
        0x794c_7630 => FsType::Other("overlayfs".into()),
        other => FsType::Other(format!("fs-0x{other:x}")),
    }
}

/// For a FUSE mount, the filesystem on the device or image it serves, from udev's database
/// (readable without privileges) or the on-disk signature. FUSE drivers report only "fuse", so
/// this is how an apfs-fuse or ntfs-3g mount is recognised. `None` if it cannot be told.
fn fuse_backing_type(path: &Path) -> Option<FsType> {
    let path = std::fs::canonicalize(path).ok()?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let (_, source) = mount_source(&info, &path)?;
    let src = Path::new(&source);
    let name = std::fs::metadata(src)
        .ok()
        .filter(|m| m.file_type().is_block_device())
        .and_then(|m| udev_fs_type(m.rdev()))
        .or_else(|| signature_fs_type(src))?;
    Some(match name.as_str() {
        "apfs" => FsType::Apfs,
        "hfsplus" | "hfs" => FsType::HfsPlus,
        "exfat" => FsType::ExFat,
        "ntfs" => FsType::Ntfs,
        "vfat" => FsType::Vfat,
        _ => return None,
    })
}

/// The mount (mount point, source) whose mount point is the longest prefix of `path`, from
/// `/proc/self/mountinfo` content.
pub(crate) fn mount_source(info: &str, path: &Path) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let sep = fields.iter().position(|f| *f == "-")?;
        let (Some(mount_point), Some(source)) = (fields.get(4), fields.get(sep + 2)) else {
            continue;
        };
        let mount_point = unescape_mountinfo(mount_point);
        if path.starts_with(&mount_point)
            && best
                .as_ref()
                .is_none_or(|(m, _)| mount_point.len() >= m.len())
        {
            best = Some((mount_point, unescape_mountinfo(source)));
        }
    }
    best
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo` octal.
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn udev_fs_type(rdev: u64) -> Option<String> {
    let (major, minor) = (rustix::fs::major(rdev), rustix::fs::minor(rdev));
    let data = std::fs::read_to_string(format!("/run/udev/data/b{major}:{minor}")).ok()?;
    data.lines()
        .find_map(|l| l.strip_prefix("E:ID_FS_TYPE="))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Reads the first 4 KiB of a device or image and recognises common signatures.
fn signature_fs_type(src: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut buf = [0u8; 4096];
    let mut f = File::open(src).ok()?;
    let n = f.read(&mut buf).ok()?;
    detect_signature(&buf[..n]).map(str::to_owned)
}

pub(crate) fn detect_signature(b: &[u8]) -> Option<&'static str> {
    let at = |off: usize, sig: &[u8]| b.get(off..off + sig.len()) == Some(sig);
    if at(32, b"NXSB") {
        Some("apfs")
    } else if at(1024, b"H+") || at(1024, b"HX") {
        Some("hfsplus")
    } else if at(3, b"EXFAT   ") {
        Some("exfat")
    } else if at(3, b"NTFS    ") {
        Some("ntfs")
    } else if at(82, b"FAT32") || at(54, b"FAT1") {
        Some("vfat")
    } else {
        None
    }
}

/// Filesystem UUID from /dev/disk/by-uuid, matched by block device number. `None` for
/// filesystems without a backing block device (tmpfs, NFS, btrfs subvolumes).
fn fs_uuid(dev: u64) -> Option<String> {
    for entry in std::fs::read_dir("/dev/disk/by-uuid").ok()?.flatten() {
        if let Ok(m) = std::fs::metadata(entry.path())
            && m.file_type().is_block_device()
            && m.rdev() == dev
        {
            return entry.file_name().into_string().ok();
        }
    }
    None
}

/// Read-only, never follows a final symlink, and avoids atime updates when we own the file
/// (`O_NOATIME` returns EPERM otherwise, in which case we open without it).
pub fn open_source(path: &Path) -> io::Result<File> {
    match super::unix::open_nofollow(path, OFlags::NOATIME) {
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
            super::unix::open_nofollow(path, OFlags::empty())
        }
        other => other,
    }
}

pub fn sync_file(f: &File) -> io::Result<()> {
    f.sync_all()
}

pub fn drop_cache(f: &File) {
    let _ = rustix::fs::fadvise(f, 0, None, Advice::DontNeed);
}

pub fn open_uncached(path: &Path) -> io::Result<UncachedReader> {
    let f = open_source(path)?;
    drop_cache(&f);
    let _ = rustix::fs::fadvise(&f, 0, None, Advice::Sequential);
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_innermost_mount_and_unescapes() {
        let info = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
                    50 22 0:42 / /media/dit/MY\\040CARD ro - fuse /dev/sdb2 ro\n\
                    51 50 0:43 / /media/dit/MY\\040CARD/inner rw - tmpfs tmpfs rw\n";
        let got = mount_source(info, Path::new("/media/dit/MY CARD/A001"));
        assert_eq!(got, Some(("/media/dit/MY CARD".into(), "/dev/sdb2".into())));
        let got = mount_source(info, Path::new("/home/x"));
        assert_eq!(got, Some(("/".into(), "/dev/sda1".into())));
    }

    #[test]
    fn signatures() {
        let mut apfs = vec![0u8; 4096];
        apfs[32..36].copy_from_slice(b"NXSB");
        assert_eq!(detect_signature(&apfs), Some("apfs"));
        let mut hfs = vec![0u8; 4096];
        hfs[1024..1026].copy_from_slice(b"H+");
        assert_eq!(detect_signature(&hfs), Some("hfsplus"));
        assert_eq!(detect_signature(&[0u8; 4096]), None);
    }
}
