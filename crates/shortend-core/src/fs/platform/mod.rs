//! Per-OS filesystem primitives. Exactly one implementation is compiled per target, so these
//! are plain functions rather than a trait.
//!
//! Every module exports the same items:
//! - `volume_probe(path)`   → device id, fs type, uuid, block size, read-only flag
//! - `free_space(path)`     → bytes available to this user
//! - `volume_id(path)`      → cheap device id for revalidation before writes
//! - `open_source(path)`    → read-only, no symlink following, no atime update where possible
//! - `create_staged(path)`  → new file, fails if it exists, no symlink following
//! - `sync_file(file)`      → durable data + metadata (F_FULLFSYNC on macOS)
//! - `sync_dir(path)`       → durable directory entries (no-op on Windows)
//! - `drop_cache(file)`     → evict clean cached pages so a later read hits the device
//! - `open_uncached(path)`  → reader whose bytes come from the device, not the page cache
//! - `rename_noreplace(a,b)`→ atomic rename that never overwrites an existing `b`

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("ShortEnd supports Linux, macOS and Windows");

use super::FsType;

/// What `volume_probe` can learn without writing anything.
#[derive(Debug, Clone)]
pub struct VolumeProbe {
    pub dev: u64,
    pub fs_uuid: Option<String>,
    pub fs_type: FsType,
    pub block_size: u64,
    pub read_only: bool,
}

/// Probes case sensitivity by creating a file in `dir` (which must be writable and owned by
/// the job) and looking it up with different case.
pub fn probe_case_sensitive(dir: &std::path::Path) -> std::io::Result<bool> {
    let name = format!("ShortEnd-CaseProbe-{}", std::process::id());
    let upper = dir.join(&name);
    let lower = dir.join(name.to_lowercase());
    let _ = std::fs::remove_file(&upper);
    drop(create_staged(&upper)?);
    let sensitive = match std::fs::symlink_metadata(&lower) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            let _ = std::fs::remove_file(&upper);
            return Err(e);
        }
    };
    std::fs::remove_file(&upper)?;
    Ok(sensitive)
}
