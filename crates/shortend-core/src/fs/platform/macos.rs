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
    let mount_bytes: Vec<u8> = sfs
        .f_mntonname
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    let mount_point = String::from_utf8_lossy(&mount_bytes);
    Ok(VolumeProbe {
        dev: meta.dev(),
        fs_uuid: volume_uuid(&mount_point),
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

/// The volume UUID macOS reports for the volume mounted at `mount_point` (APFS, HFS+, and a
/// UUID derived from the serial number for exFAT/FAT). Unlike `st_dev`, it survives the drive
/// being unplugged and reconnected. Network shares have none.
///
/// Asked through `diskutil` (always present on macOS), which avoids `getattrlist` FFI. Never
/// cached: after a reconnect the same device number can belong to a different drive.
fn volume_uuid(mount_point: &str) -> Option<String> {
    if mount_point.is_empty() {
        return None;
    }
    let out = std::process::Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", mount_point])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    plist_string(&String::from_utf8_lossy(&out.stdout), "VolumeUUID")
}

/// The `<string>` value following `<key>{key}</key>` in an XML property list.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = plist
        .split_once(&format!("<key>{key}</key>"))?
        .1
        .trim_start();
    let value = after
        .strip_prefix("<string>")?
        .split_once("</string>")?
        .0
        .trim();
    (!value.is_empty()).then(|| value.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    #[test]
    fn plist_parsing() {
        let p = "<dict>\n\t<key>VolumeName</key>\n\t<string>CARD</string>\n\t<key>VolumeUUID</key>\n\t<string>1a2b3c4d-0000-1111-2222-333344445555</string>\n</dict>";
        assert_eq!(
            super::plist_string(p, "VolumeUUID").as_deref(),
            Some("1A2B3C4D-0000-1111-2222-333344445555")
        );
        assert_eq!(super::plist_string(p, "DiskUUID"), None);
        assert_eq!(
            super::plist_string("<key>VolumeUUID</key><string></string>", "VolumeUUID"),
            None
        );
    }

    #[test]
    fn the_system_volume_has_a_uuid() {
        let probe = super::volume_probe(&std::env::temp_dir()).unwrap();
        assert!(probe.fs_uuid.is_some(), "{probe:?}");
    }
}
