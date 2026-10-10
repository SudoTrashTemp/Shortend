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
    Ok(VolumeProbe {
        dev: meta.dev(),
        fs_uuid: fs_uuid(meta.dev()),
        fs_type: fs_type_from_magic(sfs.f_type as u64),
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
        0x6573_5546 => FsType::Other("fuseblk".into()), // e.g. ntfs-3g, exfat-fuse
        0x794c_7630 => FsType::Other("overlayfs".into()),
        other => FsType::Other(format!("fs-0x{other:x}")),
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
