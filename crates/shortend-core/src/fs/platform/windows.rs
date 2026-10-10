//! Windows primitives via kernel32 (windows-sys). Each FFI call is small, takes buffers we own,
//! and has its return value checked.
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_OPEN_REPARSE_POINT, GetDiskFreeSpaceExW, GetDiskFreeSpaceW,
    GetFileInformationByHandle, GetVolumeInformationByHandleW, GetVolumePathNameW,
    MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

use super::VolumeProbe;
use crate::fs::FsType;

const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// `\\?\C:\dir` → `C:\dir`. The volume-information calls are documented for ordinary
/// paths, and canonicalised paths carry the verbatim prefix; UNC paths are left as they are.
fn plain(p: &Path) -> std::path::PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => std::path::PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Opens a file or directory for metadata queries without following reparse points.
fn open_handle(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub fn volume_id(path: &Path) -> io::Result<u64> {
    let f = open_handle(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid handle for the lifetime of `f`; `info` is a properly sized out-param.
    check(unsafe { GetFileInformationByHandle(f.as_raw_handle() as HANDLE, &mut info) })?;
    Ok(u64::from(info.dwVolumeSerialNumber))
}

pub fn volume_probe(path: &Path) -> io::Result<VolumeProbe> {
    let f = open_handle(path)?;
    let mut serial = 0u32;
    let mut max_component = 0u32;
    let mut flags = 0u32;
    let mut fs_name = [0u16; 64];
    // SAFETY: all buffers are owned locals with their true lengths passed.
    check(unsafe {
        GetVolumeInformationByHandleW(
            f.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    })?;
    let end = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..end]);
    let fs_type = match name.as_str() {
        "NTFS" => FsType::Ntfs,
        "exFAT" => FsType::ExFat,
        "FAT32" | "FAT" => FsType::Vfat,
        _ => FsType::Other(name),
    };

    let mut root = [0u16; 1024];
    let wpath = wide(&plain(path));
    let mut block_size = 4096u64;
    // SAFETY: `wpath` is NUL-terminated; `root` length is passed.
    if unsafe { GetVolumePathNameW(wpath.as_ptr(), root.as_mut_ptr(), root.len() as u32) } != 0 {
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: `root` was NUL-terminated by GetVolumePathNameW.
        if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) }
            != 0
        {
            block_size = u64::from(spc) * u64::from(bps);
        }
    }

    Ok(VolumeProbe {
        dev: u64::from(serial),
        fs_uuid: Some(format!("{serial:08X}")),
        fs_type,
        block_size: block_size.max(512),
        read_only: flags & FILE_READ_ONLY_VOLUME != 0,
    })
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let w = wide(&plain(path));
    let mut avail = 0u64;
    // SAFETY: `w` is NUL-terminated; optional out-params are null.
    check(unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    })?;
    Ok(avail)
}

pub fn open_source(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub fn create_staged(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub fn sync_file(f: &File) -> io::Result<()> {
    f.sync_all() // FlushFileBuffers
}

pub fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(()) // NTFS journals directory entries; there is no directory flush call.
}

pub fn drop_cache(_f: &File) {
    // No per-file eviction on Windows; `open_uncached` uses unbuffered I/O instead.
}

/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` fails if the target exists.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (a, b) = (wide(from), wide(to));
    // SAFETY: both paths are NUL-terminated wide strings.
    check(unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), MOVEFILE_WRITE_THROUGH) })
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

const PAGES: usize = 256; // 1 MiB per unbuffered read

/// Reads with `FILE_FLAG_NO_BUFFERING`, which requires sector-aligned buffers, sizes and
/// offsets. Every request is a whole aligned buffer; only the final read at EOF is short.
pub struct UncachedReader {
    file: File,
    buf: Vec<Page>,
    pos: usize,
    len: usize,
    eof: bool,
}

pub fn open_uncached(path: &Path) -> io::Result<UncachedReader> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Ok(UncachedReader {
        file,
        buf: vec![Page([0; 4096]); PAGES],
        pos: 0,
        len: 0,
        eof: false,
    })
}

impl Read for UncachedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.len {
            if self.eof {
                return Ok(0);
            }
            let cap = self.buf.len() * 4096;
            // SAFETY: `Page` is plain bytes with no padding; the Vec is contiguous, so its
            // storage is exactly `cap` initialised, 4096-aligned bytes.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<u8>(), cap) };
            let n = self.file.read(bytes)?;
            self.pos = 0;
            self.len = n;
            // After a short read the offset is unaligned; the next read would fail, and in
            // unbuffered mode a short read only happens at end of file anyway.
            self.eof = n < cap;
            if n == 0 {
                return Ok(0);
            }
        }
        let k = out.len().min(self.len - self.pos);
        let start = self.pos;
        // SAFETY: same contiguous storage as above; `start + k <= self.len <= cap`.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), self.buf.len() * 4096)
        };
        out[..k].copy_from_slice(&bytes[start..start + k]);
        self.pos += k;
        Ok(k)
    }
}
