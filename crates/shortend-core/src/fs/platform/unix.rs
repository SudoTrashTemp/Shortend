//! Pieces shared by Linux and macOS.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::OFlags;

pub fn volume_id(path: &Path) -> io::Result<u64> {
    Ok(std::fs::symlink_metadata(path)?.dev())
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let st = rustix::fs::statvfs(path)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

pub fn create_staged(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::CLOEXEC).bits() as i32)
        .open(path)
}

pub fn open_nofollow(path: &Path, extra: OFlags) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::CLOEXEC | extra).bits() as i32)
        .open(path)
}

pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// `RENAME_NOREPLACE`/`RENAME_EXCL` rename, with a check-then-rename fallback for
/// filesystems that reject the flag (exFAT, FUSE ntfs-3g, some network filesystems).
/// The fallback's race window is limited to a target folder owned by the running job.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(e)
            if e == rustix::io::Errno::INVAL
                || e == rustix::io::Errno::NOSYS
                || e == rustix::io::Errno::NOTSUP =>
        {
            match std::fs::symlink_metadata(to) {
                Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::rename(from, to),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e.into()),
    }
}
