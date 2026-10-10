use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{OFlags, StatVfsMountFlags};

use super::VolumeProbe;
pub use super::unix::{create_staged, free_space, rename_noreplace, sync_dir, volume_id};
use crate::fs::FsType;

pub type UncachedReader = File;

pub fn volume_probe(path: &Path) -> io::Result<VolumeProbe> {
    let meta = std::fs::metadata(path)?;
    let sfs = rustix::fs::statfs(path)?;
    let svfs = rustix::fs::statvfs(path)?;
    let name: String = sfs
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect();
    let fs_type = match name.as_str() {
        "apfs" => FsType::Apfs,
        "hfs" => FsType::HfsPlus,
        "exfat" => FsType::ExFat,
        "msdos" => FsType::Vfat,
        "ntfs" => FsType::Ntfs,
        "smbfs" => FsType::Smb,
        "nfs" => FsType::Nfs,
        _ => FsType::Other(name),
    };
    Ok(VolumeProbe {
        dev: meta.dev(),
        fs_uuid: None,
        fs_type,
        block_size: svfs.f_frsize.max(512),
        read_only: svfs.f_flag.contains(StatVfsMountFlags::RDONLY),
    })
}

pub fn open_source(path: &Path) -> io::Result<File> {
    super::unix::open_nofollow(path, OFlags::empty())
}

/// `fsync` on macOS does not flush the drive's write cache; `F_FULLFSYNC` does.
pub fn sync_file(f: &File) -> io::Result<()> {
    // Some filesystems (SMB, FAT) reject F_FULLFSYNC; fall back to a plain fsync there.
    match rustix::fs::fcntl_fullfsync(f) {
        Ok(()) => Ok(()),
        Err(_) => f.sync_all(),
    }
}

pub fn drop_cache(_f: &File) {
    // macOS has no per-file eviction call; `open_uncached` bypasses the cache instead.
}

pub fn open_uncached(path: &Path) -> io::Result<UncachedReader> {
    let f = open_source(path)?;
    rustix::fs::fcntl_nocache(&f, true)?;
    Ok(f)
}
