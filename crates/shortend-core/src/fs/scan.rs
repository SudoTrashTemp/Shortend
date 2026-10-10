//! Source walk. Never follows links; anything that is not a regular file or directory is
//! reported so preflight can refuse the job (fail closed).

use std::path::Path;

use walkdir::WalkDir;

use super::RelPath;
use crate::model::FileEntry;

#[derive(Debug, Default)]
pub struct Scan {
    pub files: Vec<FileEntry>,
    pub dirs: Vec<RelPath>,
    pub problems: Vec<String>,
}

impl Scan {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

pub fn scan(root: &Path) -> Scan {
    let mut out = Scan::default();
    let walker = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by_file_name();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                out.problems.push(format!("cannot read {path}: {e}"));
                continue;
            }
        };
        let rel_os = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel = match RelPath::from_path(rel_os) {
            Ok(r) => r,
            Err(e) => {
                out.problems.push(e.to_string());
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() {
            out.problems.push(format!(
                "{rel}: symbolic links are not copied (refusing job)"
            ));
        } else if ft.is_dir() {
            out.dirs.push(rel);
        } else if ft.is_file() {
            match entry.metadata() {
                Ok(m) => match m.modified().map(jiff::Timestamp::try_from) {
                    Ok(Ok(mtime)) => out.files.push(FileEntry {
                        rel,
                        size: m.len(),
                        mtime,
                    }),
                    _ => out
                        .problems
                        .push(format!("{rel}: unreadable modification time")),
                },
                Err(e) => out.problems.push(format!("{rel}: {e}")),
            }
        } else {
            out.problems.push(format!(
                "{rel}: special file (FIFO, socket or device) is not copied"
            ));
        }
    }
    out
}
