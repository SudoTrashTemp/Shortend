//! Filesystem layer: validated paths, source scanning, naming rules and per-OS primitives.

pub mod names;
pub mod platform;
pub mod relpath;
pub mod scan;

use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use relpath::RelPath;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsType {
    Ext4,
    Xfs,
    Btrfs,
    ExFat,
    Vfat,
    Ntfs,
    Apfs,
    HfsPlus,
    Smb,
    Nfs,
    Other(String),
}

impl fmt::Display for FsType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ext4 => "ext4",
            Self::Xfs => "XFS",
            Self::Btrfs => "Btrfs",
            Self::ExFat => "exFAT",
            Self::Vfat => "FAT",
            Self::Ntfs => "NTFS",
            Self::Apfs => "APFS",
            Self::HfsPlus => "HFS+",
            Self::Smb => "SMB",
            Self::Nfs => "NFS",
            Self::Other(name) => name,
        })
    }
}

impl FsType {
    /// Coarsest mtime resolution, used when comparing preserved timestamps.
    pub fn mtime_granularity(&self) -> std::time::Duration {
        match self {
            Self::Vfat => std::time::Duration::from_secs(2),
            // HFS+ stores whole seconds.
            Self::HfsPlus => std::time::Duration::from_secs(1),
            // NTFS stores 100 ns units.
            Self::Ntfs => std::time::Duration::from_nanos(100),
            // exFAT stores 10 ms increments; SMB/NFS servers vary. Be generous: a 2 s window
            // never confuses two different camera files, which also differ in size or content.
            Self::ExFat | Self::Smb | Self::Nfs | Self::Other(_) => {
                std::time::Duration::from_secs(2)
            }
            _ => std::time::Duration::ZERO,
        }
    }
}

/// Identity and properties of the volume holding a path, captured at preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// `st_dev` on Unix, volume serial number on Windows.
    pub dev: u64,
    pub fs_uuid: Option<String>,
    pub fs_type: FsType,
    pub case_sensitive: bool,
    pub block_size: u64,
    pub read_only: bool,
}

/// True when `path` exists as a real directory (not a symlink) on volume `dev`.
pub fn same_volume_dir(path: &Path, dev: u64) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            platform::volume_id(path).is_ok_and(|d| d == dev)
        }
        _ => false,
    }
}

/// Fails if `path` itself is a symlink; returns its canonical absolute form otherwise.
pub fn canonical_dir(path: &Path) -> io::Result<std::path::PathBuf> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(io::Error::other("is a symbolic link"));
    }
    if !meta.is_dir() {
        return Err(io::Error::other("is not a directory"));
    }
    std::fs::canonicalize(path).map(simplify)
}

/// On Windows, `canonicalize` returns verbatim paths (`\\?\C:\…`, `\\?\UNC\…`) that many
/// tools and people cannot read. Converts them back to the ordinary form; Rust's file APIs
/// still handle long paths themselves. A no-op elsewhere.
pub fn simplify(p: std::path::PathBuf) -> std::path::PathBuf {
    if !cfg!(windows) {
        return p;
    }
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => std::path::PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn verbatim_paths_are_simplified() {
        assert_eq!(
            super::simplify(PathBuf::from(r"\\?\C:\Media\A001")),
            PathBuf::from(r"C:\Media\A001")
        );
        assert_eq!(
            super::simplify(PathBuf::from(r"\\?\UNC\nas\share\x")),
            PathBuf::from(r"\\nas\share\x")
        );
        assert_eq!(
            super::simplify(PathBuf::from(r"C:\x")),
            PathBuf::from(r"C:\x")
        );
    }
}
