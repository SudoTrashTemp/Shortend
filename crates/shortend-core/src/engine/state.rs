//! Per-job state directory: `job.toml` (header), `snapshot.json` (scan), `journal.jsonl`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs::RelPath;
use crate::model::{FileEntry, Job};

pub const JOB_FILE: &str = "job.toml";
pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Root of all job state. `SHORTEND_STATE_DIR` overrides the per-OS default.
pub fn jobs_root() -> PathBuf {
    if let Some(p) = std::env::var_os("SHORTEND_STATE_DIR") {
        return PathBuf::from(p).join("jobs");
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let base = if cfg!(target_os = "macos") {
        home().join("Library/Application Support/ShortEnd")
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("ShortEnd")
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/state"))
            .join("shortend")
    };
    base.join("jobs")
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    files: Vec<FileEntry>,
    dirs: Vec<RelPath>,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| Error::io_at(&tmp, e))?;
    // A write handle: Windows refuses to flush a read-only one.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| Error::io_at(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io_at(path, e))
}

pub fn save(dir: &Path, job: &Job) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
    let header = toml::to_string_pretty(job).map_err(|e| Error::State {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;
    write_atomic(&dir.join(JOB_FILE), header.as_bytes())?;
    let snap = serde_json::to_vec(&Snapshot {
        files: job.files.clone(),
        dirs: job.dirs.clone(),
    })
    .map_err(|e| Error::State {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;
    write_atomic(&dir.join(SNAPSHOT_FILE), &snap)
}

pub fn load_header(dir: &Path) -> Result<Job> {
    let path = dir.join(JOB_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| Error::io_at(&path, e))?;
    toml::from_str(&text).map_err(|e| Error::State {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

pub fn load(dir: &Path) -> Result<Job> {
    let mut job = load_header(dir)?;
    let path = dir.join(SNAPSHOT_FILE);
    let bytes = std::fs::read(&path).map_err(|e| Error::io_at(&path, e))?;
    let snap: Snapshot = serde_json::from_slice(&bytes).map_err(|e| Error::State {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    job.files = snap.files;
    job.dirs = snap.dirs;
    Ok(job)
}

/// Finds a job directory by full ID or unique prefix.
pub fn find(id_or_prefix: &str) -> Result<PathBuf> {
    let root = jobs_root();
    let needle = id_or_prefix.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Err(Error::JobNotFound(id_or_prefix.to_owned()));
    }
    let mut hits = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&needle)
                && e.path().join(JOB_FILE).exists()
            {
                hits.push(e.path());
            }
        }
    }
    match hits.len() {
        0 => Err(Error::JobNotFound(id_or_prefix.to_owned())),
        1 => Ok(hits.pop().expect("one")),
        _ => Err(Error::AmbiguousJob(id_or_prefix.to_owned())),
    }
}
